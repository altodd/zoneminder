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
    /// zmNinja MJPEG streams hold a decoder for their whole life: their own
    /// pool so they never starve snapshots and thumbnails
    pub mjpeg_sem: Arc<tokio::sync::Semaphore>,
    pub bus: Arc<crate::notify::Bus>,
    /// epoch milliseconds when this process started
    pub started_ms: i64,
    /// on-demand H.264 transcode (None = not configured)
    pub transcoder: Option<Arc<crate::transcode::Transcoder>>,
    /// pending PTZ auto-stop timers per camera
    pub ptz_timers: Arc<parking_lot::Mutex<std::collections::HashMap<i64, tokio::task::AbortHandle>>>,
    /// HTTP client for peers, webhooks and probes
    pub http: reqwest::Client,
    /// newest live JPEG per (camera, from main stream, width), with the dts
    /// of the fragment it was decoded from
    pub snap_cache: Arc<parking_lot::Mutex<std::collections::HashMap<SnapKey, (i64, bytes::Bytes)>>>,
    /// one decode at a time per snapshot key (the rest wait and reuse it)
    pub snap_locks: Arc<parking_lot::Mutex<std::collections::HashMap<SnapKey, Arc<tokio::sync::Mutex<()>>>>>,
    /// zmNinja MJPEG streams by `connkey`, so `command=17` (quit) ends one
    pub mjpeg_streams: Arc<parking_lot::Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<()>>>>,
    /// an event re-classification pass is running
    pub classify_running: Arc<std::sync::atomic::AtomicBool>,
}

/// (camera, main stream, width)
pub type SnapKey = (i64, bool, u32);

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
/// JPEG of the newest live keyframe, `width` px wide (never upscaled;
/// `None` = up to 1280). The substream is used when it runs and is wide
/// enough, unless `force_main`: tiles that refresh every few seconds must
/// never decode 4 MP HEVC keyframes. Cached per keyframe, so any number of
/// viewers polling one camera cost one decode per fragment.
pub async fn live_jpeg(app: &App, cam: i64, width: Option<u32>, force_main: bool) -> Result<(bytes::Bytes, &'static str), Response> {
    let want = width.unwrap_or(1280).clamp(32, 3840);
    let sub = if force_main {
        None
    } else {
        app.hub
            .get_stream(cam, crate::recorder::StreamKind::Sub)
            .filter(|h| h.recent.read().as_ref().is_some_and(|r| r.params.width >= want || width.is_none()))
    };
    let source = if sub.is_some() { "sub" } else { "main" };
    let h = sub.or_else(|| app.hub.get(cam)).ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "camera not running").into_response())?;
    let (init, frag_dts, frag, src_w) = {
        let r = h.recent.read();
        let r = r.as_ref().ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "no frames yet").into_response())?;
        let f = r.frags.back().ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "no frames yet").into_response())?;
        (r.init.clone(), f.0, f.2.clone(), r.params.width)
    };
    let w = if src_w > 0 { want.min(src_w) } else { want };
    let key: SnapKey = (cam, source == "main", w);
    let cached = |app: &App| app.snap_cache.lock().get(&key).filter(|(d, _)| *d >= frag_dts).map(|(_, j)| j.clone());
    if let Some(j) = cached(app) {
        return Ok((j, source));
    }
    let lock = {
        let mut locks = app.snap_locks.lock();
        if locks.len() > 512 {
            locks.retain(|_, l| Arc::strong_count(l) > 1);
        }
        locks.entry(key).or_default().clone()
    };
    let _one = lock.lock().await;
    if let Some(j) = cached(app) {
        return Ok((j, source));
    }
    let _permit = app.decode_sem.acquire().await.map_err(err500)?;
    let jpeg = bytes::Bytes::from(crate::thumbs::frag_to_jpeg(&app.cfg.ffmpeg, &init, &frag, w, 4).await.map_err(err500)?);
    let mut c = app.snap_cache.lock();
    if c.len() > 512 {
        c.clear();
    }
    c.insert(key, (frag_dts, jpeg.clone()));
    Ok((jpeg, source))
}

