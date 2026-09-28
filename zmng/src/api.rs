//! HTTP API + static UI.  Every endpoint is O(rows returned): no per-frame
//! tables, no on-the-fly `du`, no ffmpeg per page view.

use anyhow::Result;
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tower_http::services::{ServeDir, ServeFile};
use tracing::info;

use crate::config::Config;
use crate::db::{Db, User};
use crate::mp4::TIMESCALE;
use crate::recorder::LiveHub;
use crate::video;

#[derive(Clone)]
pub struct App {
    pub cfg: Arc<Config>,
    pub db: Db,
    pub hub: Arc<LiveHub>,
    /// One-time token printed at startup; required to create the first admin
    /// so that any LAN host cannot claim a fresh install.
    pub setup_token: Arc<String>,
    pub login_guard: Arc<parking_lot::Mutex<LoginGuard>>,
    /// Bounds concurrent on-demand ffmpeg decodes (snapshot/frame endpoints).
    pub decode_sem: Arc<tokio::sync::Semaphore>,
}

/// Global brute-force limiter: after `MAX_FAILS` failed logins within a
/// minute, every login is refused for a minute.
#[derive(Default)]
pub struct LoginGuard {
    fails: Vec<std::time::Instant>,
}
const MAX_FAILS: usize = 10;
impl LoginGuard {
    pub fn allowed(&mut self) -> bool {
        let cutoff = std::time::Instant::now() - std::time::Duration::from_secs(60);
        self.fails.retain(|t| *t > cutoff);
        self.fails.len() < MAX_FAILS
    }
    pub fn record_failure(&mut self) {
        self.fails.push(std::time::Instant::now());
    }
}

const COOKIE: &str = "zmng_session";

#[derive(Clone)]
pub struct AuthUser(pub User);

pub fn hash_password(pw: &str) -> Result<String> {
    use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
    let salt = SaltString::generate(&mut OsRng);
    let hash = argon2::Argon2::default()
        .hash_password(pw.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("hash: {e}"))?;
    Ok(hash.to_string())
}

pub fn verify_password_pub(pw: &str, hash: &str) -> bool {
    verify_password(pw, hash)
}
pub fn hash_password_dummy() {
    let _ = hash_password("x");
}
pub fn visible_cameras_pub(app: &App, u: &User) -> Vec<i64> {
    visible_cameras(app, u)
}
pub fn parse_range_pub(v: &str) -> Option<(u64, Option<u64>)> {
    parse_range(v)
}
/// JPEG of the keyframe nearest `dts` (recorded) or the latest live keyframe.
pub async fn frame_jpeg_pub(app: &App, cam: i64, dts: i64, width: u32) -> Result<Response, Response> {
    let now = crate::db::now_dts();
    let (init, data) = if dts >= now - 5 * crate::mp4::TIMESCALE as i64 {
        let h = app.hub.get(cam).ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "camera not running").into_response())?;
        let r = h.recent.read();
        let r = r.as_ref().ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "no frames yet").into_response())?;
        let f = r.frags.back().ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "no frames yet").into_response())?;
        (r.init.to_vec(), f.2.to_vec())
    } else {
        let segs = app.db.segments_in_range(cam, dts, dts + 1).map_err(err500)?;
        let seg = segs.first().ok_or_else(|| (StatusCode::NOT_FOUND, "no recording at that time").into_response())?;
        let frag = seg.index.iter().find(|f| f.dts <= dts && dts < f.dts + f.duration as i64).or_else(|| seg.index.first()).copied().ok_or_else(|| err500("empty segment"))?;
        let storage = app.db.storage(seg.storage_id).map_err(err500)?.ok_or_else(|| err500("storage missing"))?;
        let path = std::path::PathBuf::from(&storage.path).join(&seg.path);
        let (init_len, off) = (seg.init_len, seg.dts_offset);
        tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, Vec<u8>)> {
            let d = crate::thumbs::read_range(&path, frag.offset, frag.len)?;
            Ok((crate::thumbs::read_range(&path, 0, init_len)?, crate::mp4::rebase_fragment(&d, off)))
        })
        .await
        .map_err(err500)?
        .map_err(err500)?
    };
    let _permit = app.decode_sem.acquire().await.map_err(err500)?;
    let jpeg = crate::thumbs::frag_to_jpeg(&app.cfg.ffmpeg, &init, &data, width.min(3840), 5).await.map_err(err500)?;
    Ok(([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "private, max-age=5")], jpeg).into_response())
}

fn verify_password(pw: &str, hash: &str) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    PasswordHash::new(hash)
        .map(|h| argon2::Argon2::default().verify_password(pw.as_bytes(), &h).is_ok())
        .unwrap_or(false)
}

fn token_from_query(uri: &axum::http::Uri) -> Option<String> {
    let q = uri.query()?;
    url::form_urlencoded::parse(q.as_bytes()).find(|(k, _)| k == "token").map(|(_, v)| v.into_owned()).filter(|v| !v.is_empty())
}

