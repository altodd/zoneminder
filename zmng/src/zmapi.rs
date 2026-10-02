//! ZoneMinder API compatibility layer for zmNinjaNg (and legacy zmNinja).
//!
//! Serves the subset of ZoneMinder's CakePHP JSON API, `index.php` media
//! views and `nph-zms` streaming that the mobile client actually uses (see
//! docs/redesign/research/03-zmninjang-api.md), mapped onto zmng's model:
//!
//! * a ZoneMinder "Monitor" = a zmng camera;
//! * a ZoneMinder "Event"  = a zmng event row (motion interval) whose video
//!   is served as a complete MP4 cut from the continuous recording;
//! * JWT `?token=` = a zmng session token (access: short, refresh: long).
//!
//! Everything lives under `/zm/...` so the client's discovery probe finds it
//! at `https://host/zm/api`.

use anyhow::Result;
use axum::{
    body::Body,
    extract::{Path, Query, RawQuery, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Form, Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tracing::{debug, info};

use crate::api::{hash_password_dummy, new_token, App, AuthUser};
use crate::db::{Event, User};
use crate::mp4::TIMESCALE;
use crate::video;

const ZM_VERSION: &str = "1.38.5";
const API_VERSION: &str = "2.0";
const ACCESS_TTL: i64 = 3600;
const REFRESH_TTL: i64 = 7 * 86_400;

type ApiResult = Result<Response, Response>;

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({"success": false, "data": {"name": msg, "message": msg}}))).into_response()
}
fn e500(e: impl std::fmt::Display) -> Response {
    err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
}
fn require(u: Option<&AuthUser>) -> Result<&User, Response> {
    u.map(|x| &x.0).ok_or_else(|| err(StatusCode::UNAUTHORIZED, "Not Authenticated"))
}

/// IANA zone name of the server, best effort.
pub fn local_tz_name() -> String {
    if let Ok(tz) = std::env::var("TZ") {
        if !tz.is_empty() {
            return tz;
        }
    }
    if let Ok(s) = std::fs::read_to_string("/etc/timezone") {
        let s = s.trim();
        if !s.is_empty() {
            return s.to_string();
        }
    }
    if let Ok(target) = std::fs::read_link("/etc/localtime") {
        let t = target.to_string_lossy().to_string();
        if let Some(i) = t.find("zoneinfo/") {
            return t[i + 9..].to_string();
        }
    }
    "UTC".to_string()
}

/// ZoneMinder datetime string (server-local, naive).
fn zm_time(dts: i64) -> String {
    let secs = dts / TIMESCALE as i64;
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0)
        .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

/// Parse a ZoneMinder naive local datetime into 90 kHz ticks.
fn parse_zm_time(s: &str) -> Option<i64> {
    use chrono::TimeZone;
    let s = s.trim();
    let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S"))
        .ok()?;
    let local = chrono::Local.from_local_datetime(&naive).single()?;
    Some(local.timestamp() * TIMESCALE as i64)
}

// ---------------------------------------------------------------------------
// host / config / misc
// ---------------------------------------------------------------------------

async fn get_version() -> Response {
    Json(json!({"version": ZM_VERSION, "apiversion": API_VERSION})).into_response()
}

#[derive(Deserialize)]
struct LoginForm {
    user: Option<String>,
    pass: Option<String>,
    token: Option<String>,
}

async fn login(State(app): State<App>, Form(f): Form<LoginForm>) -> ApiResult {
    let user = if let Some(refresh) = f.token.filter(|t| !t.is_empty()) {
        app.db.session_user(&refresh).map_err(e500)?.ok_or_else(|| err(StatusCode::UNAUTHORIZED, "Invalid refresh token"))?
    } else {
        if !app.login_guard.lock().allowed() {
            return Err(err(StatusCode::TOO_MANY_REQUESTS, "Too many failed logins"));
        }
        let (u, p) = (f.user.unwrap_or_default(), f.pass.unwrap_or_default());
        let user = app.db.user_by_name(&u).map_err(e500)?;
        let ok = user.as_ref().map(|x| crate::api::verify_password_pub(&p, &x.pass_hash)).unwrap_or(false);
        if !ok {
            hash_password_dummy();
            app.login_guard.lock().record_failure();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            return Err(err(StatusCode::UNAUTHORIZED, "Login denied for user"));
        }
        user.unwrap()
    };
    let access = new_token();
    let refresh = new_token();
    app.db.create_session(user.id, &access, ACCESS_TTL).map_err(e500)?;
    app.db.create_session(user.id, &refresh, REFRESH_TTL).map_err(e500)?;
    info!(user = %user.username, "zm-api login");
    Ok(Json(json!({
        "access_token": access,
        "access_token_expires": ACCESS_TTL,
        "refresh_token": refresh,
        "refresh_token_expires": REFRESH_TTL,
        "credentials": format!("token={access}"),
        "append_password": 0,
        "version": ZM_VERSION,
        "apiversion": API_VERSION,
    }))
    .into_response())
}

async fn get_timezone() -> Response {
    Json(json!({"DateTime": {"TimeZone": local_tz_name()}})).into_response()
}
async fn daemon_check() -> Response {
    Json(json!({"result": 1})).into_response()
}
async fn get_load() -> Response {
    let mut l = [0f64; 3];
    let n = unsafe { libc::getloadavg(l.as_mut_ptr(), 3) };
    let v: Vec<f64> = if n == 3 { l.to_vec() } else { vec![0.0, 0.0, 0.0] };
    Json(json!({"load": v})).into_response()
}
async fn get_disk_percent(State(app): State<App>) -> Response {
    let mut usage = serde_json::Map::new();
    let mut total_used = 0i64;
    let mut total_cap = 0i64;
    for s in app.db.storages().unwrap_or_default() {
        let used = app.db.storage_used_bytes(s.id).unwrap_or(0);
        let free = crate::retention::fs_free_bytes(std::path::Path::new(&s.path)).unwrap_or(0) as i64;
        total_used += used;
        total_cap += used + free;
        usage.insert(format!("storage{}", s.id), json!({"space": format!("{:.1}", used as f64 / 1e9), "color": "#4f9cf9"}));
    }
    let pct = if total_cap > 0 { total_used as f64 * 100.0 / total_cap as f64 } else { 0.0 };
    usage.insert("Total".into(), json!({"space": format!("{pct:.1}"), "color": "#8b93a1"}));
    Json(json!({"usage": usage})).into_response()
}
async fn config_by_name(State(app): State<App>, Path(name): Path<String>, headers: HeaderMap) -> Response {
    let name = name.trim_end_matches(".json");
    let value = match name {
        "ZM_PATH_ZMS" => "/zm/cgi-bin/nph-zms".to_string(),
        "ZM_GO2RTC_PATH" => go2rtc_path(&app, &headers).unwrap_or_default(),
        "ZM_MIN_STREAMING_PORT" => String::new(),
        "ZM_OPT_USE_API" | "ZM_OPT_USE_AUTH" => "1".into(),
        _ => String::new(),
    };
    Json(json!({"config": {"Id": 1, "Name": name, "Value": value, "Type": "string"}})).into_response()
}
async fn empty_list(Path(what): Path<String>) -> Response {
    let key = what.trim_end_matches(".json").to_string();
    Json(json!({ key: [] })).into_response()
}
async fn users(State(_app): State<App>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let admin = u.role == "admin";
    Ok(Json(json!({"users": [{"User": {
        "Id": u.id.to_string(), "Username": u.username, "Enabled": "1",
        "System": if admin { "Edit" } else { "None" }, "Monitors": if admin { "Edit" } else { "View" },
        // PTZ is for admins (a move changes what every viewer sees)
        "Stream": "View", "Events": "Edit", "Control": if admin { "Edit" } else { "None" }, "Groups": "View", "Snapshots": "View", "Devices": "None"
    }}]}))
    .into_response())
}
/// Stable id for a group name (ZoneMinder groups have numeric ids; ours are
/// the cameras' tags): FNV-1a of the lower-cased name, positive 31-bit.
pub fn group_id(name: &str) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for b in name.trim().to_lowercase().bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    (h & 0x7fff_ffff).max(1)
}

/// `groups.json`: the camera groups, each with the monitors this user may see
/// (zmNinjaNg filters its montage and event lists by them).
async fn groups(State(app): State<App>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let mut groups: std::collections::BTreeMap<String, (String, Vec<Value>)> = Default::default();
    for c in visible(&app, u) {
        for t in c.tags.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            groups.entry(t.to_lowercase()).or_insert_with(|| (t.to_string(), Vec::new())).1.push(json!({"Id": c.id.to_string(), "Name": c.name}));
        }
    }
    let rows: Vec<Value> = groups
        .into_values()
        .map(|(name, mons)| json!({"Group": {"Id": group_id(&name).to_string(), "Name": name, "ParentId": null}, "Monitor": mons}))
        .collect();
    Ok(Json(json!({"groups": rows})).into_response())
}