/// JPEG of the keyframe nearest `dts` (recorded) or the latest live keyframe.
pub async fn frame_jpeg_pub(app: &App, cam: i64, dts: i64, width: u32) -> Result<Response, Response> {
    let now = crate::db::now_dts();
    let (init, data) = if dts >= now - 5 * crate::mp4::TIMESCALE as i64 {
        let (jpeg, _) = live_jpeg(app, cam, Some(width), false).await?;
        return Ok(([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "private, max-age=5")], jpeg).into_response());
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
        || path == "/zm/api/host/getVersion.json" || path == "/zm/api/host/login.json" || path == "/zm/ws"
        || !(path.starts_with("/api/") || path.starts_with("/zm/"));
    let user = match token_from(req.headers()).or_else(|| token_from_query(req.uri())) {
        Some(t) => app.db.auth_user(&t).ok().flatten(),
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
        // the setup pseudo-admin may only look at /api/me (the UI needs it to
        // show the setup form); everything else waits for the real first admin
        Some(u) if u.id == 0 && path != "/api/me" && !public => {
            (StatusCode::FORBIDDEN, Json(serde_json::json!({"error":"create the first admin with the setup token first"}))).into_response()
        }
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
        // cameras assigned one by one plus every camera in the user's groups (fails closed)
        app.db.viewer_cameras(u.id).unwrap_or_default()
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
        "transcode": app.transcoder.is_some(),
        "objects": app.hub.objects.read().is_some(),
        "peers": app.cfg.peers.iter().map(|p| p.name.clone()).collect::<Vec<_>>(),
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
    let detect = crate::detect::status(&app.hub, c.id);
    let mut c = c;
    if !admin || app.db.user_count().unwrap_or(1) == 0 {
        // never leak RTSP/ONVIF credentials or internal URLs to viewers
        c.main_url = String::new();
        c.sub_url = None;
        c.onvif_url = String::new();
        c.onvif_user = String::new();
        c.ptz_url = String::new();
    }
    c.onvif_pass = String::new();
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
    if app.db.camera(id).map_err(err500)?.is_none() {
        return Err((StatusCode::NOT_FOUND, "no such camera").into_response());
    }
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
    objects: Vec<crate::notify::DetectedObject>,
    /// seconds of actual motion (not counting pre/post-roll); None for imported events
    motion_secs: Option<f64>,
    /// zones that fired during the event (cameras with named zones)
    zones: Vec<String>,
}

/// Detect fps per camera, to turn an event's motion frame count into seconds.
fn fps_map(app: &App) -> std::collections::HashMap<i64, f64> {
    app.db.cameras().map(|cs| cs.into_iter().map(|c| (c.id, c.detect_fps)).collect()).unwrap_or_default()
}

fn event_out(e: crate::db::Event, fps: &std::collections::HashMap<i64, f64>) -> EventOut {
    let objects = crate::notify::objects_from_meta(&e);
    let meta = serde_json::from_str::<serde_json::Value>(&e.meta_json).ok();
    let motion_secs = meta
        .as_ref()
        .and_then(|m| m.get("motion_frames").and_then(|f| f.as_f64()))
        .and_then(|f| fps.get(&e.camera_id).filter(|r| **r > 0.0).map(|r| (f / r * 10.0).round() / 10.0));
    EventOut {
        motion_secs,
        objects,
        zones: crate::notify::zones_from_meta(&e),
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
    let fps = fps_map(&app);
    let events = app.db.events_in_range(id, s, e).map_err(err500)?.into_iter().map(|e| event_out(e, &fps)).collect();
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
    /// comma separated event kinds (motion, person, car, ...)
    kind: Option<String>,
    /// minimum seconds of actual motion (hides blips)
    min_motion: Option<f64>,
    /// comma separated zone names: events in which one of them fired
    zone: Option<String>,
}

async fn events(State(app): State<App>, Query(q): Query<EventsQ>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let vis = visible_cameras(&app, u);
    let cams: Vec<i64> = match &q.camera {
        Some(s) if !s.is_empty() => s.split(',').filter_map(|x| x.trim().parse().ok()).filter(|c| vis.contains(c)).collect(),
        _ => vis.clone(),
    };
    let kinds: Option<Vec<String>> = q.kind.as_ref().filter(|s| !s.is_empty()).map(|s| s.split(',').map(|k| k.trim().to_string()).filter(|k| !k.is_empty()).collect());
    let zones: Option<Vec<String>> = q.zone.as_ref().filter(|s| !s.trim().is_empty()).map(|s| s.split(',').map(|k| k.trim().to_string()).filter(|k| !k.is_empty()).collect());
    let list = app
        .db
        .events_q(&crate::db::EventQuery {
            cameras: Some(&cams),
            start: q.start.map(video::ms_to_dts),
            end: q.end.map(video::ms_to_dts),
            min_score: q.min_score.unwrap_or(0),
            archived_only: q.archived.unwrap_or(false),
            before_id: q.before,
            limit: q.limit.unwrap_or(50).min(500),
            ascending: q.order.as_deref() == Some("asc"),
            kinds: kinds.as_deref(),
            min_motion_secs: q.min_motion,
            zones: zones.as_deref(),
        })
        .map_err(err500)?;
    let fps = fps_map(&app);
    let out: Vec<EventOut> = list.into_iter().map(|e| event_out(e, &fps)).collect();
    Ok(Json(out).into_response())
}

async fn event_get(State(app): State<App>, Path(id): Path<i64>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let e = app.db.event(id).map_err(err500)?.ok_or_else(|| (StatusCode::NOT_FOUND, "no such event").into_response())?;
    can_see(&app, u, e.camera_id)?;
    Ok(Json(event_out(e, &fps_map(&app))).into_response())
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

#[derive(Deserialize, Default)]
struct ClassifyReq {
    /// how far back (default 7 days)
    days: Option<f64>,
    /// only this camera
    camera: Option<i64>,
    /// also events that were classified before
    #[serde(default)]
    again: bool,
}

/// Run object detection on the thumbnails of past events (for events
/// recorded before a detector was configured, or after changing labels).
/// Runs in the background, one event at a time; returns how many are queued.
async fn events_classify(State(app): State<App>, user: Option<axum::Extension<AuthUser>>, body: Option<Json<ClassifyReq>>) -> ApiResult {
    use std::sync::atomic::Ordering;
    require_admin(user.as_ref().map(|e| &e.0))?;
    let q = body.map(|b| b.0).unwrap_or_default();
    let det = app.hub.objects.read().clone().ok_or_else(|| bad("no object detector configured ([objects] in zmng.toml)"))?;
    let since = crate::db::now_dts() - (q.days.unwrap_or(7.0).clamp(0.01, 90.0) * 86_400.0 * TIMESCALE as f64) as i64;
    let cams: std::collections::HashMap<i64, crate::db::Camera> =
        app.db.cameras().map_err(err500)?.into_iter().filter(|c| c.objects && q.camera.is_none_or(|id| id == c.id)).map(|c| (c.id, c)).collect();
    let ids: Vec<i64> = cams.keys().copied().collect();
    let all = app.db.events(Some(&ids), Some(since), None, 0, false, None, 5000, false, None).map_err(err500)?;
    // background boxes (parked vehicles) for events that predate motion cells
    let statics = crate::objects::static_boxes(&all, 5);
    let events: Vec<crate::db::Event> = all.into_iter().filter(|e| e.thumb_path.is_some() && (q.again || !e.meta_json.contains("\"classified\""))).collect();
    if app.classify_running.swap(true, Ordering::SeqCst) {
        return Err((StatusCode::CONFLICT, Json(serde_json::json!({"error": "a classification pass is already running"}))).into_response());
    }
    let queued = events.len();
    let app2 = app.clone();
    tokio::spawn(async move {
        let (mut changed, mut failed) = (0usize, 0usize);
        for e in events {
            let Some(cam) = cams.get(&e.camera_id) else { continue };
            let Ok(jpeg) = tokio::fs::read(app2.cfg.thumb_dir.join(e.thumb_path.as_deref().unwrap_or_default())).await else { continue };
            match crate::objects::classify_event(&det, &app2.db, cam, e.id, jpeg, &statics).await {
                Ok(true) => {
                    changed += 1;
                    app2.bus.publish_event(&app2.db, e.id, crate::notify::Notification::EventUpdate);
                }
                Ok(false) => {}
                Err(err) => {
                    failed += 1;
                    tracing::warn!(event = e.id, "classify: {err:#}");
                }
            }
        }
        info!(queued, changed, failed, background_boxes = statics.len(), "event classification pass done");
        app2.classify_running.store(false, Ordering::SeqCst);
    });
    Ok((StatusCode::ACCEPTED, Json(serde_json::json!({"queued": queued}))).into_response())
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
    /// "h264" transcodes an HEVC source on demand (needs [transcode])
    codec: Option<String>,
}

/// Whether the client asked for H.264 (`codec=h264`); anything else is 400.
fn wants_h264(codec: &Option<String>) -> Result<bool, Response> {
    match codec.as_deref() {
        None | Some("") => Ok(false),
        Some("h264") | Some("avc") | Some("avc1") => Ok(true),
        _ => Err(bad("codec must be h264")),
    }
}

/// Wrap an fMP4 stream in a transcode session when the client wants H.264
/// and the source is not H.264 already. Returns (mime, body).
/// Returns (mime, body, transcoded).
async fn maybe_transcode(app: &App, want_h264: bool, source_mime: &str, first_dts: i64, fps: f64, body: impl futures::Stream<Item = std::io::Result<bytes::Bytes>> + Send + 'static) -> Result<(String, Body, bool), Response> {
    if !want_h264 || source_mime.contains("avc1") {
        return Ok((source_mime.to_string(), Body::from_stream(body), false));
    }
    let Some(t) = app.transcoder.as_ref() else {
        return Err((StatusCode::NOT_IMPLEMENTED, Json(serde_json::json!({"error": "no [transcode] configured; use the substream"}))).into_response());
    };
    let tc = match t.start(body, first_dts, fps).await {
        Ok(tc) => tc,
        Err(e) if e.to_string().contains("sessions are in use") => return Err((StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"error": e.to_string()}))).into_response()),
        Err(e) => return Err(err500(e)),
    };
    use futures::StreamExt;
    let init = futures::stream::once(async move { Ok::<_, std::io::Error>(tc.init) });
    Ok((tc.mime, Body::from_stream(init.chain(tc.frags)), true))
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
    let want_h264 = wants_h264(&q.codec)?;
    let plan = video::plan_range_stream(&app.db, id, kind.as_str(), video::ms_to_dts(q.start), video::ms_to_dts(q.end)).map_err(err500)?;
    if plan.items.is_empty() {
        return Err((StatusCode::NOT_FOUND, "no recording in range").into_response());
    }
    let source_mime = plan.mime.clone().unwrap_or_else(|| "video/mp4".into());
    let first_dts = plan.first_dts.unwrap_or(video::ms_to_dts(q.start));
    let first = video::dts_to_ms(first_dts);
    let len = plan.bytes;
    let exact = plan.exact_len;
    let fps = app.hub.get(id).map(|h| h.status.read().fps as f64).filter(|f| *f > 0.5).unwrap_or(18.0);
    let (mime, body, transcoded) = maybe_transcode(&app, want_h264, &source_mime, first_dts, fps, video::stream_plan(plan)).await?;
    let mut b = Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, "private, max-age=3600")
        .header(header::ACCEPT_RANGES, "none")
        .header("X-First-Dts-Ms", first.to_string());
    if exact && !transcoded {
        b = b.header(header::CONTENT_LENGTH, len);
    }
    Ok(b.body(body).unwrap())
}