fn token_from(headers: &HeaderMap) -> Option<String> {
    if let Some(a) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if let Some(t) = a.strip_prefix("Bearer ") {
            return Some(t.to_string());
        }
    }
    let cookies = headers.get_all(header::COOKIE);
    for c in cookies.iter().filter_map(|v| v.to_str().ok()) {
        for part in c.split(';') {
            let part = part.trim();
            if let Some(v) = part.strip_prefix(&format!("{COOKIE}=")) {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Auth middleware: sets `AuthUser` extension or returns 401.  When no users
/// exist yet (fresh install) every request is treated as admin so the UI can
/// create the first account.
async fn auth_mw(State(app): State<App>, mut req: Request<Body>, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let public = path == "/api/login" || path == "/api/setup" || path == "/api/health"
        || path == "/zm/api/host/getVersion.json" || path == "/zm/api/host/login.json"
        || !(path.starts_with("/api/") || path.starts_with("/zm/"));
    let user = match token_from(req.headers()).or_else(|| token_from_query(req.uri())) {
        Some(t) => app.db.session_user(&t).ok().flatten(),
        None => None,
    };
    let user = match user {
        Some(u) => Some(u),
        None if app.db.user_count().unwrap_or(1) == 0 => Some(User {
            id: 0,
            username: "setup".into(),
            role: "admin".into(),
            pass_hash: String::new(),
        }),
        None => None,
    };
    match user {
        Some(u) => {
            req.extensions_mut().insert(AuthUser(u));
            next.run(req).await
        }
        None if public => next.run(req).await,
        None => (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error":"unauthorized"}))).into_response(),
    }
}

fn require(req_user: Option<&AuthUser>) -> Result<&User, Response> {
    req_user.map(|u| &u.0).ok_or_else(|| (StatusCode::UNAUTHORIZED, "unauthorized").into_response())
}

fn require_admin(req_user: Option<&AuthUser>) -> Result<&User, Response> {
    let u = require(req_user)?;
    if u.role != "admin" {
        return Err((StatusCode::FORBIDDEN, "admin only").into_response());
    }
    Ok(u)
}

fn visible_cameras(app: &App, u: &User) -> Vec<i64> {
    if u.role == "admin" {
        app.db.cameras().map(|cs| cs.iter().map(|c| c.id).collect()).unwrap_or_default()
    } else {
        app.db.user_cameras(u.id).unwrap_or_default()
    }
}

fn can_see(app: &App, u: &User, cam: i64) -> Result<(), Response> {
    if visible_cameras(app, u).contains(&cam) {
        Ok(())
    } else {
        Err((StatusCode::NOT_FOUND, "no such camera").into_response())
    }
}

type ApiResult = Result<Response, Response>;

fn err500(e: impl std::fmt::Display) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response()
}
fn bad(e: impl std::fmt::Display) -> Response {
    (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": e.to_string()}))).into_response()
}

// ---------------------------------------------------------------------------
// auth endpoints
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct LoginReq {
    username: String,
    password: String,
}

async fn login(State(app): State<App>, Json(req): Json<LoginReq>) -> ApiResult {
    if !app.login_guard.lock().allowed() {
        return Err((StatusCode::TOO_MANY_REQUESTS, Json(serde_json::json!({"error":"too many failed logins; try again in a minute"}))).into_response());
    }
    let user = app.db.user_by_name(&req.username).map_err(err500)?;
    let ok = user.as_ref().map(|u| verify_password(&req.password, &u.pass_hash)).unwrap_or(false);
    if !ok {
        // constant-ish time: hash anyway
        let _ = hash_password("x");
        app.login_guard.lock().record_failure();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        return Err((StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error":"bad credentials"}))).into_response());
    }
    let user = user.unwrap();
    let token = new_token();
    let ttl = app.cfg.session_hours as i64 * 3600;
    app.db.create_session(user.id, &token, ttl).map_err(err500)?;
    info!(user = %user.username, "login");
    let cookie = format!(
        "{COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={ttl}{}",
        if app.cfg.secure_cookies { "; Secure" } else { "" }
    );
    let mut resp = Json(serde_json::json!({"token": token, "user": user})).into_response();
    resp.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).unwrap());
    Ok(resp)
}

async fn logout(State(app): State<App>, headers: HeaderMap) -> ApiResult {
    if let Some(t) = token_from(&headers) {
        let _ = app.db.delete_session(&t);
    }
    let mut resp = Json(serde_json::json!({"ok": true})).into_response();
    resp.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_str(&format!("{COOKIE}=; Path=/; Max-Age=0")).unwrap());
    Ok(resp)
}

#[derive(Deserialize)]
struct SetupReq {
    username: String,
    password: String,
    setup_token: String,
}

/// Create the first admin (only allowed while there are no users, and only
/// with the token printed in the server log at startup).
async fn setup(State(app): State<App>, Json(req): Json<SetupReq>) -> ApiResult {
    if app.db.user_count().map_err(err500)? > 0 {
        return Err((StatusCode::FORBIDDEN, "already set up").into_response());
    }
    if req.setup_token.trim() != app.setup_token.as_str() {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        return Err((StatusCode::FORBIDDEN, Json(serde_json::json!({"error":"wrong setup token (see server log)"}))).into_response());
    }
    if req.username.trim().is_empty() || req.password.len() < 8 {
        return Err(bad("username required, password >= 8 chars"));
    }
    let hash = hash_password(&req.password).map_err(err500)?;
    if !app.db.add_first_admin(req.username.trim(), &hash).map_err(err500)? {
        return Err((StatusCode::FORBIDDEN, "already set up").into_response());
    }
    Ok(Json(serde_json::json!({"ok": true})).into_response())
}

async fn me(State(app): State<App>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    Ok(Json(serde_json::json!({
        "user": u,
        "cameras": visible_cameras(&app, u),
        "setup_needed": app.db.user_count().unwrap_or(0) == 0,
        "go2rtc_url": app.cfg.go2rtc_url,
    }))
    .into_response())
}

pub fn new_token() -> String {
    let bytes: [u8; 32] = rand::random();
    hex::encode(bytes)
}

// ---------------------------------------------------------------------------
// cameras
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct CameraOut {
    #[serde(flatten)]
    camera: crate::db::Camera,
    status: crate::recorder::CamStatus,
    sub_status: Option<crate::recorder::CamStatus>,
    detect: Option<crate::detect::DetectStatus>,
}

fn camera_out(app: &App, c: crate::db::Camera, admin: bool) -> CameraOut {
    let status = app.hub.get(c.id).map(|h| h.status.read().clone()).unwrap_or_default();
    let sub_status = app.hub.get_stream(c.id, crate::recorder::StreamKind::Sub).map(|h| h.status.read().clone());
    let detect = crate::detect::status(c.id);
    let mut c = c;
    if !admin || app.db.user_count().unwrap_or(1) == 0 {
        // never leak RTSP credentials to viewers
        c.main_url = String::new();
        c.sub_url = None;
    }
    CameraOut { camera: c, status, sub_status, detect }
}

async fn cameras(State(app): State<App>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let vis = visible_cameras(&app, u);
    let admin = u.role == "admin";
    let out: Vec<CameraOut> = app
        .db
        .cameras()
        .map_err(err500)?
        .into_iter()
        .filter(|c| vis.contains(&c.id))
        .map(|c| camera_out(&app, c, admin))
        .collect();
    Ok(Json(out).into_response())
}

async fn camera_get(State(app): State<App>, Path(id): Path<i64>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    can_see(&app, u, id)?;
    let c = app.db.camera(id).map_err(err500)?.ok_or_else(|| (StatusCode::NOT_FOUND, "no such camera").into_response())?;
    Ok(Json(camera_out(&app, c, u.role == "admin")).into_response())
}

#[derive(Deserialize)]
struct CameraCreate {
    name: String,
    main_url: String,
    sub_url: Option<String>,
    storage_id: Option<i64>,
}

async fn camera_create(State(app): State<App>, user: Option<axum::Extension<AuthUser>>, Json(req): Json<CameraCreate>) -> ApiResult {
    require_admin(user.as_ref().map(|e| &e.0))?;
    let storage = match req.storage_id {
        Some(s) => s,
        None => app.db.storages().map_err(err500)?.first().map(|s| s.id).ok_or_else(|| bad("no storage configured"))?,
    };
    let id = app.db.add_camera(&req.name, &req.main_url, req.sub_url.as_deref(), storage).map_err(err500)?;
    Ok(Json(serde_json::json!({"id": id})).into_response())
}

async fn camera_update(
    State(app): State<App>,
    Path(id): Path<i64>,
    user: Option<axum::Extension<AuthUser>>,
    Json(patch): Json<serde_json::Map<String, serde_json::Value>>,
) -> ApiResult {
    require_admin(user.as_ref().map(|e| &e.0))?;
    app.db.update_camera(id, &patch).map_err(bad)?;
    Ok(Json(serde_json::json!({"ok": true})).into_response())
}

async fn camera_delete(State(app): State<App>, Path(id): Path<i64>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    require_admin(user.as_ref().map(|e| &e.0))?;
    // stop recording first so no new files appear while we delete
    app.db.update_camera(id, &serde_json::json!({"enabled": false}).as_object().unwrap().clone()).map_err(bad)?;
    app.hub.remove_camera(id);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let (db, td) = (app.db.clone(), app.cfg.thumb_dir.clone());
    tokio::task::spawn_blocking(move || crate::retention::delete_camera_files(&db, &td, id)).await.map_err(err500)?.map_err(err500)?;
    app.db.delete_camera(id).map_err(err500)?;
    Ok(Json(serde_json::json!({"ok": true})).into_response())
}

// ---------------------------------------------------------------------------
// timeline / events
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RangeQ {
    /// epoch milliseconds
    start: i64,
    end: i64,
    /// bucket size in seconds for the motion histogram (default: range/600)
    bucket: Option<f64>,
}

#[derive(Serialize)]
struct TimelineOut {
    coverage: Vec<[i64; 2]>, // ms intervals with recording
    bucket_ms: i64,
    /// max motion score per bucket (0..255), aligned to `start`
    motion: Vec<u8>,
    events: Vec<EventOut>,
}

#[derive(Serialize)]
struct EventOut {
    id: i64,
    camera_id: i64,
    start: i64,
    end: Option<i64>,
    peak: Option<i64>,
    kind: String,
    score: u8,
    thumb: Option<String>,
    archived: bool,
    notes: Option<String>,
}

fn event_out(e: crate::db::Event) -> EventOut {
    EventOut {
        id: e.id,
        camera_id: e.camera_id,
        start: video::dts_to_ms(e.start_dts),
        end: e.end_dts.map(video::dts_to_ms),
        peak: e.peak_dts.map(video::dts_to_ms),
        kind: e.kind,
        score: e.score,
        thumb: e.thumb_path.map(|_| format!("/api/events/{}/thumb.jpg", e.id)),
        archived: e.archived,
        notes: e.notes,
    }
}

async fn timeline(State(app): State<App>, Path(id): Path<i64>, Query(q): Query<RangeQ>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    can_see(&app, u, id)?;
    if q.end <= q.start || q.end - q.start > 8 * 86_400_000 {
        return Err(bad("range must be 0 < end-start <= 8 days"));
    }
    let (s, e) = (video::ms_to_dts(q.start), video::ms_to_dts(q.end));
    let segs = app.db.segments_in_range(id, s, e).map_err(err500)?;
    // coverage: merge adjacent segments
    let mut coverage: Vec<[i64; 2]> = Vec::new();
    for seg in &segs {
        let (a, b) = (video::dts_to_ms(seg.start_dts.max(s)), video::dts_to_ms(seg.end_dts.min(e)));
        if let Some(last) = coverage.last_mut() {
            if a - last[1] < 1500 {
                last[1] = b;
                continue;
            }
        }
        coverage.push([a, b]);
    }
    // motion histogram
    let bucket_ms = q.bucket.map(|b| (b * 1000.0) as i64).unwrap_or((q.end - q.start) / 600).max(1000);
    let n = ((q.end - q.start) / bucket_ms + 1) as usize;
    let mut motion = vec![0u8; n.min(20_000)];
    for seg in &segs {
        for f in &seg.index {
            if f.motion == 0 {
                continue;
            }
            let ms = video::dts_to_ms(f.dts);
            if ms < q.start || ms >= q.end {
                continue;
            }
            let i = ((ms - q.start) / bucket_ms) as usize;
            if i < motion.len() {
                motion[i] = motion[i].max(f.motion);
            }
        }
    }
    let events = app.db.events_in_range(id, s, e).map_err(err500)?.into_iter().map(event_out).collect();
    Ok(Json(TimelineOut { coverage, bucket_ms, motion, events }).into_response())
}

#[derive(Deserialize)]
struct EventsQ {
    camera: Option<String>, // comma separated
    start: Option<i64>,
    end: Option<i64>,
    min_score: Option<u8>,
    archived: Option<bool>,
    before: Option<i64>,
    limit: Option<usize>,
    order: Option<String>,
}

async fn events(State(app): State<App>, Query(q): Query<EventsQ>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let vis = visible_cameras(&app, u);
    let cams: Vec<i64> = match &q.camera {
        Some(s) if !s.is_empty() => s.split(',').filter_map(|x| x.trim().parse().ok()).filter(|c| vis.contains(c)).collect(),
        _ => vis.clone(),
    };
    let list = app
        .db
        .events(
            Some(&cams),
            q.start.map(video::ms_to_dts),
            q.end.map(video::ms_to_dts),
            q.min_score.unwrap_or(0),
            q.archived.unwrap_or(false),
            q.before,
            q.limit.unwrap_or(50).min(500),
            q.order.as_deref() == Some("asc"),
        )
        .map_err(err500)?;
    let out: Vec<EventOut> = list.into_iter().map(event_out).collect();
    Ok(Json(out).into_response())
}

async fn event_get(State(app): State<App>, Path(id): Path<i64>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let e = app.db.event(id).map_err(err500)?.ok_or_else(|| (StatusCode::NOT_FOUND, "no such event").into_response())?;
    can_see(&app, u, e.camera_id)?;
    Ok(Json(event_out(e)).into_response())
}

#[derive(Deserialize)]
struct EventPatch {
    archived: Option<bool>,
    notes: Option<String>,
}

async fn event_update(State(app): State<App>, Path(id): Path<i64>, user: Option<axum::Extension<AuthUser>>, Json(p): Json<EventPatch>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let e = app.db.event(id).map_err(err500)?.ok_or_else(|| (StatusCode::NOT_FOUND, "no such event").into_response())?;
    can_see(&app, u, e.camera_id)?;
    if let Some(a) = p.archived {
        app.db.set_event_archived(id, a).map_err(err500)?;
    }
    if let Some(n) = p.notes {
        app.db.set_event_notes(id, &n).map_err(err500)?;
    }
    Ok(Json(serde_json::json!({"ok": true})).into_response())
}

async fn event_thumb(State(app): State<App>, Path(id): Path<i64>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let e = app.db.event(id).map_err(err500)?.ok_or_else(|| (StatusCode::NOT_FOUND, "no such event").into_response())?;
    can_see(&app, u, e.camera_id)?;
    let Some(rel) = e.thumb_path else {
        return Err((StatusCode::NOT_FOUND, "no thumbnail yet").into_response());
    };
    let data = tokio::fs::read(app.cfg.thumb_dir.join(rel)).await.map_err(|_| (StatusCode::NOT_FOUND, "thumb missing").into_response())?;
    Ok((
        [(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "private, max-age=31536000, immutable")],
        data,
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// video
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct VideoQ {
    start: i64,
    end: i64,
    /// "main" (default) or "sub"
    stream: Option<String>,
}

fn stream_of(q: &Option<String>) -> Result<crate::recorder::StreamKind, Response> {
    match q.as_deref() {
        None | Some("main") => Ok(crate::recorder::StreamKind::Main),
        Some("sub") => Ok(crate::recorder::StreamKind::Sub),
        _ => Err(bad("stream must be main or sub")),
    }
}

async fn video_range(State(app): State<App>, Path(id): Path<i64>, Query(q): Query<VideoQ>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    can_see(&app, u, id)?;
    if q.end <= q.start || q.end - q.start > 6 * 3600 * 1000 {
        return Err(bad("range must be 0 < end-start <= 6h"));
    }
    let kind = stream_of(&q.stream)?;
    let plan = video::plan_range_stream(&app.db, id, kind.as_str(), video::ms_to_dts(q.start), video::ms_to_dts(q.end)).map_err(err500)?;
    if plan.items.is_empty() {
        return Err((StatusCode::NOT_FOUND, "no recording in range").into_response());
    }
    let mime = plan.mime.clone().unwrap_or_else(|| "video/mp4".into());
    let first = plan.first_dts.map(video::dts_to_ms).unwrap_or(q.start);
    let len = plan.bytes;
    let exact = plan.exact_len;
    let body = Body::from_stream(video::stream_plan(plan));
    let mut b = Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, "private, max-age=3600")
        .header(header::ACCEPT_RANGES, "none")
        .header("X-First-Dts-Ms", first.to_string());
    if exact {
        b = b.header(header::CONTENT_LENGTH, len);
    }
    Ok(b.body(body).unwrap())
}

async fn video_hls(State(app): State<App>, Path(id): Path<i64>, Query(q): Query<VideoQ>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    can_see(&app, u, id)?;
    if q.end <= q.start || q.end - q.start > 24 * 3600 * 1000 {
        return Err(bad("range must be 0 < end-start <= 24h"));
    }
    let kind = stream_of(&q.stream)?;
    let m3u8 = video::hls_playlist(
        &app.db,
        id,
        kind.as_str(),
        video::ms_to_dts(q.start),
        video::ms_to_dts(q.end),
        |sid| format!("/api/segments/{sid}/file.mp4"),
        |sid, fi| format!("/api/segments/{sid}/frag/{fi}/seg.m4s"),
        |sid| format!("/api/segments/{sid}/init.mp4"),
    )
    .map_err(err500)?;
    Ok(([(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")], m3u8).into_response())
}

/// Raw segment file with HTTP Range support (used by HLS byte-range playlists).
async fn segment_file(State(app): State<App>, Path(id): Path<i64>, headers: HeaderMap, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let seg = app.db.segment(id).map_err(err500)?.ok_or_else(|| (StatusCode::NOT_FOUND, "no such segment").into_response())?;
    can_see(&app, u, seg.camera_id)?;
    let storage = app.db.storage(seg.storage_id).map_err(err500)?.ok_or_else(|| err500("storage missing"))?;
    let path = std::path::PathBuf::from(&storage.path).join(&seg.path);
    let total = seg.bytes as u64;
    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok()).and_then(parse_range);
    let (start, end) = match range {
        Some((s, e)) => (s, e.map(|e| e.min(total - 1)).unwrap_or(total - 1)),
        None => (0, total - 1),
    };
    if start > end || start >= total {
        return Err((StatusCode::RANGE_NOT_SATISFIABLE, "bad range").into_response());
    }
    let len = end - start + 1;
    let data = tokio::task::spawn_blocking(move || crate::thumbs::read_range(&path, start, len as u32))
        .await
        .map_err(err500)?
        .map_err(err500)?;
    let mut resp = Response::builder()
        .status(if range.is_some() { StatusCode::PARTIAL_CONTENT } else { StatusCode::OK })
        .header(header::CONTENT_TYPE, "video/mp4")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, len)
        .header(header::CACHE_CONTROL, "private, max-age=31536000, immutable");
    if range.is_some() {
        resp = resp.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{total}"));
    }
    Ok(resp.body(Body::from(data)).unwrap())
}

/// Init segment of one segment (for legacy files whose fragments are served
/// individually with patched timestamps).
async fn segment_init(State(app): State<App>, Path(id): Path<i64>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let seg = app.db.segment(id).map_err(err500)?.ok_or_else(|| (StatusCode::NOT_FOUND, "no such segment").into_response())?;
    can_see(&app, u, seg.camera_id)?;
    let se = app.db.sample_entry(seg.sample_entry_id).map_err(err500)?.ok_or_else(|| err500("sample entry missing"))?;
    let vp = crate::mp4::VideoParams {
        codec: crate::mp4::Codec::parse(&se.codec).unwrap_or(crate::mp4::Codec::H264),
        width: se.width,
        height: se.height,
        sample_entry: bytes::Bytes::from(se.data),
        rfc6381: se.rfc6381,
    };
    let init = crate::mp4::init_segment(&vp);
    Ok(([(header::CONTENT_TYPE, "video/mp4"), (header::CACHE_CONTROL, "private, max-age=31536000, immutable")], init).into_response())
}

/// One fragment of a segment with `tfdt` shifted to absolute time.
async fn segment_frag(State(app): State<App>, Path((id, fi)): Path<(i64, usize)>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let seg = app.db.segment(id).map_err(err500)?.ok_or_else(|| (StatusCode::NOT_FOUND, "no such segment").into_response())?;
    can_see(&app, u, seg.camera_id)?;
    let frag = *seg.index.get(fi).ok_or_else(|| (StatusCode::NOT_FOUND, "no such fragment").into_response())?;
    let storage = app.db.storage(seg.storage_id).map_err(err500)?.ok_or_else(|| err500("storage missing"))?;
    let path = std::path::PathBuf::from(&storage.path).join(&seg.path);
    let off = seg.dts_offset;
    let data = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let d = crate::thumbs::read_range(&path, frag.offset, frag.len)?;
        Ok(crate::mp4::rebase_fragment(&d, off))
    })
    .await
    .map_err(err500)?
    .map_err(err500)?;
    Ok(([(header::CONTENT_TYPE, "video/iso.segment"), (header::CACHE_CONTROL, "private, max-age=31536000, immutable")], data).into_response())
}

fn parse_range(v: &str) -> Option<(u64, Option<u64>)> {
    let v = v.strip_prefix("bytes=")?;
    let (a, b) = v.split_once('-')?;
    let start = a.parse().ok()?;
    let end = if b.is_empty() { None } else { Some(b.parse().ok()?) };
    Some((start, end))
}

#[derive(Deserialize)]
struct StreamQ {
    stream: Option<String>,
}

async fn live(State(app): State<App>, Path(id): Path<i64>, Query(q): Query<StreamQ>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    can_see(&app, u, id)?;
    let kind = stream_of(&q.stream)?;
    let h = app.hub.get_stream(id, kind).ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "stream not running").into_response())?;
    let (mime, first_ms) = {
        let r = h.recent.read();
        (
            r.as_ref().map(|r| r.params.mime()).unwrap_or_else(|| "video/mp4".into()),
            r.as_ref().and_then(|r| r.frags.back().map(|f| video::dts_to_ms(f.0))).unwrap_or_else(|| crate::db::now_dts() / 90),
        )
    };
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, "no-store")
        .header("X-Accel-Buffering", "no")
        .header("X-First-Dts-Ms", first_ms.to_string())
        .body(Body::from_stream(video::live_mp4(h)))
        .unwrap())
}

/// Live snapshot: decode the most recent keyframe (cost: one I-frame decode).
async fn snapshot(State(app): State<App>, Path(id): Path<i64>, Query(q): Query<SnapQ>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    can_see(&app, u, id)?;
    let h = app.hub.get(id).ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "camera not running").into_response())?;
    let (init, frag) = {
        let r = h.recent.read();
        let r = r.as_ref().ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "no frames yet").into_response())?;
        let f = r.frags.back().ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "no frames yet").into_response())?;
        (r.init.clone(), f.2.clone())
    };
    let _permit = app.decode_sem.acquire().await.map_err(err500)?;
    let jpeg = crate::thumbs::frag_to_jpeg(&app.cfg.ffmpeg, &init, &frag, q.width.unwrap_or(1280).min(3840), 4).await.map_err(err500)?;
    Ok(([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "no-store")], jpeg).into_response())
}