async fn storage_list(State(app): State<App>) -> Response {
    let rows: Vec<Value> = app
        .db
        .storages()
        .unwrap_or_default()
        .into_iter()
        .map(|s| json!({"Storage": {"Id": s.id.to_string(), "Name": format!("storage{}", s.id), "Path": s.path, "Type": "local", "Scheme": "Medium", "DiskSpace": app.db.storage_used_bytes(s.id).unwrap_or(0).to_string(), "Enabled": "1"}}))
        .collect();
    Json(json!({"storage": rows})).into_response()
}
async fn not_found() -> Response {
    err(StatusCode::NOT_FOUND, "Not Found")
}
async fn saved() -> Response {
    Json(json!({"message": "Saved"})).into_response()
}
/// zmNinjaNg's Start/Stop/Restart and run-state buttons: zmng has no run
/// states and is a system service, so say so instead of "Saved".
async fn state_change() -> Response {
    err(StatusCode::NOT_IMPLEMENTED, "zmng has no run states; start, stop or restart it on the server (systemctl restart zmng)")
}

// ---------------------------------------------------------------------------
// monitors
// ---------------------------------------------------------------------------

fn monitor_json(app: &App, c: &crate::db::Camera, seq: usize) -> Value {
    let st = app.hub.get(c.id).map(|h| h.status.read().clone()).unwrap_or_default();
    // an external go2rtc, or our own MSE websocket (see go2rtc_session)
    let go2rtc = app.cfg.go2rtc_url.is_some() || app.cfg.zmninja_mse;
    let det = crate::detect::status(&app.hub, c.id).filter(|d| d.running);
    let on = |b: bool| if b { "Always" } else { "None" };
    json!({
        "Monitor": {
            "Id": c.id.to_string(), "Name": c.name, "Type": "Ffmpeg", "Function": if c.enabled { "Mocord" } else { "None" },
            "Capturing": on(c.enabled), "Analysing": on(c.enabled && det.is_some()), "Recording": on(c.enabled), "Decoding": on(c.enabled),
            "Enabled": if c.enabled { "1" } else { "0" }, "Width": st.width.to_string(), "Height": st.height.to_string(),
            "Orientation": "ROTATE_0", "Controllable": if c.ptz { "1" } else { "0" }, "ControlId": if c.ptz { json!("1") } else { Value::Null }, "ServerId": null, "StorageId": c.storage_id.to_string(),
            "Sequence": seq.to_string(), "Deleted": false, "LinkedMonitors": null, "Path": "", "Options": null,
            "StreamChannel": "CameraDirectSecondary", "RTSPServer": "0", "Go2RTCEnabled": go2rtc, "Go2RTCType": null,
            "RTSP2WebEnabled": false, "JanusEnabled": false, "DefaultPlayer": null, "Colours": "4",
            "ImageBufferCount": "3", "MaxFPS": null, "AlarmMaxFPS": null, "SaveJPEGs": "0", "VideoWriter": "2",
            "RefBlendPerc": "6", "AlarmRefBlendPerc": "6", "Importance": "Normal", "Notes": "", "Latitude": null, "Longitude": null,
            "ModectDuringPTZ": "0", "DefaultCodec": "auto", "AnalysisSource": "Secondary", "AnalysisImage": "YChannel", "Manufacturer": null, "Model": null
        },
        "Monitor_Status": {
            "MonitorId": c.id.to_string(),
            "Status": if st.connected { "Connected" } else { "NotRunning" },
            "CaptureFPS": format!("{:.1}", st.fps), "AnalysisFPS": format!("{:.1}", det.map(|d| d.fps).unwrap_or(0.0)),
            "CaptureBandwidth": ((st.kbps * 125.0) as i64).to_string()
        }
    })
}

fn visible(app: &App, u: &User) -> Vec<crate::db::Camera> {
    let vis = crate::api::visible_cameras_pub(app, u);
    app.db.cameras().unwrap_or_default().into_iter().filter(|c| vis.contains(&c.id)).collect()
}

async fn monitors(State(app): State<App>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let rows: Vec<Value> = visible(&app, u).iter().enumerate().map(|(i, c)| monitor_json(&app, c, i)).collect();
    Ok(Json(json!({"monitors": rows})).into_response())
}
async fn monitor_one(State(app): State<App>, Path(id): Path<String>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let id: i64 = id.trim_end_matches(".json").parse().map_err(|_| err(StatusCode::NOT_FOUND, "Invalid monitor"))?;
    let cams = visible(&app, u);
    let (i, c) = cams.iter().enumerate().find(|(_, c)| c.id == id).ok_or_else(|| err(StatusCode::NOT_FOUND, "Invalid monitor"))?;
    Ok(Json(json!({"monitor": monitor_json(&app, c, i)})).into_response())
}
/// `id:<n>/command:<status|on|off>.json` path segments.
fn alarm_args(rest: &str) -> (Option<i64>, String) {
    let mut id = None;
    let mut cmd = String::new();
    for seg in rest.trim_end_matches(".json").split('/') {
        match seg.split_once(':') {
            Some(("id", v)) => id = v.trim().parse().ok(),
            Some(("command", v)) => cmd = v.trim().to_lowercase(),
            _ => {}
        }
    }
    (id, cmd)
}

/// ZoneMinder's alarm state for a monitor: 2 (Alarm) while the detector has
/// an event open, else 0 (Idle). zmNinjaNg polls this for its Live Activity
/// page and the alarm ring. Forcing an alarm on or off has no equivalent
/// here (events come from motion and object detection), so `on`/`off` get
/// ZoneMinder's error shape and the app reports the failure.
async fn monitor_alarm(State(app): State<App>, Path(rest): Path<String>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let (id, cmd) = alarm_args(&rest);
    let id = id.filter(|id| crate::api::visible_cameras_pub(&app, u).contains(id)).ok_or_else(|| err(StatusCode::NOT_FOUND, "Invalid monitor"))?;
    if cmd != "status" {
        return Ok(Json(json!({"status": "false", "code": 501, "error": "zmng does not force alarms: events come from motion and object detection"})).into_response());
    }
    let alarm = crate::detect::status(&app.hub, id).is_some_and(|d| d.running && d.in_event.is_some());
    let state = if alarm { "2" } else { "0" };
    Ok(Json(json!({"status": state, "output": state})).into_response())
}

/// `POST monitors/<id>.json` from the app's mode and settings dialogs. zmng
/// records and detects continuously, so only on/off maps: `Enabled`,
/// `Capturing` (None = off) and `Function` None/Mocord. A value that is
/// already true (Analysing=Always on an enabled camera) is accepted; every
/// other change is refused with a message rather than a "Saved" that did
/// nothing. Admins only, as in ZoneMinder (Monitors: Edit).
async fn monitor_post(State(app): State<App>, Path(id): Path<String>, user: Option<axum::Extension<AuthUser>>, Form(f): Form<HashMap<String, String>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    if u.role != "admin" {
        return Err(err(StatusCode::FORBIDDEN, "Insufficient privileges"));
    }
    let id: i64 = id.trim_end_matches(".json").parse().map_err(|_| err(StatusCode::NOT_FOUND, "Invalid monitor"))?;
    let cam = app.db.camera(id).map_err(e500)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "Invalid monitor"))?;
    let mut enabled: Option<bool> = None;
    let mut refused = Vec::new();
    let mut want = |v: bool, field: &str, refused: &mut Vec<String>| match enabled {
        Some(prev) if prev != v => refused.push(format!("{field} contradicts another field")),
        _ => enabled = Some(v),
    };
    for (k, v) in &f {
        let Some(field) = k.strip_prefix("Monitor[").and_then(|x| x.strip_suffix(']')) else { continue };
        let v = v.trim();
        match field {
            "Enabled" => want(v == "1" || v.eq_ignore_ascii_case("true"), field, &mut refused),
            "Capturing" if v.eq_ignore_ascii_case("None") => want(false, field, &mut refused),
            "Capturing" if v.eq_ignore_ascii_case("Always") || v.eq_ignore_ascii_case("Ondemand") => want(true, field, &mut refused),
            "Function" if v.eq_ignore_ascii_case("None") => want(false, field, &mut refused),
            "Function" if v.eq_ignore_ascii_case("Mocord") => want(true, field, &mut refused),
            // what an enabled camera does anyway
            "Analysing" | "Recording" | "Decoding" if v.eq_ignore_ascii_case("Always") => {}
            "Name" if v == cam.name => {}
            _ => refused.push(format!("{field}={v}")),
        }
    }
    if !refused.is_empty() {
        refused.sort();
        let msg = format!("zmng cannot set {} (it records and detects continuously; only Enabled, Capturing and Function None/Mocord can be changed from the app)", refused.join(", "));
        return Err(err(StatusCode::BAD_REQUEST, &msg));
    }
    if let Some(on) = enabled.filter(|on| *on != cam.enabled) {
        app.db.update_camera(id, json!({"enabled": on}).as_object().unwrap()).map_err(e500)?;
        info!(camera = id, enabled = on, user = %u.username, "camera switched from the ZoneMinder API");
    }
    Ok(saved().await)
}
async fn monitor_daemon(Path(_rest): Path<String>) -> Response {
    Json(json!({"status": "ok", "statustext": "running"})).into_response()
}
/// ZoneMinder zone rows for one camera: its zones as Active, its masks as
/// Inactive, or one "All" zone (the whole frame) when it has none; percent
/// coordinates with two decimals, as ZoneMinder stores them.
fn zone_rows(c: &crate::db::Camera) -> Vec<Value> {
    let fmt = |poly: &[(f64, f64)]| poly.iter().map(|(x, y)| format!("{:.2},{:.2}", x * 100.0, y * 100.0)).collect::<Vec<_>>().join(" ");
    let mut zones = crate::detect::parse_polys(&c.zones_json);
    let masks = crate::detect::parse_polys(&c.masks_json);
    let named = !zones.is_empty();
    if zones.is_empty() {
        zones.push(vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)]);
    }
    let row = |i: usize, kind: &str, name: String, poly: &[(f64, f64)]| {
        json!({"Zone": {"Id": (c.id * 1000 + i as i64 + 1).to_string(), "MonitorId": c.id.to_string(), "Name": name, "Type": kind,
            "Units": "Percent", "NumCoords": poly.len().to_string(), "Coords": fmt(poly), "CheckMethod": "Blobs",
            "MinPixelThreshold": c.pixel_threshold.to_string(), "AlarmRGB": "16711680"}})
    };
    let mut out: Vec<Value> = zones.iter().enumerate().map(|(i, p)| row(i, "Active", if named { format!("Zone {}", i + 1) } else { "All".into() }, p)).collect();
    out.extend(masks.iter().enumerate().map(|(i, p)| row(zones.len() + i, "Inactive", format!("Mask {}", i + 1), p)));
    out
}