#[derive(Deserialize)]
struct ExportQ {
    /// comma separated camera ids
    cameras: String,
    /// epoch milliseconds
    start: i64,
    end: i64,
    /// "main" (default, full resolution) or "sub"
    stream: Option<String>,
}

/// Check an export request and plan it (blocking reads of the index and of
/// fragment headers run off the async threads).
async fn export_plan(app: &App, u: &User, q: &ExportQ) -> Result<crate::export::ExportPlan, Response> {
    let (start, end) = (video::ms_to_dts(q.start), video::ms_to_dts(q.end));
    if end <= start || end - start > crate::export::MAX_RANGE_DTS {
        return Err(bad("range must be 0 < end-start <= 6 h"));
    }
    let kind = stream_of(&q.stream)?;
    let mut ids: Vec<i64> = Vec::new();
    for part in q.cameras.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let id: i64 = part.parse().map_err(|_| bad("cameras: comma separated ids"))?;
        can_see(app, u, id)?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    if ids.is_empty() || ids.len() > crate::export::MAX_CAMERAS {
        return Err(bad(format!("choose 1 to {} cameras", crate::export::MAX_CAMERAS)));
    }
    let mut cams = Vec::new();
    for id in ids {
        cams.push(app.db.camera(id).map_err(err500)?.ok_or_else(|| (StatusCode::NOT_FOUND, "no such camera").into_response())?);
    }
    let db = app.db.clone();
    let stream = kind.as_str().to_string();
    tokio::task::spawn_blocking(move || crate::export::plan(&db, &cams, &stream, start, end)).await.map_err(err500)?.map_err(err500)
}

fn export_info(u: &User) -> crate::export::ExportInfo {
    crate::export::ExportInfo { user: u.username.clone(), server: format!("zmng {}", env!("CARGO_PKG_VERSION")), timezone: crate::zmapi::local_tz_name(), at: chrono::Local::now() }
}

fn export_zip_name(plan: &crate::export::ExportPlan) -> String {
    let (a, b) = (crate::export::local(plan.start_dts), crate::export::local(plan.end_dts));
    format!("zmng-export {}-{}.zip", a.format("%Y-%m-%d %H%M"), b.format("%H%M"))
}

/// What an export of this range would contain: files, sizes, gaps, cameras
/// without recording, and the archive size (when known in advance).
async fn export_preview(State(app): State<App>, Query(q): Query<ExportQ>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let plan = export_plan(&app, u, &q).await?;
    let info = export_info(u);
    let name = export_zip_name(&plan);
    let (plan, total) = tokio::task::spawn_blocking(move || {
        let total = crate::export::length(&plan, &info);
        (plan, total)
    })
    .await
    .map_err(err500)?;
    let total = total.map_err(err500)?;
    let files: Vec<serde_json::Value> = plan
        .files
        .iter()
        .map(|f| serde_json::json!({"name": f.name, "camera_id": f.camera_id, "camera": f.camera, "stream": f.stream, "format": f.format,
            "codec": f.codec, "width": f.width, "height": f.height, "bytes": f.size, "frames": f.frames,
            "first_ms": f.first_dts / 90, "end_ms": f.end_dts / 90, "gaps": crate::export::gaps_json(&f.gaps)}))
        .collect();
    let missing: Vec<serde_json::Value> = plan.missing.iter().map(|m| serde_json::json!({"camera_id": m.camera_id, "camera": m.camera, "reason": m.reason})).collect();
    Ok(Json(serde_json::json!({"zip": name, "total_bytes": total, "files": files, "missing": missing})).into_response())
}

/// The export itself: a ZIP streamed as it is written (see `export.rs`).
async fn export_zip(State(app): State<App>, Query(q): Query<ExportQ>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let plan = export_plan(&app, u, &q).await?;
    if plan.files.is_empty() {
        return Err((StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "no recording for these cameras in this range"}))).into_response());
    }
    let info = export_info(u);
    let name = export_zip_name(&plan);
    info!(user = %u.username, cameras = %q.cameras, start = q.start, end = q.end, stream = %plan.stream, files = plan.files.len(), "export");
    let (plan, info, total) = tokio::task::spawn_blocking(move || {
        let total = crate::export::length(&plan, &info);
        (plan, info, total)
    })
    .await
    .map_err(err500)?;
    let total = total.map_err(err500)?;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(4);
    tokio::task::spawn_blocking(move || {
        let mut w = crate::export::ChannelWriter::new(tx.clone());
        let res = crate::export::write(&plan, &info, &mut w, false).and_then(|n| {
            std::io::Write::flush(&mut w)?;
            Ok(n)
        });
        match res {
            Ok(n) if total.is_some_and(|t| t != n) => tracing::error!(expected = total, wrote = n, "export length differs from its dry run"),
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("export stopped: {e:#}");
                let _ = tx.blocking_send(Err(std::io::Error::other(e.to_string())));
            }
        }
    });
    let body = Body::from_stream(futures::stream::poll_fn(move |cx| rx.poll_recv(cx)));
    let mut b = Response::builder()
        .header(header::CONTENT_TYPE, "application/zip")
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\""));
    if let Some(t) = total {
        b = b.header(header::CONTENT_LENGTH, t);
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
    let init = crate::mp4::init_segment(&se.video_params());
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
    let audio_rate = app.db.sample_entry(seg.sample_entry_id).map_err(err500)?.and_then(|se| se.audio.map(|a| a.sample_rate));
    let data = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let d = crate::thumbs::read_range(&path, frag.offset, frag.len)?;
        Ok(crate::mp4::rebase_fragment_av(&d, off, audio_rate))
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
    codec: Option<String>,
}