#[derive(Deserialize)]
struct SnapQ {
    width: Option<u32>,
}

/// Frame at an arbitrary recorded time (keyframe-accurate).
async fn frame_at(State(app): State<App>, Path(id): Path<i64>, Query(q): Query<FrameQ>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    can_see(&app, u, id)?;
    let dts = video::ms_to_dts(q.t);
    let kind = stream_of(&q.stream)?;
    let mut segs = app.db.segments_in_range_stream(id, kind.as_str(), dts, dts + 1).map_err(err500)?;
    if segs.is_empty() && kind == crate::recorder::StreamKind::Sub {
        segs = app.db.segments_in_range(id, dts, dts + 1).map_err(err500)?; // fall back to main
    }
    let seg = segs.first().ok_or_else(|| (StatusCode::NOT_FOUND, "no recording at that time").into_response())?;
    let frag = seg.index.iter().find(|f| f.dts <= dts && dts < f.dts + f.duration as i64).or_else(|| seg.index.first()).copied().ok_or_else(|| err500("empty segment"))?;
    let storage = app.db.storage(seg.storage_id).map_err(err500)?.ok_or_else(|| err500("storage missing"))?;
    let path = std::path::PathBuf::from(&storage.path).join(&seg.path);
    let init_len = seg.init_len;
    let off = seg.dts_offset;
    let (init, data) = tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, Vec<u8>)> {
        let d = crate::thumbs::read_range(&path, frag.offset, frag.len)?;
        Ok((crate::thumbs::read_range(&path, 0, init_len)?, crate::mp4::rebase_fragment(&d, off)))
    })
    .await
    .map_err(err500)?
    .map_err(err500)?;
    let _permit = app.decode_sem.acquire().await.map_err(err500)?;
    let jpeg = crate::thumbs::frag_to_jpeg(&app.cfg.ffmpeg, &init, &data, q.width.unwrap_or(640).min(3840), 5).await.map_err(err500)?;
    Ok(([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "private, max-age=86400")], jpeg).into_response())
}