async fn zones(State(app): State<App>, Query(q): Query<HashMap<String, String>>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let only: Option<i64> = q.get("MonitorId").and_then(|s| s.parse().ok());
    let rows: Vec<Value> = visible(&app, u).iter().filter(|c| only.is_none_or(|id| id == c.id)).flat_map(zone_rows).collect();
    Ok(Json(json!({"zones": rows})).into_response())
}

// ---------------------------------------------------------------------------
// events
// ---------------------------------------------------------------------------

#[derive(Default, Debug)]
pub struct EventFilter {
    pub monitors: Vec<i64>,
    pub start_ge: Option<i64>,
    pub start_le: Option<i64>,
    pub end_le: Option<i64>,
    pub min_score: u8,
    pub archived: Option<bool>,
    pub ids: Vec<i64>,
    pub notes_re: Option<String>,
}

/// Parse ZoneMinder's path-segment filter grammar, e.g.
/// `MonitorId:3/StartDateTime >=:2026-09-27 10:00:00/AlarmFrames >=:1.json`.
pub fn parse_filter(path: &str) -> EventFilter {
    let mut f = EventFilter::default();
    for raw in path.split('/') {
        let seg = raw.trim_end_matches(".json").trim();
        if seg.is_empty() || seg == "index" {
            continue;
        }
        let Some((key, val)) = seg.split_once(':') else { continue };
        let (key, val) = (key.trim(), val.trim());
        match key {
            "MonitorId" => {
                if let Ok(v) = val.parse() {
                    f.monitors.push(v);
                }
            }
            "StartDateTime >=" | "StartDateTime >" | "StartTime >=" | "StartTime >" => f.start_ge = parse_zm_time(val),
            "StartDateTime <=" | "StartDateTime <" | "StartTime <=" | "StartTime <" => f.start_le = parse_zm_time(val),
            "EndDateTime <=" | "EndDateTime <" | "EndTime <=" => f.end_le = parse_zm_time(val),
            "AlarmFrames >=" => f.min_score = if val.parse::<i64>().unwrap_or(0) > 0 { 1 } else { 0 },
            "Archived" => f.archived = Some(val == "1"),
            "Id IN" => f.ids = val.split(',').filter_map(|x| x.trim().parse().ok()).collect(),
            "Notes REGEXP" | "Cause REGEXP" => f.notes_re = Some(val.to_string()),
            _ => debug!(seg, "ignored event filter segment"),
        }
    }
    f
}

fn event_json(e: &Event, cam_name: &str, width: u32, height: u32) -> Value {
    let end = e.end_dts.unwrap_or(e.start_dts);
    let len = (end - e.start_dts) as f64 / TIMESCALE as f64;
    let frames = (len * 18.0).round() as i64;
    let peak_frame = e.peak_dts.map(|p| ((p - e.start_dts) as f64 / TIMESCALE as f64 * 18.0).round() as i64).unwrap_or(1).max(1);
    let cause = match e.kind.as_str() {
        "motion" => "Motion",
        "zm" => "ZoneMinder",
        other => other,
    };
    json!({
        "Id": e.id.to_string(), "MonitorId": e.camera_id.to_string(), "Name": format!("{cam_name} {}", e.id),
        "Cause": cause, "Notes": e.notes.clone().unwrap_or_else(|| format!("detected:{}", e.kind)),
        "StartDateTime": zm_time(e.start_dts), "EndDateTime": e.end_dts.map(zm_time),
        "Length": format!("{len:.2}"), "Frames": frames.to_string(), "AlarmFrames": (if e.score > 0 { frames.max(1) } else { 0 }).to_string(),
        "AlarmFrameId": peak_frame.to_string(), "MaxScoreFrameId": peak_frame.to_string(),
        "MaxScore": e.score.to_string(), "AvgScore": (e.score / 2).to_string(), "TotScore": (e.score as i64 * frames).to_string(),
        "Width": width.to_string(), "Height": height.to_string(), "Orientation": "ROTATE_0",
        "DefaultVideo": format!("{}-video.mp4", e.id), "Videoed": "1", "SaveJPEGs": "0",
        "Archived": if e.archived { "1" } else { "0" }, "StorageId": "1", "DiskSpace": null, "Emailed": "0", "Scheme": "Medium"
    })
}

#[derive(Deserialize)]
struct PageQ {
    page: Option<usize>,
    limit: Option<usize>,
    sort: Option<String>,
    direction: Option<String>,
}

async fn events_index(State(app): State<App>, path: Option<Path<String>>, Query(q): Query<PageQ>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let filter = parse_filter(path.as_ref().map(|p| p.0.as_str()).unwrap_or(""));
    let vis = crate::api::visible_cameras_pub(&app, u);
    let cams: Vec<i64> = if filter.monitors.is_empty() { vis.clone() } else { filter.monitors.iter().copied().filter(|m| vis.contains(m)).collect() };
    let page = q.page.unwrap_or(1).max(1);
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let asc = q.direction.as_deref().map(|d| d.eq_ignore_ascii_case("asc")).unwrap_or(false);
    let _ = q.sort;
    let (rows, total) = app.db.events_page(&cams, &filter, page, limit, asc).map_err(e500)?;
    let names: HashMap<i64, (String, u32, u32)> = app
        .db
        .cameras()
        .unwrap_or_default()
        .into_iter()
        .map(|c| {
            let st = app.hub.get(c.id).map(|h| h.status.read().clone()).unwrap_or_default();
            (c.id, (c.name, st.width, st.height))
        })
        .collect();
    let events: Vec<Value> = rows
        .iter()
        .map(|e| {
            let (n, w, h) = names.get(&e.camera_id).cloned().unwrap_or_default();
            json!({"Event": event_json(e, &n, w, h)})
        })
        .collect();
    let page_count = total.div_ceil(limit).max(1);
    Ok(Json(json!({
        "events": events,
        "pagination": {"page": page, "current": rows.len(), "count": total, "prevPage": page > 1, "nextPage": page < page_count,
                        "pageCount": page_count, "order": {"Event.StartDateTime": if asc { "asc" } else { "desc" }}, "limit": limit, "options": {}, "paramType": "named", "queryScope": null}
    }))
    .into_response())
}

fn parse_id(s: &str) -> Option<i64> {
    s.trim_end_matches(".json").parse().ok()
}

async fn event_one(State(app): State<App>, Path(id): Path<String>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let id = parse_id(&id).ok_or_else(|| err(StatusCode::NOT_FOUND, "Invalid event"))?;
    let e = app.db.event(id).map_err(e500)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "Invalid event"))?;
    if !crate::api::visible_cameras_pub(&app, u).contains(&e.camera_id) {
        return Err(err(StatusCode::NOT_FOUND, "Invalid event"));
    }
    let cam = app.db.camera(e.camera_id).map_err(e500)?;
    let st = app.hub.get(e.camera_id).map(|h| h.status.read().clone()).unwrap_or_default();
    Ok(Json(json!({"event": {"Event": event_json(&e, &cam.map(|c| c.name).unwrap_or_default(), st.width, st.height), "Frame": []}})).into_response())
}