async fn live(State(app): State<App>, Path(id): Path<i64>, Query(q): Query<StreamQ>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    can_see(&app, u, id)?;
    let kind = stream_of(&q.stream)?;
    let want_h264 = wants_h264(&q.codec)?;
    let h = app.hub.get_stream(id, kind).ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "stream not running").into_response())?;
    let (source_mime, first_dts) = {
        let r = h.recent.read();
        (
            r.as_ref().map(|r| r.params.mime()).unwrap_or_else(|| "video/mp4".into()),
            r.as_ref().and_then(|r| r.frags.back().map(|f| f.0)).unwrap_or_else(crate::db::now_dts),
        )
    };
    let fps = h.status.read().fps as f64;
    let (mime, body, _) = maybe_transcode(&app, want_h264, &source_mime, first_dts, if fps > 0.5 { fps } else { 18.0 }, video::live_mp4(h)).await?;
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, "no-store")
        .header("X-Accel-Buffering", "no")
        .header("X-First-Dts-Ms", video::dts_to_ms(first_dts).to_string())
        .body(body)
        .unwrap())
}

/// Live snapshot: decode the most recent keyframe (cost: one I-frame
/// decode). The substream is used when it runs, unless `stream=main` or the
/// requested width exceeds what the substream has: 23 tiles refreshing every
/// 2 s must never decode 4 MP HEVC keyframes.
async fn snapshot(State(app): State<App>, Path(id): Path<i64>, Query(q): Query<SnapQ>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    can_see(&app, u, id)?;
    let (jpeg, source) = live_jpeg(&app, id, q.width, q.stream.as_deref() == Some("main")).await?;
    Ok(([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "no-store"), (axum::http::HeaderName::from_static("x-stream"), source)], jpeg).into_response())
}

/// The fragment holding `dts` in a recorder's open (not yet indexed)
/// segment: (file, fragment, init length, dts offset 0).
fn open_fragment(app: &App, cam: i64, kind: crate::recorder::StreamKind, dts: i64) -> Option<(std::path::PathBuf, crate::mp4::FragEntry, u32, i64)> {
    let h = app.hub.get_stream(cam, kind)?;
    let open = h.open.read().clone()?;
    let f = open.frags.iter().find(|f| f.dts <= dts && dts < f.dts + f.duration as i64).copied()?;
    Some((open.abs_path, f, open.init_len, 0))
}

#[derive(Deserialize)]
struct SnapQ {
    width: Option<u32>,
    /// "main" forces the main stream (full resolution)
    stream: Option<String>,
}

/// Frame at an arbitrary recorded time (keyframe-accurate).
async fn frame_at(State(app): State<App>, Path(id): Path<i64>, Query(q): Query<FrameQ>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    can_see(&app, u, id)?;
    let dts = video::ms_to_dts(q.t);
    let kind = stream_of(&q.stream)?;
    let mut segs = app.db.segments_in_range_stream(id, kind.as_str(), dts, dts + 1).map_err(err500)?;
    let mut located = None;
    if segs.is_empty() {
        // the last minute is still in the open segment (on disk, not indexed yet)
        located = open_fragment(&app, id, kind, dts);
    }
    if segs.is_empty() && located.is_none() && kind == crate::recorder::StreamKind::Sub {
        segs = app.db.segments_in_range(id, dts, dts + 1).map_err(err500)?; // fall back to main
        if segs.is_empty() {
            located = open_fragment(&app, id, crate::recorder::StreamKind::Main, dts);
        }
    }
    let (path, frag, init_len, off) = match located {
        Some(l) => l,
        None => {
            let seg = segs.first().ok_or_else(|| (StatusCode::NOT_FOUND, "no recording at that time").into_response())?;
            let frag = seg.index.iter().find(|f| f.dts <= dts && dts < f.dts + f.duration as i64).or_else(|| seg.index.first()).copied().ok_or_else(|| err500("empty segment"))?;
            let storage = app.db.storage(seg.storage_id).map_err(err500)?.ok_or_else(|| err500("storage missing"))?;
            (std::path::PathBuf::from(&storage.path).join(&seg.path), frag, seg.init_len, seg.dts_offset)
        }
    };
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

/// Scrub preview tile nearest `t` (no decode: one small file read).
async fn preview_at(State(app): State<App>, Path(id): Path<i64>, Query(q): Query<PreviewQ>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    can_see(&app, u, id)?;
    let dts = video::ms_to_dts(q.t);
    let max_delta = (app.cfg.preview_secs.max(1) as i64) * TIMESCALE as i64;
    let store = crate::preview::Store::new(&app.cfg.thumb_dir);
    let tile = tokio::task::spawn_blocking(move || store.nearest(id, dts, max_delta)).await.map_err(err500)?.map_err(err500)?;
    match tile {
        Some(jpeg) => Ok(([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "private, max-age=86400")], jpeg).into_response()),
        None => Err((StatusCode::NOT_FOUND, "no preview at that time").into_response()),
    }
}

// ---------------------------------------------------------------------------
// PTZ (ONVIF)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct PtzReq {
    /// move | stop | preset | set_preset | home
    action: String,
    #[serde(default)]
    pan: f64,
    #[serde(default)]
    tilt: f64,
    #[serde(default)]
    zoom: f64,
    /// auto-stop after this many seconds (0 = keep moving until `stop`)
    #[serde(default)]
    seconds: f64,
    preset: Option<String>,
    name: Option<String>,
}

fn ptz_client(cam: &crate::db::Camera) -> Result<(crate::onvif::Client, String, String), Response> {
    if !cam.ptz || cam.ptz_url.is_empty() || cam.ptz_profile.is_empty() {
        return Err(bad("camera has no PTZ (run the PTZ probe first)"));
    }
    let (u, p) = crate::onvif::credentials(cam);
    let c = crate::onvif::Client::new(&u, &p).map_err(err500)?;
    Ok((c, cam.ptz_url.clone(), cam.ptz_profile.clone()))
}