#[derive(Deserialize)]
struct FrameQ {
    t: i64,
    width: Option<u32>,
    /// "sub" decodes the substream keyframe (~10x cheaper; use for scrub previews)
    stream: Option<String>,
}

// ---------------------------------------------------------------------------
// admin: users, storage, stats
// ---------------------------------------------------------------------------

/// Long-lived bearer token for automation (Home Assistant etc.), bound to the
/// calling admin.  Returned once; stored as a session with a 10-year TTL.
async fn create_token(State(app): State<App>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require_admin(user.as_ref().map(|e| &e.0))?;
    if u.id == 0 {
        return Err(bad("finish setup first"));
    }
    let token = new_token();
    app.db.create_session(u.id, &token, 10 * 365 * 86400).map_err(err500)?;
    Ok(Json(serde_json::json!({"token": token})).into_response())
}

async fn users(State(app): State<App>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    require_admin(user.as_ref().map(|e| &e.0))?;
    let list = app.db.users().map_err(err500)?;
    let out: Vec<serde_json::Value> = list
        .into_iter()
        .map(|u| serde_json::json!({"id": u.id, "username": u.username, "role": u.role, "cameras": app.db.user_cameras(u.id).unwrap_or_default()}))
        .collect();
    Ok(Json(out).into_response())
}