async fn event_put(State(app): State<App>, Path(id): Path<String>, user: Option<axum::Extension<AuthUser>>, Form(f): Form<HashMap<String, String>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let id = parse_id(&id).ok_or_else(|| err(StatusCode::NOT_FOUND, "Invalid event"))?;
    let e = app.db.event(id).map_err(e500)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "Invalid event"))?;
    if !crate::api::visible_cameras_pub(&app, u).contains(&e.camera_id) {
        return Err(err(StatusCode::NOT_FOUND, "Invalid event"));
    }
    if let Some(a) = f.get("Event[Archived]") {
        app.db.set_event_archived(id, a == "1").map_err(e500)?;
    }
    if let Some(n) = f.get("Event[Notes]") {
        app.db.set_event_notes(id, n).map_err(e500)?;
    }
    Ok(saved().await)
}

async fn event_delete(State(app): State<App>, Path(id): Path<String>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    if u.role != "admin" {
        // viewers may archive and annotate; deleting footage metadata is an admin act
        return Err(err(StatusCode::FORBIDDEN, "Insufficient privileges"));
    }
    let id = parse_id(&id).ok_or_else(|| err(StatusCode::NOT_FOUND, "Invalid event"))?;
    let e = app.db.event(id).map_err(e500)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "Invalid event"))?;
    if !crate::api::visible_cameras_pub(&app, u).contains(&e.camera_id) {
        return Err(err(StatusCode::NOT_FOUND, "Invalid event"));
    }
    app.db.delete_event(id).map_err(e500)?;
    Ok(saved().await)
}

async fn console_events(State(app): State<App>, Path(interval): Path<String>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let interval = interval.trim_end_matches(".json").to_lowercase();
    let secs: i64 = if interval.contains("day") { 86_400 } else if interval.contains("week") { 7 * 86_400 } else { 3600 };
    let since = crate::db::now_dts() - secs * TIMESCALE as i64;
    let mut results = serde_json::Map::new();
    for cam in crate::api::visible_cameras_pub(&app, u) {
        let n = app.db.events_in_range(cam, since, i64::MAX).map_err(e500)?.len();
        results.insert(cam.to_string(), json!(n));
    }
    Ok(Json(json!({"results": results})).into_response())
}

// ---------------------------------------------------------------------------
// notifications (ZM 1.39.2 shape): store push tokens; delivery needs an FCM relay
// ---------------------------------------------------------------------------

async fn notifications_get(State(app): State<App>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let rows = app.db.push_tokens(u.id).map_err(e500)?;
    Ok(Json(json!({"notifications": rows.into_iter().map(|r| json!({"Notification": r})).collect::<Vec<_>>()})).into_response())
}
async fn notifications_post(State(app): State<App>, user: Option<axum::Extension<AuthUser>>, Form(f): Form<HashMap<String, String>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let g = |k: &str| f.get(&format!("Notification[{k}]")).cloned().unwrap_or_default();
    let id = app
        .db
        .upsert_push_token(u.id, &g("Token"), &g("Platform"), &g("MonitorList"), g("Interval").parse().unwrap_or(0), &g("PushState"), &g("AppVersion"), &g("Profile"))
        .map_err(e500)?;
    let row = app.db.push_tokens(u.id).map_err(e500)?.into_iter().find(|r| r["Id"] == json!(id.to_string()));
    Ok(Json(json!({"notification": {"Notification": row}})).into_response())
}
async fn notifications_put(State(app): State<App>, Path(id): Path<String>, user: Option<axum::Extension<AuthUser>>, Form(f): Form<HashMap<String, String>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let id = parse_id(&id).ok_or_else(|| err(StatusCode::NOT_FOUND, "Invalid"))?;
    let g = |k: &str| f.get(&format!("Notification[{k}]")).cloned();
    app.db.update_push_token(u.id, id, g("MonitorList"), g("Interval").and_then(|v| v.parse().ok()), g("PushState"), g("Profile")).map_err(e500)?;
    Ok(saved().await)
}
async fn notifications_delete(State(app): State<App>, Path(id): Path<String>, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let id = parse_id(&id).ok_or_else(|| err(StatusCode::NOT_FOUND, "Invalid"))?;
    app.db.delete_push_token(u.id, id).map_err(e500)?;
    Ok(saved().await)
}

// ---------------------------------------------------------------------------
// index.php media views
// ---------------------------------------------------------------------------

async fn index_php(State(app): State<App>, RawQuery(raw): RawQuery, headers: HeaderMap, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let q: HashMap<String, String> = url::form_urlencoded::parse(raw.unwrap_or_default().as_bytes()).into_owned().collect();
    let view = q.get("view").map(|s| s.as_str()).unwrap_or("");
    match view {
        "image" => {
            let eid: i64 = q.get("eid").and_then(|s| s.parse().ok()).ok_or_else(|| err(StatusCode::BAD_REQUEST, "eid required"))?;
            let e = app.db.event(eid).map_err(e500)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such event"))?;
            if !crate::api::visible_cameras_pub(&app, u).contains(&e.camera_id) {
                return Err(err(StatusCode::NOT_FOUND, "no such event"));
            }
            let width: u32 = q.get("width").and_then(|s| s.parse().ok()).unwrap_or(640).clamp(64, 3840);
            let fid = q.get("fid").map(|s| s.as_str()).unwrap_or("snapshot");
            // snapshot/alarm/objdetect -> stored thumbnail; numeric -> frame at that offset
            if let Ok(n) = fid.parse::<i64>() {
                let t = e.start_dts + (n.max(1) - 1) * TIMESCALE as i64 / 18;
                return crate::api::frame_jpeg_pub(&app, e.camera_id, t, width).await;
            }
            if fid == "objdetect" {
                return objdetect_jpeg(&app, &e, width).await;
            }
            if let Some(rel) = &e.thumb_path {
                if width <= 640 {
                    if let Ok(data) = tokio::fs::read(app.cfg.thumb_dir.join(rel)).await {
                        return Ok(([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "private, max-age=86400")], data).into_response());
                    }
                }
            }
            crate::api::frame_jpeg_pub(&app, e.camera_id, e.peak_dts.unwrap_or(e.start_dts), width).await
        }
        "view_video" => {
            let eid: i64 = q.get("eid").and_then(|s| s.parse().ok()).ok_or_else(|| err(StatusCode::BAD_REQUEST, "eid required"))?;
            let e = app.db.event(eid).map_err(e500)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such event"))?;
            if !crate::api::visible_cameras_pub(&app, u).contains(&e.camera_id) {
                return Err(err(StatusCode::NOT_FOUND, "no such event"));
            }
            let end = e.end_dts.unwrap_or(e.start_dts + 60 * TIMESCALE as i64);
            serve_event_mp4(&app, e.camera_id, e.start_dts, end, &headers, eid).await
        }
        "view_event_hls" => {
            let eid: i64 = q.get("eid").and_then(|s| s.parse().ok()).ok_or_else(|| err(StatusCode::BAD_REQUEST, "eid required"))?;
            let e = app.db.event(eid).map_err(e500)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such event"))?;
            if !crate::api::visible_cameras_pub(&app, u).contains(&e.camera_id) {
                return Err(err(StatusCode::NOT_FOUND, "no such event"));
            }
            let end = e.end_dts.unwrap_or(e.start_dts + 60 * TIMESCALE as i64);
            let tok = q.get("token").cloned().unwrap_or_default();
            let m3u8 = video::hls_playlist(&app.db, e.camera_id, "main", e.start_dts, end,
                |sid| format!("/api/segments/{sid}/file.mp4?token={tok}"),
                |sid, fi| format!("/api/segments/{sid}/frag/{fi}/seg.m4s?token={tok}"),
                |sid| format!("/api/segments/{sid}/init.mp4?token={tok}"))
                .map_err(e500)?;
            Ok(([(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")], m3u8).into_response())
        }
        "request" => {
            if q.get("request").map(|s| s.as_str()) == Some("control") {
                return zm_control(&app, u, &q).await;
            }
            // stream control (pause/quit/query): quit ends that MJPEG stream,
            // the rest are acknowledged
            let cmd = q.get("command").map(|s| s.as_str()).unwrap_or("");
            if cmd == "17" {
                if let Some(stop) = q.get("connkey").and_then(|k| app.mjpeg_streams.lock().remove(k)) {
                    let _ = stop.send(());
                }
            }
            if cmd == "99" {
                return Ok(Json(json!({"result": "Ok", "status": {"progress": 0, "duration": 0, "rate": 1, "zoom": 1, "paused": 0, "delayed": 0}})).into_response());
            }
            Ok(Json(json!({"result": "Ok"})).into_response())
        }
        _ => Err(err(StatusCode::NOT_FOUND, "unsupported view")),
    }
}

/// ZoneMinder's `objdetect.jpg`: the event picture (the stored thumbnail,
/// or the main-stream frame at the peak when a larger one is asked for) with
/// the detected objects' boxes drawn on it. 404 when nothing was detected,
/// as ZoneMinder has no such file then (the app skips that frame).
async fn objdetect_jpeg(app: &App, e: &Event, width: u32) -> ApiResult {
    let objs = crate::notify::objects_from_meta(e);
    if objs.is_empty() {
        return Err(err(StatusCode::NOT_FOUND, "no objects detected in this event"));
    }
    let stored = match (&e.thumb_path, width <= 640) {
        (Some(rel), true) => tokio::fs::read(app.cfg.thumb_dir.join(rel)).await.ok(),
        _ => None,
    };
    let jpeg = match stored {
        Some(j) => j,
        None => {
            let res = crate::api::frame_jpeg_pub(app, e.camera_id, e.peak_dts.unwrap_or(e.start_dts), width).await?;
            http_body_util::BodyExt::collect(res.into_body()).await.map_err(e500)?.to_bytes().to_vec()
        }
    };
    let out = tokio::task::spawn_blocking(move || crate::objects::draw_boxes(&jpeg, &objs)).await.map_err(e500)?.map_err(e500)?;
    Ok(([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "private, max-age=300")], out).into_response())
}

/// ZoneMinder control command name -> (pan, tilt, zoom) velocity, or None
/// for stop. Presets are handled separately.
pub fn zm_control_velocity(control: &str) -> Option<Option<(f64, f64, f64)>> {
    let v = match control {
        "moveConUp" => (0.0, 1.0, 0.0),
        "moveConDown" => (0.0, -1.0, 0.0),
        "moveConLeft" => (-1.0, 0.0, 0.0),
        "moveConRight" => (1.0, 0.0, 0.0),
        "moveConUpLeft" => (-1.0, 1.0, 0.0),
        "moveConUpRight" => (1.0, 1.0, 0.0),
        "moveConDownLeft" => (-1.0, -1.0, 0.0),
        "moveConDownRight" => (1.0, -1.0, 0.0),
        "zoomConTele" => (0.0, 0.0, 1.0),
        "zoomConWide" => (0.0, 0.0, -1.0),
        "moveStop" | "zoomStop" => return Some(None),
        _ => return None,
    };
    Some(Some(v))
}

/// `index.php?view=request&request=control&id=<mid>&control=<cmd>[&preset=N]`
async fn zm_control(app: &App, u: &User, q: &HashMap<String, String>) -> ApiResult {
    if u.role != "admin" {
        return Err(err(StatusCode::FORBIDDEN, "Insufficient privileges"));
    }
    let mid: i64 = q.get("id").and_then(|s| s.parse().ok()).ok_or_else(|| err(StatusCode::BAD_REQUEST, "id required"))?;
    let cam = app.db.camera(mid).map_err(e500)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "Invalid monitor"))?;
    if !cam.ptz || cam.ptz_url.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "monitor is not controllable"));
    }
    let (user, pass) = crate::onvif::credentials(&cam);
    let c = crate::onvif::Client::new(&user, &pass).map_err(e500)?;
    let control = q.get("control").map(|s| s.as_str()).unwrap_or("");
    let res = match control {
        "presetGoto" => {
            let p = q.get("preset").cloned().unwrap_or_default();
            c.goto_preset(&cam.ptz_url, &cam.ptz_profile, &p).await
        }
        "presetHome" => c.goto_home(&cam.ptz_url, &cam.ptz_profile).await,
        other => match zm_control_velocity(other) {
            Some(Some((p, t, z))) => c.continuous_move(&cam.ptz_url, &cam.ptz_profile, p, t, z).await,
            Some(None) => c.stop(&cam.ptz_url, &cam.ptz_profile).await,
            None => return Err(err(StatusCode::BAD_REQUEST, "unsupported control")),
        },
    };
    res.map_err(|e| err(StatusCode::BAD_GATEWAY, &format!("camera: {e:#}")))?;
    Ok(Json(json!({"result": "Ok"})).into_response())
}