fn ptz_err(e: anyhow::Error) -> Response {
    (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": format!("camera: {e:#}")}))).into_response()
}

/// Discover the camera's ONVIF media/PTZ services and store them.
async fn ptz_probe(State(app): State<App>, Path(id): Path<i64>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    require_admin(user.as_ref().map(|e| &e.0))?;
    let cam = app.db.camera(id).map_err(err500)?.ok_or_else(|| (StatusCode::NOT_FOUND, "no such camera").into_response())?;
    let device = crate::onvif::default_device_url(&cam).ok_or_else(|| bad("set onvif_url or a main_url with a host"))?;
    let (u, p) = crate::onvif::credentials(&cam);
    let c = crate::onvif::Client::new(&u, &p).map_err(err500)?;
    let probe = c.probe(&device).await.map_err(ptz_err)?;
    let patch = serde_json::json!({"ptz": true, "ptz_url": probe.ptz_url, "ptz_profile": probe.profile, "onvif_url": device});
    app.db.update_camera(id, patch.as_object().unwrap()).map_err(err500)?;
    Ok(Json(serde_json::json!({"ok": true, "ptz_url": probe.ptz_url, "media_url": probe.media_url, "profile": probe.profile})).into_response())
}

/// Move/stop/preset. Admins only: a PTZ move changes what every viewer sees.
async fn ptz_command(State(app): State<App>, Path(id): Path<i64>, user: Option<axum::Extension<AuthUser>>, Json(req): Json<PtzReq>) -> ApiResult {
    require_admin(user.as_ref().map(|e| &e.0))?;
    let cam = app.db.camera(id).map_err(err500)?.ok_or_else(|| (StatusCode::NOT_FOUND, "no such camera").into_response())?;
    let (c, url, profile) = ptz_client(&cam)?;
    // any new command cancels a pending auto-stop
    if let Some(t) = app.ptz_timers.lock().remove(&id) {
        t.abort();
    }
    match req.action.as_str() {
        "move" => {
            if req.pan.abs() > 1.0 || req.tilt.abs() > 1.0 || req.zoom.abs() > 1.0 {
                return Err(bad("pan/tilt/zoom must be within -1..1"));
            }
            c.continuous_move(&url, &profile, req.pan, req.tilt, req.zoom).await.map_err(ptz_err)?;
            // (re)arm after the move so two quick moves never leave a stale timer
            if let Some(t) = app.ptz_timers.lock().remove(&id) {
                t.abort();
            }
            if req.seconds > 0.0 {
                let (c2, url2, profile2) = (c, url.clone(), profile.clone());
                let secs = req.seconds.min(30.0);
                let handle = tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs_f64(secs)).await;
                    if let Err(e) = c2.stop(&url2, &profile2).await {
                        tracing::warn!(camera = id, "ptz auto-stop: {e:#}");
                    }
                });
                app.ptz_timers.lock().insert(id, handle.abort_handle());
            }
        }
        "stop" => c.stop(&url, &profile).await.map_err(ptz_err)?,
        "preset" => c.goto_preset(&url, &profile, req.preset.as_deref().ok_or_else(|| bad("preset required"))?).await.map_err(ptz_err)?,
        "set_preset" => {
            let token = c.set_preset(&url, &profile, req.name.as_deref().ok_or_else(|| bad("name required"))?).await.map_err(ptz_err)?;
            return Ok(Json(serde_json::json!({"ok": true, "preset": token})).into_response());
        }
        "home" => c.goto_home(&url, &profile).await.map_err(ptz_err)?,
        _ => return Err(bad("action must be move, stop, preset, set_preset or home")),
    }
    Ok(Json(serde_json::json!({"ok": true})).into_response())
}

/// Admin only, like the pad: the reply may carry the camera's ONVIF URL.
async fn ptz_presets(State(app): State<App>, Path(id): Path<i64>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    require_admin(user.as_ref().map(|e| &e.0))?;
    let cam = app.db.camera(id).map_err(err500)?.ok_or_else(|| (StatusCode::NOT_FOUND, "no such camera").into_response())?;
    let (c, url, profile) = ptz_client(&cam)?;
    let presets = c.presets(&url, &profile).await.map_err(ptz_err)?;
    Ok(Json(presets).into_response())
}

#[derive(Deserialize)]
struct PreviewQ {
    t: i64,
}

/// Hours (UTC, `YYYYMMDDHH`) that have preview strips.
async fn preview_hours(State(app): State<App>, Path(id): Path<i64>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    can_see(&app, u, id)?;
    let hours = crate::preview::Store::new(&app.cfg.thumb_dir).hours(id);
    Ok(Json(serde_json::json!({"interval_secs": app.cfg.preview_secs, "hours": hours})).into_response())
}

/// `<hour>.jpg` = sprite sheet of that hour, `<hour>.json` = its manifest.
async fn preview_sprite(State(app): State<App>, Path((id, hour)): Path<(i64, String)>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    can_see(&app, u, id)?;
    let (key, want_json) = match hour.rsplit_once('.') {
        Some((k, "jpg")) => (k.to_string(), false),
        Some((k, "json")) => (k.to_string(), true),
        _ => return Err(bad("use <hour>.jpg or <hour>.json")),
    };
    if key.len() != 10 || !key.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad("hour must be YYYYMMDDHH"));
    }
    let store = crate::preview::Store::new(&app.cfg.thumb_dir);
    let res = tokio::task::spawn_blocking(move || store.sprite(id, &key)).await.map_err(err500)?.map_err(err500)?;
    let Some((jpeg, manifest)) = res else { return Err((StatusCode::NOT_FOUND, "no previews for that hour").into_response()) };
    if want_json {
        Ok(([(header::CACHE_CONTROL, "private, max-age=300")], Json(manifest)).into_response())
    } else {
        Ok(([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "private, max-age=300")], jpeg).into_response())
    }
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
#[derive(Deserialize, Default)]
struct TokenCreate {
    /// what it is for ("Home Assistant", "barn peer"); default "API token"
    name: Option<String>,
    /// whose token it is (default: the admin creating it); a federation
    /// peer's or a guest integration's token belongs to a viewer
    user_id: Option<i64>,
    /// None/absent = never expires
    expires_days: Option<f64>,
}

/// Issue a long-lived API token (admins). It acts exactly as its user,
/// with that user's cameras; it is returned once and stored hashed.
async fn create_token(State(app): State<App>, user: Option<axum::Extension<AuthUser>>, body: Option<Json<TokenCreate>>) -> ApiResult {
    let me = require_admin(user.as_ref().map(|e| &e.0))?;
    if me.id == 0 {
        return Err(bad("finish setup first"));
    }
    let req = body.map(|b| b.0).unwrap_or_default();
    let name = req.name.as_deref().map(str::trim).unwrap_or("API token");
    if name.is_empty() || name.len() > 80 {
        return Err(bad("token name: 1-80 characters"));
    }
    let owner = match req.user_id {
        Some(id) => app.db.session_user_by_id(id).map_err(err500)?.ok_or_else(|| bad("no such user"))?,
        None => me.clone(),
    };
    let expires_at = match req.expires_days {
        Some(d) if !(d > 0.0 && d <= 3650.0) => return Err(bad("expires_days: more than 0, at most 3650")),
        Some(d) => Some(crate::db::now_secs() + (d * 86400.0) as i64),
        None => None,
    };
    let token = format!("zmng_{}", new_token());
    let id = app.db.create_api_token(owner.id, name, &token, expires_at, Some(me.id)).map_err(err500)?;
    info!(by = %me.username, owner = %owner.username, name, "API token created");
    Ok(Json(serde_json::json!({"id": id, "token": token, "name": name, "user_id": owner.id, "username": owner.username, "expires_at": expires_at})).into_response())
}

/// API tokens: every token for admins, a user's own otherwise.
async fn list_tokens(State(app): State<App>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let rows = app.db.api_tokens(if u.role == "admin" { None } else { Some(u.id) }).map_err(err500)?;
    Ok(Json(rows).into_response())
}

/// Revoke a token: admins any, users their own (others' look absent).
async fn revoke_token(State(app): State<App>, Path(id): Path<i64>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    if !app.db.delete_api_token(id, if u.role == "admin" { None } else { Some(u.id) }).map_err(err500)? {
        return Err((StatusCode::NOT_FOUND, "no such token").into_response());
    }
    info!(by = %u.username, token = id, "API token revoked");
    Ok(Json(serde_json::json!({"ok": true})).into_response())
}