#[derive(Deserialize)]
struct UserCreate {
    username: String,
    password: String,
    role: Option<String>,
    cameras: Option<Vec<i64>>,
}

async fn user_create(State(app): State<App>, user: Option<axum::Extension<AuthUser>>, Json(req): Json<UserCreate>) -> ApiResult {
    require_admin(user.as_ref().map(|e| &e.0))?;
    if req.password.len() < 8 {
        return Err(bad("password >= 8 chars"));
    }
    let role = req.role.unwrap_or_else(|| "viewer".into());
    if role != "admin" && role != "viewer" {
        return Err(bad("role must be admin or viewer"));
    }
    let hash = hash_password(&req.password).map_err(err500)?;
    let id = app.db.add_user(req.username.trim(), &hash, &role).map_err(bad)?;
    if let Some(cs) = req.cameras {
        app.db.set_user_cameras(id, &cs).map_err(err500)?;
    }
    Ok(Json(serde_json::json!({"id": id})).into_response())
}

#[derive(Deserialize)]
struct UserPatch {
    cameras: Option<Vec<i64>>,
}

async fn user_update(State(app): State<App>, Path(id): Path<i64>, user: Option<axum::Extension<AuthUser>>, Json(p): Json<UserPatch>) -> ApiResult {
    require_admin(user.as_ref().map(|e| &e.0))?;
    if let Some(cs) = p.cameras {
        app.db.set_user_cameras(id, &cs).map_err(err500)?;
    }
    Ok(Json(serde_json::json!({"ok": true})).into_response())
}