/// `/zm/api/controls/1.json`: one generic continuous-move PTZ control.
async fn control_one(Path(_id): Path<String>) -> Response {
    Json(json!({"control": {"Control": {
        "Id": "1", "Name": "ONVIF", "Type": "Remote", "Protocol": "onvif", "CanWake": "0", "CanSleep": "0", "CanReset": "0",
        "CanZoom": "1", "CanAutoZoom": "0", "CanZoomAbs": "0", "CanZoomRel": "0", "CanZoomCon": "1", "MinZoomRange": null, "MaxZoomRange": null,
        "CanFocus": "0", "CanIris": "0", "CanGain": "0", "CanWhite": "0",
        "CanMove": "1", "CanMoveDiag": "1", "CanMoveMap": "0", "CanMoveAbs": "0", "CanMoveRel": "0", "CanMoveCon": "1",
        "CanPan": "1", "CanTilt": "1", "HasPresets": "1", "NumPresets": "8", "HasHomePreset": "1", "CanSetPresets": "0"
    }}})).into_response()
}

/// A complete MP4 (Content-Length + Accept-Ranges) cut from the recording.
async fn serve_event_mp4(app: &App, cam: i64, start: i64, end: i64, headers: &HeaderMap, eid: i64) -> ApiResult {
    let mut plan = video::plan_range(&app.db, cam, start, end).map_err(e500)?;
    if plan.items.is_empty() {
        return Err(err(StatusCode::NOT_FOUND, "no recording for this event"));
    }
    // a downloadable file must start at t=0 for ordinary players
    plan.tfdt_shift = -plan.first_dts.unwrap_or(start);
    let mime = plan.mime.clone().unwrap_or_else(|| "video/mp4".into());
    if !plan.exact_len {
        // legacy fragments are rewritten while streaming; no byte ranges possible
        return Ok(Response::builder()
            .header(header::CONTENT_TYPE, mime)
            .header(header::ACCEPT_RANGES, "none")
            .body(Body::from_stream(video::stream_plan(plan)))
            .unwrap());
    }
    let total = plan.bytes;
    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok()).and_then(crate::api::parse_range_pub);
    let (s, e) = match range {
        Some((s, e)) => (s, e.map(|e| e.min(total - 1)).unwrap_or(total - 1)),
        None => (0, total - 1),
    };
    if s > e || s >= total {
        return Err(err(StatusCode::RANGE_NOT_SATISFIABLE, "bad range"));
    }
    let len = e - s + 1;
    // cap a single response at 64 MB; clients continue with the next range
    let e = if len > 64 << 20 { s + (64 << 20) - 1 } else { e };
    let len = e - s + 1;
    let data = tokio::task::spawn_blocking(move || read_plan_bytes(&plan, s, len)).await.map_err(e500)?.map_err(e500)?;
    let mut b = Response::builder()
        .status(if range.is_some() { StatusCode::PARTIAL_CONTENT } else { StatusCode::OK })
        .header(header::CONTENT_TYPE, mime)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, len)
        .header(header::CONTENT_DISPOSITION, format!("inline; filename=\"event-{eid}.mp4\""));
    if range.is_some() {
        b = b.header(header::CONTENT_RANGE, format!("bytes {s}-{e}/{total}"));
    }
    Ok(b.body(Body::from(data)).unwrap())
}