#[derive(Deserialize)]
struct PasswordChange {
    current: String,
    new: String,
}

/// Change your own password (any role). The current password is required,
/// also when the request comes with an API token, and failures count
/// towards the login throttle. Other sign-ins of this user end; the one
/// making the change and the user's API tokens stay.
async fn change_own_password(State(app): State<App>, headers: HeaderMap, uri: axum::http::Uri, user: Option<axum::Extension<AuthUser>>, Json(p): Json<PasswordChange>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    if u.id == 0 {
        return Err(bad("finish setup first"));
    }
    if !app.login_guard.lock().allowed() {
        return Err((StatusCode::TOO_MANY_REQUESTS, Json(serde_json::json!({"error":"too many failed attempts; try again in a minute"}))).into_response());
    }
    if !verify_password(&p.current, &u.pass_hash) {
        app.login_guard.lock().record_failure();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        return Err((StatusCode::FORBIDDEN, Json(serde_json::json!({"error":"the current password is not right"}))).into_response());
    }
    if p.new.len() < 8 {
        return Err(bad("the new password needs at least 8 characters"));
    }
    if p.new == p.current {
        return Err(bad("the new password is the same as the current one"));
    }
    app.db.set_password_hash(u.id, &hash_password(&p.new).map_err(err500)?).map_err(err500)?;
    let keep = token_from(&headers).or_else(|| token_from_query(&uri));
    let ended = app.db.delete_user_sessions(u.id, keep.as_deref()).map_err(err500)?;
    info!(user = %u.username, other_sessions_ended = ended, "password changed by the user");
    Ok(Json(serde_json::json!({"ok": true, "sessions_ended": ended})).into_response())
}

async fn users(State(app): State<App>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    require_admin(user.as_ref().map(|e| &e.0))?;
    let list = app.db.users().map_err(err500)?;
    let out: Vec<serde_json::Value> = list
        .into_iter()
        .map(|u| serde_json::json!({"id": u.id, "username": u.username, "role": u.role, "cameras": app.db.user_cameras(u.id).unwrap_or_default(), "groups": app.db.user_groups(u.id).unwrap_or_default(), "visible": if u.role == "admin" { Vec::new() } else { app.db.viewer_cameras(u.id).unwrap_or_default() }}))
        .collect();
    Ok(Json(out).into_response())
}

#[derive(Deserialize)]
struct UserCreate {
    username: String,
    password: String,
    role: Option<String>,
    cameras: Option<Vec<i64>>,
    groups: Option<Vec<String>>,
}

/// Usernames: what ZoneMinder clients accept (letters, digits, . _ @ -).
fn valid_username(name: &str) -> Result<&str, Response> {
    let n = name.trim();
    if n.is_empty() || n.len() > 64 || !n.chars().all(|c| c.is_ascii_alphanumeric() || "._@-".contains(c)) {
        return Err(bad("username: 1-64 letters, digits, . _ @ or -"));
    }
    Ok(n)
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
    let name = valid_username(&req.username)?;
    if app.db.user_by_name(name).map_err(err500)?.is_some() {
        return Err(bad("username already taken"));
    }
    let hash = hash_password(&req.password).map_err(err500)?;
    let id = app.db.add_user(name, &hash, &role).map_err(bad)?;
    if let Some(cs) = req.cameras {
        app.db.set_user_cameras(id, &cs).map_err(err500)?;
    }
    if let Some(gs) = req.groups {
        app.db.set_user_groups(id, &gs).map_err(err500)?;
    }
    Ok(Json(serde_json::json!({"id": id})).into_response())
}

#[derive(Deserialize)]
struct UserPatch {
    cameras: Option<Vec<i64>>,
    groups: Option<Vec<String>>,
    username: Option<String>,
    password: Option<String>,
    role: Option<String>,
}

/// Edit a user: name, password, role, and what a viewer may see (cameras
/// and/or groups). A password reset ends the user's sign-ins; renames and
/// grant changes leave them, and API tokens stay valid throughout. The last
/// admin cannot be demoted, and nobody can demote themselves.
async fn user_update(State(app): State<App>, Path(id): Path<i64>, headers: HeaderMap, user: Option<axum::Extension<AuthUser>>, Json(p): Json<UserPatch>) -> ApiResult {
    let me = require_admin(user.as_ref().map(|e| &e.0))?.clone();
    let target = app.db.session_user_by_id(id).map_err(err500)?.ok_or_else(|| (StatusCode::NOT_FOUND, "no such user").into_response())?;
    // validate everything before changing anything
    let name = p.username.as_deref().map(valid_username).transpose()?;
    if let Some(n) = name {
        if n != target.username && app.db.user_by_name(n).map_err(err500)?.is_some() {
            return Err(bad("username already taken"));
        }
    }
    if p.password.as_ref().is_some_and(|pw| pw.len() < 8) {
        return Err(bad("password >= 8 chars"));
    }
    if let Some(r) = &p.role {
        if r != "admin" && r != "viewer" {
            return Err(bad("role must be admin or viewer"));
        }
        if r != "admin" && target.role == "admin" {
            if me.id == id {
                return Err(bad("you cannot remove your own admin role"));
            }
            if app.db.admin_count().map_err(err500)? <= 1 {
                return Err(bad("this is the last admin"));
            }
        }
    }
    if let Some(n) = name {
        if n != target.username {
            app.db.set_username(id, n).map_err(bad)?;
            info!(user = id, from = %target.username, to = %n, "user renamed");
        }
    }
    if let Some(pw) = &p.password {
        app.db.set_password_hash(id, &hash_password(pw).map_err(err500)?).map_err(err500)?;
        // a reset ends that user's sign-ins (an admin resetting their own keeps this one)
        let keep = if me.id == id { token_from(&headers) } else { None };
        let ended = app.db.delete_user_sessions(id, keep.as_deref()).map_err(err500)?;
        info!(user = id, by = %me.username, sessions_ended = ended, "password reset");
    }
    if let Some(r) = &p.role {
        app.db.set_role(id, r).map_err(err500)?;
    }
    if let Some(cs) = p.cameras {
        app.db.set_user_cameras(id, &cs).map_err(err500)?;
    }
    if let Some(gs) = p.groups {
        app.db.set_user_groups(id, &gs).map_err(err500)?;
    }
    Ok(Json(serde_json::json!({"ok": true})).into_response())
}

#[derive(Deserialize)]
struct GroupRename {
    from: String,
    /// absent or null = remove the group (cameras and users keep everything else)
    to: Option<String>,
}