async fn user_delete(State(app): State<App>, Path(id): Path<i64>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let me = require_admin(user.as_ref().map(|e| &e.0))?;
    if me.id == id {
        return Err(bad("cannot delete yourself"));
    }
    app.db.delete_user(id).map_err(err500)?;
    Ok(Json(serde_json::json!({"ok": true})).into_response())
}

async fn storages(State(app): State<App>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    require_admin(user.as_ref().map(|e| &e.0))?;
    let mut out = Vec::new();
    for s in app.db.storages().map_err(err500)? {
        let used = app.db.storage_used_bytes(s.id).unwrap_or(0);
        let free = crate::retention::fs_free_bytes(std::path::Path::new(&s.path)).unwrap_or(0);
        out.push(serde_json::json!({"id": s.id, "path": s.path, "max_bytes": s.max_bytes, "reserve_bytes": s.reserve_bytes, "used_bytes": used, "free_bytes": free,
            "archive_to": s.archive_to, "archive_after_days": s.archive_after_days, "archive_rate_mbps": s.archive_rate_mbps, "available": std::path::Path::new(&s.path).is_dir()}));
    }
    Ok(Json(out).into_response())
}

async fn storage_update(State(app): State<App>, Path(id): Path<i64>, user: Option<axum::Extension<AuthUser>>, Json(patch): Json<serde_json::Map<String, serde_json::Value>>) -> ApiResult {
    require_admin(user.as_ref().map(|e| &e.0))?;
    app.db.update_storage(id, &patch).map_err(bad)?;
    Ok(Json(serde_json::json!({"ok": true})).into_response())
}