/// Read bytes [offset, offset+len) of the virtual file a plan describes.
fn read_plan_bytes(plan: &video::RangePlan, offset: u64, len: u64) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(len as usize);
    let mut pos = 0u64;
    let end = offset + len;
    let mut open: Option<(std::path::PathBuf, std::fs::File)> = None;
    for item in &plan.items {
        let (ilen, src): (u64, Option<(&std::path::PathBuf, u64)>) = match item {
            video::RangeItem::Init(b) => (b.len() as u64, None),
            video::RangeItem::Frag { path, offset: o, len: l, .. } => (*l as u64, Some((path, *o))),
        };
        let (frag_shift, audio_rate) = match item {
            video::RangeItem::Frag { dts_offset, audio_rate, .. } => (dts_offset + plan.tfdt_shift, *audio_rate),
            _ => (0, None),
        };
        let a = pos.max(offset);
        let b = (pos + ilen).min(end);
        if a < b {
            let (from, to) = (a - pos, b - pos);
            match item {
                video::RangeItem::Init(bytes) => out.extend_from_slice(&bytes[from as usize..to as usize]),
                video::RangeItem::Frag { .. } => {
                    let (path, o) = src.unwrap();
                    if open.as_ref().map(|(p, _)| p != path).unwrap_or(true) {
                        open = Some((path.clone(), std::fs::File::open(path)?));
                    }
                    let f = &mut open.as_mut().unwrap().1;
                    // read the whole fragment so its tfdt can be shifted, then slice
                    f.seek(SeekFrom::Start(o))?;
                    let mut buf = vec![0u8; ilen as usize];
                    f.read_exact(&mut buf)?;
                    if frag_shift != 0 {
                        crate::mp4::shift_tfdt_av(&mut buf, frag_shift, audio_rate);
                    }
                    out.extend_from_slice(&buf[from as usize..to as usize]);
                }
            }
        }
        pos += ilen;
        if pos >= end {
            break;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// nph-zms: MJPEG live / single JPEG, produced by ffmpeg from the substream
// recorder's live fMP4 (one ffmpeg per viewer, 640x360 -> cheap)
// ---------------------------------------------------------------------------

async fn nph_zms(State(app): State<App>, RawQuery(raw): RawQuery, user: Option<axum::Extension<AuthUser>>) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let q: HashMap<String, String> = url::form_urlencoded::parse(raw.unwrap_or_default().as_bytes()).into_owned().collect();
    if q.get("source").map(|s| s == "event").unwrap_or(false) {
        return Err(err(StatusCode::NOT_FOUND, "event streaming via zms is not supported; use the MP4"));
    }
    let mid: i64 = q.get("monitor").and_then(|s| s.parse().ok()).ok_or_else(|| err(StatusCode::BAD_REQUEST, "monitor required"))?;
    if !crate::api::visible_cameras_pub(&app, u).contains(&mid) {
        return Err(err(StatusCode::NOT_FOUND, "no such monitor"));
    }
    let scale: u32 = q.get("scale").and_then(|s| s.parse().ok()).unwrap_or(100).clamp(5, 100);
    let mode = q.get("mode").map(|s| s.as_str()).unwrap_or("jpeg");
    let single = mode == "single" || q.get("frames").map(|f| f == "1").unwrap_or(false);
    let maxfps: f32 = q.get("maxfps").and_then(|s| s.parse::<f32>().ok()).unwrap_or(5.0).clamp(0.5, 15.0);
    // ZoneMinder's scale is relative to the monitor's native size; `width`
    // wins when given. The substream serves anything up to its own width
    // (640 px), so a tile never costs a 4 MP HEVC decode.
    let st = app.hub.get(mid).map(|h| h.status.read().clone()).unwrap_or_default();
    let width = q.get("width").and_then(|s| s.parse::<u32>().ok()).unwrap_or((st.width.max(640) * scale / 100).max(160)).clamp(64, 3840);
    let sub_w = app.hub.get_stream(mid, crate::recorder::StreamKind::Sub).and_then(|h| h.recent.read().as_ref().map(|r| r.params.width));
    if single {
        let (jpeg, _) = crate::api::live_jpeg(&app, mid, Some(sub_w.map(|w| width.min(w)).unwrap_or(width)), false).await?;
        return Ok(([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "no-store")], jpeg).into_response());
    }
    // MJPEG from the substream recorder (fall back to main if no substream)
    let h = app
        .hub
        .get_stream(mid, crate::recorder::StreamKind::Sub)
        .or_else(|| app.hub.get(mid))
        .ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "monitor not running"))?;
    let (init, first, src_w) = {
        let r = h.recent.read();
        let r = r.as_ref().ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "no frames yet"))?;
        let f = r.frags.back().ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "no frames yet"))?;
        (r.init.clone(), f.2.clone(), r.params.width)
    };
    let width = width.min(src_w.max(160)).min(1280);
    // a client reusing its connkey replaces its old stream (and frees its slot)
    let connkey = q.get("connkey").filter(|k| !k.is_empty()).cloned();
    if let Some(old) = connkey.as_ref().and_then(|k| app.mjpeg_streams.lock().remove(k)) {
        let _ = old.send(());
    }
    // fail fast: a montage that opens more streams than the pool gets a 503
    // it can retry, not a hung request
    let _permit = app.mjpeg_sem.clone().try_acquire_owned().map_err(|_| {
        (StatusCode::SERVICE_UNAVAILABLE, [(header::RETRY_AFTER, "2")], "too many MJPEG streams; use the web UI or zmNinja's substream").into_response()
    })?;
    let mut child = crate::thumbs::ffmpeg_command(&app.cfg.ffmpeg)
        .args([
            "-nostdin", "-loglevel", "error", "-threads", "1", "-fflags", "nobuffer", "-flags", "low_delay",
            "-probesize", "65536", "-analyzeduration", "0", "-f", "mp4", "-i", "pipe:0", "-an", "-threads", "1", "-filter_threads", "1",
            "-vf", &format!("fps={maxfps},scale={width}:-2"),
            "-q:v", "6", "-f", "mpjpeg", "-boundary_tag", "ZoneMinderFrame", "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(e500)?;
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut rx = h.live.subscribe();
    tokio::spawn(async move {
        if stdin.write_all(&init).await.is_err() || stdin.write_all(&first).await.is_err() {
            return;
        }
        loop {
            match rx.recv().await {
                Ok(f) => {
                    if f.init != init || stdin.write_all(&f.data).await.is_err() {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            }
        }
    });
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    // with a connkey the registry holds the sender (quit = send); without one
    // the stream keeps it, or the dropped sender would end the stream at once
    let (registered, unregistered) = match connkey {
        Some(k) => {
            app.mjpeg_streams.lock().insert(k.clone(), stop_tx);
            (Some(StreamGuard { key: k, streams: app.mjpeg_streams.clone() }), None)
        }
        None => (None, Some(stop_tx)),
    };
    let permit = _permit;
    let stream = futures::StreamExt::take_until(tokio_util::io::ReaderStream::new(stdout), stop_rx);
    let stream = futures::StreamExt::map(stream, move |chunk| {
        // released (ffmpeg killed, slot freed) when the client disconnects or quits
        let _keep = (&permit, &child, &registered, &unregistered);
        chunk
    });
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "multipart/x-mixed-replace; boundary=ZoneMinderFrame")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(stream))
        .unwrap())
}

/// Forgets a stream's `connkey` when its response is dropped.
struct StreamGuard {
    key: String,
    streams: std::sync::Arc<parking_lot::Mutex<HashMap<String, tokio::sync::oneshot::Sender<()>>>>,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        let mut m = self.streams.lock();
        // only our own entry: a newer stream may have taken the key over
        if m.get(&self.key).is_some_and(|tx| tx.is_closed()) {
            m.remove(&self.key);
        }
    }
}

// ---------------------------------------------------------------------------
// go2rtc-compatible live stream (MSE over a websocket) for zmNinjaNg
//
// zmNinjaNg plays live video through go2rtc's `video-rtc` element when a
// monitor has `Go2RTCEnabled` and `ZM_GO2RTC_PATH` is set; otherwise it falls
// back to MJPEG from nph-zms. We speak the part of go2rtc's websocket
// protocol that MSE needs: the client sends `{"type":"mse","value":<codecs
// it can play>}`, we answer `{"type":"mse","value":<mime>}` and then send the
// init segment and every fragment as binary messages. WebRTC/HLS requests
// (the client races WebRTC against MSE) get an `error`, which makes it keep
// the MSE picture.
// ---------------------------------------------------------------------------

/// `ZM_GO2RTC_PATH`: an external go2rtc when configured, else this server's
/// own endpoint as the client reached it (the client appends `/ws`).
fn go2rtc_path(app: &App, headers: &HeaderMap) -> Option<String> {
    if let Some(u) = &app.cfg.go2rtc_url {
        return Some(u.clone());
    }
    if !app.cfg.zmninja_mse {
        return None;
    }
    let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).map(|v| v.split(',').next().unwrap_or("").trim().to_string()).filter(|v| !v.is_empty());
    let proto = h("x-forwarded-proto").unwrap_or_else(|| if app.cfg.secure_cookies { "https".into() } else { "http".into() });
    let host = h("x-forwarded-host").or_else(|| h("host"))?;
    Some(format!("{proto}://{host}/zm/go2rtc"))
}

/// Camera and stream a go2rtc stream name means: `<id>` and
/// `<id>_CameraDirectPrimary` = main, `<id>_CameraDirectSecondary` = substream.
pub fn parse_go2rtc_src(src: &str) -> Option<(i64, bool)> {
    let (id, suffix) = src.split_once('_').unwrap_or((src, ""));
    Some((id.parse().ok()?, suffix == "CameraDirectSecondary"))
}

fn codec_family(c: &str) -> &str {
    match c.trim().split('.').next().unwrap_or("") {
        "hvc1" | "hev1" => "hevc",
        "avc1" | "avc3" => "avc",
        other => other,
    }
}

/// Whether a client that listed `accepted` (go2rtc's comma-separated codec
/// list) can play a stream with these parameters through MSE.
pub fn client_accepts(accepted: &str, params: &crate::mp4::VideoParams) -> bool {
    let fams: Vec<&str> = accepted.split(',').map(codec_family).collect();
    fams.contains(&codec_family(&params.rfc6381)) && params.audio.as_ref().is_none_or(|a| fams.contains(&codec_family(&a.rfc6381)))
}

#[derive(Deserialize)]
struct Go2rtcQ {
    src: Option<String>,
}

async fn go2rtc_ws(State(app): State<App>, Query(q): Query<Go2rtcQ>, user: Option<axum::Extension<AuthUser>>, ws: axum::extract::ws::WebSocketUpgrade) -> ApiResult {
    let u = require(user.as_ref().map(|e| &e.0))?;
    let (cam, sub) = q.src.as_deref().and_then(parse_go2rtc_src).ok_or_else(|| err(StatusCode::NOT_FOUND, "unknown stream"))?;
    if !crate::api::visible_cameras_pub(&app, u).contains(&cam) {
        return Err(err(StatusCode::NOT_FOUND, "unknown stream"));
    }
    Ok(ws.on_upgrade(move |socket| go2rtc_session(app, socket, cam, sub)))
}