/// Rename or remove a camera group on every camera and every user grant.
async fn group_rename(State(app): State<App>, user: Option<axum::Extension<AuthUser>>, Json(g): Json<GroupRename>) -> ApiResult {
    require_admin(user.as_ref().map(|e| &e.0))?;
    let to = g.to.as_deref().map(str::trim).filter(|t| !t.is_empty());
    if g.from.trim().is_empty() || to.is_some_and(|t| t.contains(',') || t.len() > 64) {
        return Err(bad("group names: 1-64 characters, no commas"));
    }
    let n = app.db.rename_group(&g.from, to).map_err(err500)?;
    Ok(Json(serde_json::json!({"ok": true, "cameras": n})).into_response())
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
            "archive_to": s.archive_to, "archive_after_days": s.archive_after_days, "archive_rate_mbps": s.archive_rate_mbps, "read_only": s.read_only, "available": crate::retention::storage_mounted(&s),
            "archive_backlog_bytes": crate::tier::backlog_bytes(&app.db, &s).unwrap_or(0)}));
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
            cams.push(serde_json::json!({"id": id, "status": h.status.read().clone(), "sub_status": sub, "detect": crate::detect::status(&app.hub, id)}));
        }
    }
    Ok(Json(serde_json::json!({"earliest_ms": earliest, "now_ms": crate::db::now_dts() / 90, "cameras": cams})).into_response())
}

/// Server-sent events: every notification for cameras the user may see.
/// The UI uses it for toasts and to refresh lists without polling.
async fn event_stream(State(app): State<App>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
    let u = require(user.as_ref().map(|e| &e.0))?;
    let user = u.clone();
    let rx = app.bus.subscribe();
    let app2 = app.clone();
    let stream = futures::stream::unfold(rx, move |mut rx| {
        let (app, user) = (app2.clone(), user.clone());
        async move {
            loop {
                match rx.recv().await {
                    Ok(n) => {
                        if !n.is_external() {
                            continue;
                        }
                        // re-evaluated per notification (one indexed query) so a
                        // revoked camera or deleted account stops the feed at once
                        let user = app.db.session_user_by_id(user.id).ok().flatten()?;
                        let allowed = match n.camera_id() {
                            Some(c) => visible_cameras(&app, &user).contains(&c),
                            None => user.role == "admin",
                        };
                        if !allowed {
                            continue;
                        }
                        let ev = SseEvent::default().event(n.name()).json_data(&n).unwrap_or_else(|_| SseEvent::default());
                        return Some((Ok::<_, std::convert::Infallible>(ev), rx));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return None,
                }
            }
        }
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15)).text("ping")).into_response())
}

/// Full status report (cameras, detectors, storage headroom, issues).
async fn status(State(app): State<App>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let vis = visible_cameras(&app, u);
    let admin = u.role == "admin";
    let (db, hub, started, after) = (app.db.clone(), app.hub.clone(), app.started_ms, app.cfg.alert_after_minutes as i64 * 60_000);
    // statvfs on a hung mount must not stall a runtime worker
    let r = tokio::task::spawn_blocking(move || crate::health::report(&db, &hub, started, after, &vis, admin)).await.map_err(err500)?.map_err(err500)?;
    Ok(Json(r).into_response())
}

/// Prometheus text format of the same report (admin token).
async fn metrics(State(app): State<App>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require_admin(user.as_ref().map(|e| &e.0))?;
    let vis = visible_cameras(&app, u);
    let (db, hub, started, after) = (app.db.clone(), app.hub.clone(), app.started_ms, app.cfg.alert_after_minutes as i64 * 60_000);
    let r = tokio::task::spawn_blocking(move || crate::health::report(&db, &hub, started, after, &vis, true)).await.map_err(err500)?.map_err(err500)?;
    Ok(([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], crate::health::prometheus(&r)).into_response())
}

// ---------------------------------------------------------------------------
// federation: peers
// ---------------------------------------------------------------------------

/// Peers and whether they answer right now (admins also see the URL).
async fn peers(State(app): State<App>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let admin = u.role == "admin";
    let checks = app.cfg.peers.iter().map(|p| {
        let (http, p) = (app.http.clone(), p.clone());
        async move {
            let ok = tokio::time::timeout(std::time::Duration::from_secs(3), http.get(format!("{}/api/health", p.url.trim_end_matches('/'))).send())
                .await
                .map(|r| r.map(|r| r.status().is_success()).unwrap_or(false))
                .unwrap_or(false);
            let mut v = serde_json::json!({"name": p.name, "ok": ok});
            if admin {
                v["url"] = serde_json::Value::String(p.url.clone());
            }
            v
        }
    });
    let out: Vec<serde_json::Value> = futures::future::join_all(checks).await;
    Ok(Json(out).into_response())
}