#[derive(Deserialize)]
struct StorageCreate {
    path: String,
    max_gb: Option<f64>,
    reserve_gb: Option<f64>,
}

async fn storage_create(State(app): State<App>, user: Option<axum::Extension<AuthUser>>, Json(req): Json<StorageCreate>) -> ApiResult {
    require_admin(user.as_ref().map(|e| &e.0))?;
    if !std::path::Path::new(&req.path).is_dir() {
        return Err(bad("path must be an existing directory"));
    }
    let id = app
        .db
        .add_storage(&req.path, req.max_gb.map(|g| (g * 1e9) as i64), (req.reserve_gb.unwrap_or(50.0) * 1e9) as i64)
        .map_err(bad)?;
    Ok(Json(serde_json::json!({"id": id})).into_response())
}

async fn stats(State(app): State<App>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let vis = visible_cameras(&app, u);
    let earliest = app.db.earliest_segment_dts(None).map_err(err500)?.map(video::dts_to_ms);
    let mut cams = Vec::new();
    for id in vis {
        if let Some(h) = app.hub.get(id) {
            let sub = app.hub.get_stream(id, crate::recorder::StreamKind::Sub).map(|s| s.status.read().clone());
            cams.push(serde_json::json!({"id": id, "status": h.status.read().clone(), "sub_status": sub, "detect": crate::detect::status(id)}));
        }
    }
    Ok(Json(serde_json::json!({"earliest_ms": earliest, "now_ms": crate::db::now_dts() / 90, "cameras": cams})).into_response())
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({"ok": true, "ts": TIMESCALE}))
}