fn go2rtc_msg(typ: &str, value: &str) -> axum::extract::ws::Message {
    axum::extract::ws::Message::Text(json!({"type": typ, "value": value}).to_string().into())
}

/// Answer for a request we do not serve (None = ignore the message).
fn go2rtc_decline(typ: &str) -> Option<axum::extract::ws::Message> {
    match typ {
        "webrtc/offer" | "webrtc" | "hls" | "mp4" | "mjpeg" => Some(go2rtc_msg("error", &format!("{typ}: not supported by zmng (MSE only)"))),
        _ => None,
    }
}

async fn go2rtc_session(app: App, mut socket: axum::extract::ws::WebSocket, cam: i64, prefer_sub: bool) {
    use axum::extract::ws::Message;
    use futures::StreamExt;
    loop {
        let m = match tokio::time::timeout(std::time::Duration::from_secs(30), socket.next()).await {
            Ok(Some(Ok(m))) => m,
            _ => return,
        };
        let v: Value = match m {
            Message::Text(t) => serde_json::from_str(&t).unwrap_or(Value::Null),
            Message::Ping(p) => {
                let _ = socket.send(Message::Pong(p)).await;
                continue;
            }
            Message::Close(_) => return,
            _ => continue,
        };
        let typ = v["type"].as_str().unwrap_or("");
        if typ != "mse" {
            if let Some(r) = go2rtc_decline(typ) {
                if socket.send(r).await.is_err() {
                    return;
                }
            }
            continue;
        }
        // the requested stream first, the other one if the client cannot decode it
        let accepted = v["value"].as_str().unwrap_or("");
        let order = if prefer_sub { [crate::recorder::StreamKind::Sub, crate::recorder::StreamKind::Main] } else { [crate::recorder::StreamKind::Main, crate::recorder::StreamKind::Sub] };
        let pick = order.into_iter().filter_map(|k| app.hub.get_stream(cam, k)).find(|h| h.recent.read().as_ref().is_some_and(|r| client_accepts(accepted, &r.params)));
        match pick {
            Some(h) => {
                debug!(camera = cam, "go2rtc-compatible MSE stream started");
                mse_stream(&mut socket, h).await;
                return;
            }
            None => {
                if socket.send(go2rtc_msg("error", "mse: no stream in a codec this client can play")).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// Stream one recorder as MSE: the mime, the init segment, the newest
/// fragment (starts on a keyframe now), then every fragment as it closes.
/// Timestamps are rebased so the session starts at 0 (go2rtc's player
/// never seeks, so absolute times would leave it waiting at t=0).
async fn mse_stream(socket: &mut axum::extract::ws::WebSocket, h: std::sync::Arc<crate::recorder::CamHandle>) {
    use axum::extract::ws::Message;
    use futures::StreamExt;
    let mut rx = h.live.subscribe();
    let Some((params, init, first)) = h.recent.read().as_ref().map(|r| (r.params.clone(), r.init.clone(), r.frags.back().map(|f| (f.0, f.2.clone())))) else { return };
    if socket.send(go2rtc_msg("mse", &params.mime())).await.is_err() || socket.send(Message::Binary(init.clone())).await.is_err() {
        return;
    }
    let audio_rate = params.audio.as_ref().map(|a| a.sample_rate);
    let mut base: Option<i64> = None;
    let mut last_dts = i64::MIN;
    let mut send_frag = |dts: i64, data: &[u8]| -> Option<Vec<Message>> {
        if dts <= last_dts {
            return Some(Vec::new()); // already sent (primed from `recent`)
        }
        last_dts = dts;
        let b = *base.get_or_insert(dts);
        let mut d = data.to_vec();
        crate::mp4::shift_tfdt_av(&mut d, -b, audio_rate);
        // go2rtc's player buffers at most 2 MB while its SourceBuffer is busy
        Some(d.chunks(512 * 1024).map(|c| Message::Binary(bytes::Bytes::copy_from_slice(c))).collect())
    };
    if let Some((dts, data)) = first {
        for m in send_frag(dts, &data).unwrap_or_default() {
            if socket.send(m).await.is_err() {
                return;
            }
        }
    }
    loop {
        tokio::select! {
            m = socket.next() => match m {
                Some(Ok(Message::Text(t))) => {
                    let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                    if let Some(r) = go2rtc_decline(v["type"].as_str().unwrap_or("")) {
                        if socket.send(r).await.is_err() { return; }
                    }
                }
                Some(Ok(Message::Ping(p))) => { if socket.send(Message::Pong(p)).await.is_err() { return; } }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                _ => {}
            },
            f = rx.recv() => match f {
                Ok(f) => {
                    // new parameters (camera reconnected with another resolution or
                    // codec): end the session; the player reconnects and renegotiates
                    if f.init != init {
                        let _ = socket.send(Message::Close(None)).await;
                        return;
                    }
                    for m in send_frag(f.dts, &f.data).unwrap_or_default() {
                        if socket.send(m).await.is_err() { return; }
                    }
                }
                // fell behind (slow phone link): a gap would stall MSE, so start over
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => { let _ = socket.send(Message::Close(None)).await; return; }
                Err(_) => return,
            },
        }
    }
}


// ---------------------------------------------------------------------------
// Event-server websocket (zmeventnotification protocol) for zmNinjaNg
// ---------------------------------------------------------------------------

const ES_VERSION: &str = "7.0.0";

/// Per-connection notification filter set by the client (`control/filter`).
#[derive(Default, Debug, Clone, PartialEq)]
pub struct EsFilter {
    /// monitor ids the client wants (empty = every visible monitor)
    pub monitors: Vec<i64>,
    /// per-monitor minimum seconds between alarms, parallel to `monitors`
    pub intervals: Vec<i64>,
}

impl EsFilter {
    pub fn parse(monlist: &str, intlist: &str) -> EsFilter {
        let monitors: Vec<i64> = monlist.split(',').filter_map(|s| s.trim().parse().ok()).collect();
        let mut intervals: Vec<i64> = intlist.split(',').filter_map(|s| s.trim().parse().ok()).collect();
        intervals.resize(monitors.len(), 0);
        EsFilter { monitors, intervals }
    }
    /// Minimum interval for a monitor (0 when unfiltered).
    pub fn interval(&self, monitor: i64) -> Option<i64> {
        if self.monitors.is_empty() {
            return Some(0);
        }
        self.monitors.iter().position(|m| *m == monitor).map(|i| self.intervals[i])
    }
}

/// The `alarm` message zmNinja parses.
pub fn es_alarm_json(e: &crate::notify::EventInfo) -> Value {
    let cause = if e.objects.is_empty() {
        "Motion".to_string()
    } else {
        format!("{} detected", e.objects.iter().map(|o| o.label.as_str()).collect::<Vec<_>>().join(", "))
    };
    let detection: Vec<Value> = e
        .objects
        .iter()
        .map(|o| json!({"label": o.label, "confidence": format!("{:.0}%", o.confidence * 100.0), "box": o.bbox}))
        .collect();
    json!({
        "event": "alarm", "type": "", "status": "Success",
        "events": [{
            "MonitorId": e.camera_id, "MonitorName": e.camera_name, "EventId": e.id,
            "Cause": cause, "Name": e.camera_name, "DetectionJson": detection,
        }]
    })
}

async fn es_ws(State(app): State<App>, ws: axum::extract::ws::WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| es_session(app, socket))
}

async fn es_session(app: App, mut socket: axum::extract::ws::WebSocket) {
    use axum::extract::ws::Message;
    use futures::StreamExt;
    async fn send(socket: &mut axum::extract::ws::WebSocket, v: Value) -> bool {
        socket.send(Message::Text(v.to_string().into())).await.is_ok()
    }
    // 1. authenticate within 20 s
    let auth = tokio::time::timeout(std::time::Duration::from_secs(20), socket.next()).await;
    let msg: Value = match auth {
        Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str(&t).unwrap_or(Value::Null),
        _ => return,
    };
    let user = if msg["event"] == "auth" {
        let (u, p) = (msg["data"]["user"].as_str().unwrap_or(""), msg["data"]["password"].as_str().unwrap_or(""));
        if !app.login_guard.lock().allowed() {
            let _ = send(&mut socket, json!({"event": "auth", "type": "", "status": "Fail", "reason": "BADAUTH"})).await;
            return;
        }
        let user = app.db.user_by_name(u).ok().flatten();
        let ok = user.as_ref().map(|x| crate::api::verify_password_pub(p, &x.pass_hash)).unwrap_or(false);
        if !ok {
            hash_password_dummy();
            app.login_guard.lock().record_failure();
            let _ = send(&mut socket, json!({"event": "auth", "type": "", "status": "Fail", "reason": "BADAUTH"})).await;
            return;
        }
        user.unwrap()
    } else {
        let _ = send(&mut socket, json!({"event": "auth", "type": "", "status": "Fail", "reason": "NOAUTH"})).await;
        return;
    };
    if !send(&mut socket, json!({"event": "auth", "type": "", "status": "Success", "reason": "", "version": ES_VERSION})).await {
        return;
    }
    info!(user = %user.username, "event-server websocket connected");
    let mut filter = EsFilter::default();
    let mut last_sent: HashMap<i64, i64> = HashMap::new();
    let mut rx = app.bus.subscribe();
    loop {
        tokio::select! {
            m = socket.next() => {
                let v: Value = match m {
                    Some(Ok(Message::Text(t))) => serde_json::from_str(&t).unwrap_or(Value::Null),
                    Some(Ok(Message::Ping(p))) => { let _ = socket.send(Message::Pong(p)).await; continue; }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                    _ => continue,
                };
                let typ = v["data"]["type"].as_str().unwrap_or("");
                let reply: Option<Value> = match (v["event"].as_str().unwrap_or(""), typ) {
                    ("control", "version") => Some(json!({"event": "control", "type": "version", "status": "Success", "reason": "", "version": ES_VERSION})),
                    ("control", "filter") => {
                        filter = EsFilter::parse(v["data"]["monlist"].as_str().unwrap_or(""), v["data"]["intlist"].as_str().unwrap_or(""));
                        Some(json!({"event": "control", "type": "filter", "status": "Success", "reason": ""}))
                    }
                    ("push", "token") => {
                        let d = &v["data"];
                        let g = |k: &str| d[k].as_str().unwrap_or("").to_string();
                        let res = app.db.upsert_push_token(user.id, &g("token"), &g("platform"), &g("monlist"), 0, &g("state"), &g("appversion"), &g("profile"));
                        let (status, reason) = match res { Ok(_) => ("Success", String::new()), Err(e) => ("Fail", e.to_string()) };
                        Some(json!({"event": "push", "type": "token", "status": status, "reason": reason}))
                    }
                    ("push", _) => Some(json!({"event": "push", "type": typ, "status": "Success", "reason": ""})),
                    _ => None,
                };
                if let Some(r) = reply {
                    if !send(&mut socket, r).await { return; }
                }
            }
            n = rx.recv() => {
                let n = match n {
                    Ok(n) => n,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                };
                let crate::notify::Notification::EventStart(e) = n else { continue };
                if !crate::api::visible_cameras_pub(&app, &user).contains(&e.camera_id) {
                    continue;
                }
                let Some(min_secs) = filter.interval(e.camera_id) else { continue };
                let now = crate::db::now_secs();
                if last_sent.get(&e.camera_id).is_some_and(|t| now - t < min_secs) {
                    continue;
                }
                last_sent.insert(e.camera_id, now);
                if !send(&mut socket, es_alarm_json(&e)).await { return; }
            }
        }
    }
}

// ---------------------------------------------------------------------------

pub fn router() -> Router<App> {
    Router::new()
        .route("/zm/api/host/getVersion.json", get(get_version))
        .route("/zm/api/host/login.json", post(login).get(get_version))
        .route("/zm/api/host/getTimeZone.json", get(get_timezone))
        .route("/zm/api/host/daemonCheck.json", get(daemon_check))
        .route("/zm/api/host/getLoad.json", get(get_load))
        .route("/zm/api/host/getDiskPercent.json", get(get_disk_percent))
        .route("/zm/api/configs/viewByName/{name}", get(config_by_name))
        .route("/zm/api/servers.json", get(|| async { Json(json!({"servers": []})).into_response() }))
        .route("/zm/api/states.json", get(|| async { Json(json!({"states": []})).into_response() }))
        .route("/zm/api/states/change/{*rest}", post(state_change))
        .route("/zm/api/groups.json", get(groups))
        .route("/zm/api/tags.json", get(not_found))
        .route("/zm/api/tags/{*rest}", get(not_found))
        .route("/zm/api/zones.json", get(zones))
        .route("/zm/api/users.json", get(users))
        .route("/zm/api/storage.json", get(storage_list))
        .route("/zm/api/monitors.json", get(monitors))
        .route("/zm/api/monitors/alarm/{*rest}", get(monitor_alarm))
        .route("/zm/api/monitors/daemonStatus/{*rest}", get(monitor_daemon))
        .route("/zm/api/monitors/{id}", get(monitor_one).post(monitor_post))
        .route("/zm/api/controls/{id}", get(control_one))
        .route("/zm/api/events/index.json", get(events_index))
        .route("/zm/api/events/index/{*filters}", get(events_index))
        .route("/zm/api/events/consoleEvents/{interval}", get(console_events))
        .route("/zm/api/events/{id}", get(event_one).put(event_put).delete(event_delete))
        .route("/zm/api/notifications.json", get(notifications_get).post(notifications_post))
        .route("/zm/api/notifications/{id}", axum::routing::put(notifications_put).delete(notifications_delete))
        .route("/zm/api/{*rest}", get(empty_list))
        .route("/zm/index.php", get(index_php))
        .route("/zm/cgi-bin/nph-zms", get(nph_zms))
        .route("/zm/go2rtc/ws", get(go2rtc_ws))
        .route("/zm/go2rtc/api/ws", get(go2rtc_ws))
        .route("/zm/ws", get(es_ws))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_grammar() {
        let f = parse_filter("MonitorId:3/MonitorId:5/StartDateTime >=:2026-09-27 10:00:00/AlarmFrames >=:1/Archived:1/Id IN:1,2,3.json");
        assert_eq!(f.monitors, vec![3, 5]);
        assert!(f.start_ge.is_some());
        assert_eq!(f.min_score, 1);
        assert_eq!(f.archived, Some(true));
        assert_eq!(f.ids, vec![1, 2, 3]);
        let g = parse_filter("index.json");
        assert!(g.monitors.is_empty());
    }

    #[test]
    fn es_filter_and_alarm() {
        let f = EsFilter::parse("1,2", "0,60");
        assert_eq!(f.interval(1), Some(0));
        assert_eq!(f.interval(2), Some(60));
        assert_eq!(f.interval(3), None);
        assert_eq!(EsFilter::default().interval(9), Some(0));
        let short = EsFilter::parse("4,5", "30");
        assert_eq!(short.interval(5), Some(0));
        let e = crate::notify::EventInfo { id: 12, camera_id: 3, camera_name: "Front".into(), start_ms: 0, end_ms: None, score: 1, kind: "person".into(), thumb: None,
            objects: vec![crate::notify::DetectedObject { label: "person".into(), confidence: 0.91, bbox: [0.1, 0.1, 0.5, 0.9] }] };
        let a = es_alarm_json(&e);
        assert_eq!(a["event"], "alarm");
        assert_eq!(a["events"][0]["MonitorId"], 3);
        assert_eq!(a["events"][0]["EventId"], 12);
        assert_eq!(a["events"][0]["Cause"], "person detected");
        assert_eq!(a["events"][0]["DetectionJson"][0]["confidence"], "91%");
        let plain = crate::notify::EventInfo { objects: vec![], ..e };
        assert_eq!(es_alarm_json(&plain)["events"][0]["Cause"], "Motion");
    }

    #[test]
    fn go2rtc_stream_names_and_codecs() {
        assert_eq!(parse_go2rtc_src("7"), Some((7, false)));
        assert_eq!(parse_go2rtc_src("7_CameraDirectPrimary"), Some((7, false)));
        assert_eq!(parse_go2rtc_src("12_CameraDirectSecondary"), Some((12, true)));
        assert_eq!(parse_go2rtc_src("x_CameraDirectSecondary"), None);
        let p = |v: &str, a: Option<&str>| crate::mp4::VideoParams {
            codec: if v.starts_with("avc") { crate::mp4::Codec::H264 } else { crate::mp4::Codec::H265 },
            width: 640,
            height: 360,
            sample_entry: bytes::Bytes::new(),
            rfc6381: v.into(),
            audio: a.map(|a| crate::mp4::AudioParams { sample_entry: bytes::Bytes::new(), sample_rate: 16000, channels: 1, rfc6381: a.into() }),
        };
        let chrome = "avc1.640029,avc1.64002A,avc1.640033,mp4a.40.2,mp4a.40.5,opus";
        assert!(client_accepts(chrome, &p("avc1.4D401E", None)));
        assert!(!client_accepts(chrome, &p("hvc1.1.6.L150.80", None)));
        assert!(client_accepts("hvc1.1.6.L153.B0", &p("hev1.1.6.L150.90", None)));
        assert!(client_accepts(chrome, &p("avc1.4D401E", Some("mp4a.40.2"))));
        assert!(!client_accepts("avc1.640029", &p("avc1.4D401E", Some("mp4a.40.2"))), "audio the client cannot play");
    }

    #[test]
    fn zm_time_roundtrip() {
        let dts = 1_790_000_000i64 * TIMESCALE as i64;
        let s = zm_time(dts);
        assert_eq!(parse_zm_time(&s), Some(dts));
    }

}