/// Reverse proxy to a peer's read API with the peer token. The browser
/// never sees the token; the peer's own ACL (for the token's account)
/// decides what comes back.
async fn peer_proxy(State(app): State<App>, Path((name, path)): Path<(String, String)>, user: Option<axum::Extension<AuthUser>>, req: Request<Body>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let peer = app.cfg.peers.iter().find(|p| p.name == name).ok_or_else(|| (StatusCode::NOT_FOUND, "no such peer").into_response())?;
    let need_admin = crate::peers::allowed(req.method(), &path).ok_or_else(|| (StatusCode::NOT_FOUND, "not proxied").into_response())?;
    if need_admin && u.role != "admin" {
        return Err((StatusCode::FORBIDDEN, "admin only").into_response());
    }
    // axum percent-decodes the capture and reqwest normalizes dot segments,
    // so the allow-list is enforced on the final, parsed URL as well
    if path.split('/').any(|seg| seg.is_empty() || seg == "." || seg == ".." || seg.contains('%') || seg.contains('\\')) {
        return Err((StatusCode::NOT_FOUND, "not proxied").into_response());
    }
    let mut url = format!("{}/{}", peer.url.trim_end_matches('/'), path.trim_start_matches('/'));
    if let Some(q) = req.uri().query() {
        url.push('?');
        url.push_str(q);
    }
    let parsed = url::Url::parse(&url).map_err(|_| (StatusCode::NOT_FOUND, "not proxied").into_response())?;
    let base = url::Url::parse(&peer.url).map_err(err500)?;
    if parsed.host_str() != base.host_str() || parsed.port_or_known_default() != base.port_or_known_default() || parsed.scheme() != base.scheme() {
        return Err((StatusCode::NOT_FOUND, "not proxied").into_response());
    }
    // a peer may live under a path prefix (https://host/zmng)
    let rel_path = parsed.path().strip_prefix(base.path().trim_end_matches('/')).unwrap_or(parsed.path()).to_string();
    if crate::peers::allowed(req.method(), &rel_path) != Some(need_admin) {
        return Err((StatusCode::NOT_FOUND, "not proxied").into_response());
    }
    let method = req.method().clone();
    let mut out = app.http.request(method.clone(), &url).bearer_auth(&peer.token).timeout(std::time::Duration::from_secs(3600));
    for h in ["range", "content-type", "if-none-match", "accept"] {
        if let Some(v) = req.headers().get(h) {
            out = out.header(h, v.clone());
        }
    }
    let body = axum::body::to_bytes(req.into_body(), 1 << 20).await.map_err(|_| bad("body too large"))?;
    if !body.is_empty() {
        out = out.body(body);
    }
    let res = out.send().await.map_err(|e| (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": format!("peer {name}: {e}")}))).into_response())?;
    if res.status() == reqwest::StatusCode::UNAUTHORIZED || res.status() == reqwest::StatusCode::FORBIDDEN {
        // the peer rejected OUR token; never let that log the local user out
        return Err((StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": format!("peer {name}: token rejected ({})", res.status())}))).into_response());
    }
    // the peer token may belong to an admin there: camera JSON is stripped of
    // RTSP/ONVIF URLs and credentials whatever the peer returned
    let strip = method == axum::http::Method::GET && (rel_path == "/api/cameras" || rel_path.starts_with("/api/cameras/") && !rel_path[13..].contains('/'));
    if strip {
        let ct = res.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let status = res.status().as_u16();
        let body = res.bytes().await.map_err(|e| (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": format!("peer {name}: {e}")}))).into_response())?;
        let mut v: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
        let blank = |c: &mut serde_json::Value| {
            if let Some(o) = c.as_object_mut() {
                for k in ["main_url", "sub_url", "onvif_url", "onvif_user", "onvif_pass", "ptz_url"] {
                    if let Some(x) = o.get_mut(k) {
                        *x = if x.is_null() { serde_json::Value::Null } else { serde_json::Value::String(String::new()) };
                    }
                }
            }
        };
        match &mut v {
            serde_json::Value::Array(a) => a.iter_mut().for_each(blank),
            other => blank(other),
        }
        return Ok(Response::builder().status(status).header(header::CONTENT_TYPE, if ct.is_empty() { "application/json".to_string() } else { ct }).body(Body::from(v.to_string())).unwrap());
    }
    let mut b = Response::builder().status(res.status().as_u16());
    for h in crate::peers::PASS_HEADERS {
        if let Some(v) = res.headers().get(*h) {
            b = b.header(*h, v.clone());
        }
    }
    use futures::TryStreamExt;
    let stream = res.bytes_stream().map_err(std::io::Error::other);
    Ok(b.body(Body::from_stream(stream)).unwrap())
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({"ok": true, "ts": TIMESCALE}))
}

// ---------------------------------------------------------------------------

impl App {
    /// Assemble the shared application state. The setup token is printed by
    /// `serve` when no users exist yet.
    pub fn new(cfg: Config, db: Db, hub: Arc<LiveHub>, bus: Arc<crate::notify::Bus>) -> App {
        let transcoder = cfg.transcode.clone().map(|t| Arc::new(crate::transcode::Transcoder::new(t, cfg.ffmpeg.clone())));
        App {
            cfg: Arc::new(cfg),
            db,
            hub,
            bus,
            setup_token: Arc::new(new_token()[..12].to_string()),
            login_guard: Arc::new(parking_lot::Mutex::new(LoginGuard::default())),
            decode_sem: Arc::new(tokio::sync::Semaphore::new(4)),
            // a 640x360 MJPEG stream costs a few % of one core now that ffmpeg
            // no longer starts a thread per core (see thumbs::ffmpeg_command)
            mjpeg_sem: Arc::new(tokio::sync::Semaphore::new(8)),
            started_ms: crate::db::now_dts() / 90,
            transcoder,
            ptz_timers: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
            http: reqwest::Client::builder().connect_timeout(std::time::Duration::from_secs(3)).build().expect("reqwest client"),
            snap_cache: Default::default(),
            snap_locks: Default::default(),
            mjpeg_streams: Default::default(),
            classify_running: Default::default(),
        }
    }
}

/// The complete router: JSON API, ZoneMinder-compatible API, static UI.
/// Separate from `serve` so tests can drive it in-process.
pub fn router(app: App) -> Router {
    let web_dir = app.cfg.web_dir.clone();
    let index = ServeFile::new(web_dir.join("index.html"));
    // revalidate the UI on every load (a 304 when unchanged): without a
    // Cache-Control, browsers kept serving the previous app.js for hours
    // after an upgrade (heuristic freshness from Last-Modified)
    let static_files = tower::ServiceBuilder::new()
        .layer(tower_http::set_header::SetResponseHeaderLayer::if_not_present(header::CACHE_CONTROL, HeaderValue::from_static("no-cache")))
        .service(ServeDir::new(&web_dir).not_found_service(index));

    let api = Router::new()
        .route("/api/health", get(health))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/setup", post(setup))
        .route("/api/me", get(me))
        .route("/api/cameras", get(cameras).post(camera_create))
        .route("/api/cameras/{id}", get(camera_get).patch(camera_update).delete(camera_delete))
        .route("/api/cameras/{id}/timeline", get(timeline))
        .route("/api/export.json", get(export_preview))
        .route("/api/export.zip", get(export_zip))
        .route("/api/cameras/{id}/video.mp4", get(video_range))
        .route("/api/cameras/{id}/playlist.m3u8", get(video_hls))
        .route("/api/cameras/{id}/live.mp4", get(live))
        .route("/api/cameras/{id}/snapshot.jpg", get(snapshot))
        .route("/api/cameras/{id}/frame.jpg", get(frame_at))
        .route("/api/cameras/{id}/preview.jpg", get(preview_at))
        .route("/api/cameras/{id}/ptz", post(ptz_command))
        .route("/api/cameras/{id}/ptz/probe", post(ptz_probe))
        .route("/api/cameras/{id}/ptz/presets", get(ptz_presets))
        .route("/api/cameras/{id}/previews", get(preview_hours))
        .route("/api/cameras/{id}/previews/{hour}", get(preview_sprite))
        .route("/api/segments/{id}/file.mp4", get(segment_file))
        .route("/api/segments/{id}/init.mp4", get(segment_init))
        .route("/api/segments/{id}/frag/{fi}/seg.m4s", get(segment_frag))
        .route("/api/events", get(events))
        .route("/api/events/classify", post(events_classify))
        .route("/api/events/{id}", get(event_get).patch(event_update))
        .route("/api/events/{id}/thumb.jpg", get(event_thumb))
        .route("/api/events/stream", get(event_stream))
        .route("/api/tokens", get(list_tokens).post(create_token))
        .route("/api/tokens/{id}", axum::routing::delete(revoke_token))
        .route("/api/me/password", post(change_own_password))
        .route("/api/users", get(users).post(user_create))
        .route("/api/users/{id}", axum::routing::patch(user_update).delete(user_delete))
        .route("/api/groups/rename", post(group_rename))
        .route("/api/storages", get(storages).post(storage_create))
        .route("/api/storages/{id}", axum::routing::patch(storage_update))
        .route("/api/stats", get(stats))
        .route("/api/peers", get(peers))
        .route("/api/peers/{name}/{*path}", axum::routing::any(peer_proxy))
        .route("/api/status", get(status))
        .route("/api/metrics", get(metrics))
        .merge(crate::zmapi::router())
        .layer(middleware::from_fn_with_state(app.clone(), auth_mw))
        .with_state(app);

    api.fallback_service(static_files).layer(tower_http::trace::TraceLayer::new_for_http())
}

pub async fn serve(cfg: Config, db: Db, hub: Arc<LiveHub>, bus: Arc<crate::notify::Bus>) -> Result<()> {
    let listen = cfg.listen.clone();
    let app = App::new(cfg, db, hub, bus);
    if app.db.user_count()? == 0 {
        info!("no users yet: open the UI and create the first admin with setup token {}", app.setup_token);
    }
    let router = router(app);
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    info!(%listen, "http server listening");
    axum::serve(listener, router).await?;
    Ok(())
}