// ---------------------------------------------------------------------------

pub async fn serve(cfg: Config, db: Db, hub: Arc<LiveHub>) -> Result<()> {
    let listen = cfg.listen.clone();
    let web_dir = cfg.web_dir.clone();
    let setup_token = new_token()[..12].to_string();
    if db.user_count()? == 0 {
        info!("no users yet: open the UI and create the first admin with setup token {setup_token}");
    }
    let app = App {
        cfg: Arc::new(cfg),
        db,
        hub,
        setup_token: Arc::new(setup_token),
        login_guard: Arc::new(parking_lot::Mutex::new(LoginGuard::default())),
        decode_sem: Arc::new(tokio::sync::Semaphore::new(4)),
    };
    let index = ServeFile::new(web_dir.join("index.html"));
    let static_files = ServeDir::new(&web_dir).not_found_service(index);

    let api = Router::new()
        .route("/api/health", get(health))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/setup", post(setup))
        .route("/api/me", get(me))
        .route("/api/cameras", get(cameras).post(camera_create))
        .route("/api/cameras/{id}", get(camera_get).patch(camera_update).delete(camera_delete))
        .route("/api/cameras/{id}/timeline", get(timeline))
        .route("/api/cameras/{id}/video.mp4", get(video_range))
        .route("/api/cameras/{id}/playlist.m3u8", get(video_hls))
        .route("/api/cameras/{id}/live.mp4", get(live))
        .route("/api/cameras/{id}/snapshot.jpg", get(snapshot))
        .route("/api/cameras/{id}/frame.jpg", get(frame_at))
        .route("/api/segments/{id}/file.mp4", get(segment_file))
        .route("/api/segments/{id}/init.mp4", get(segment_init))
        .route("/api/segments/{id}/frag/{fi}/seg.m4s", get(segment_frag))
        .route("/api/events", get(events))
        .route("/api/events/{id}", get(event_get).patch(event_update))
        .route("/api/events/{id}/thumb.jpg", get(event_thumb))
        .route("/api/tokens", post(create_token))
        .route("/api/users", get(users).post(user_create))
        .route("/api/users/{id}", axum::routing::patch(user_update).delete(user_delete))
        .route("/api/storages", get(storages).post(storage_create))
        .route("/api/storages/{id}", axum::routing::patch(storage_update))
        .route("/api/stats", get(stats))
        .merge(crate::zmapi::router())
        .layer(middleware::from_fn_with_state(app.clone(), auth_mw))
        .with_state(app);

    let router = api.fallback_service(static_files).layer(tower_http::trace::TraceLayer::new_for_http());
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    info!(%listen, "http server listening");
    axum::serve(listener, router).await?;
    Ok(())
}
