//! Operability: the status report a volunteer can read, Prometheus metrics,
//! `zmng doctor`, and index backup/restore.
//!
//! Everything here is derived from state that already exists (recorder and
//! detector status, the segment index) with a handful of indexed queries; no
//! per-frame data, no filesystem walks.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::Path;
use std::sync::Arc;

use crate::db::{now_dts, Db};
use crate::mp4::TIMESCALE;
use crate::recorder::LiveHub;
use crate::video::dts_to_ms;

#[derive(Clone, Debug, Serialize)]
pub struct CameraHealth {
    pub id: i64,
    pub name: String,
    pub enabled: bool,
    pub storage_id: i64,
    pub recording: bool,
    pub codec: Option<String>,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub kbps: f32,
    pub last_frame_ms: Option<i64>,
    pub reconnects: u32,
    pub last_error: Option<String>,
    pub sub_recording: Option<bool>,
    pub detect_running: bool,
    pub detect_fps: f32,
    pub in_event: Option<i64>,
    pub last_event_ms: Option<i64>,
    pub events_24h: i64,
    /// bytes recorded (main + sub) in the last 24 h
    pub ingest_24h: i64,
    pub oldest_ms: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct StorageHealth {
    pub id: i64,
    pub path: String,
    pub available: bool,
    pub used_bytes: i64,
    pub free_bytes: u64,
    pub max_bytes: Option<i64>,
    pub reserve_bytes: i64,
    pub archive_to: Option<i64>,
    /// bytes past the archive cutoff still on this primary (tiering backlog)
    pub archive_backlog_bytes: i64,
    /// bytes written to this storage in the last 24 h
    pub ingest_24h: i64,
    /// bytes retention may still fill before it starts deleting
    pub headroom_bytes: i64,
    /// how many days of footage this volume holds at the current ingest rate
    pub effective_days: Option<f64>,
    pub oldest_ms: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Issue {
    pub level: String, // "warn" | "error"
    pub subject: String,
    pub text: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct HealthReport {
    pub version: String,
    pub started_ms: i64,
    pub now_ms: i64,
    pub uptime_secs: i64,
    pub cameras: Vec<CameraHealth>,
    /// empty for viewers
    pub storages: Vec<StorageHealth>,
    pub issues: Vec<Issue>,
    pub load: [f64; 3],
}

fn loadavg() -> [f64; 3] {
    let mut l = [0f64; 3];
    let n = unsafe { libc::getloadavg(l.as_mut_ptr(), 3) };
    if n == 3 { l } else { [0.0; 3] }
}

/// Build the report. `visible` restricts cameras; `admin` adds storages and
/// per-camera error strings (which may contain hostnames).
pub fn report(db: &Db, hub: &Arc<LiveHub>, started_ms: i64, alert_after_ms: i64, visible: &[i64], admin: bool) -> Result<HealthReport> {
    let now = now_dts();
    let day_ago = now - 86_400 * TIMESCALE as i64;
    let ingest = db.ingest_since(day_ago)?; // (camera_id, storage_id, bytes)
    let last_events = db.last_event_per_camera()?;
    let counts = db.event_counts_since(day_ago)?;
    let oldest_cam = db.oldest_segment_per_camera()?;
    let mut issues = Vec::new();
    let mut cameras = Vec::new();
    for cam in db.cameras()?.into_iter().filter(|c| visible.contains(&c.id)) {
        let st = hub.get(cam.id).map(|h| h.status.read().clone()).unwrap_or_default();
        let sub = hub.get_stream(cam.id, crate::recorder::StreamKind::Sub).map(|h| h.status.read().connected);
        let det = crate::detect::status(hub, cam.id);
        let stale = st.last_frame_dts > 0 && dts_to_ms(now - st.last_frame_dts) > alert_after_ms;
        let recording = st.connected && !stale;
        if cam.enabled && !recording {
            issues.push(Issue { level: "error".into(), subject: cam.name.clone(), text: match (&st.last_error, admin) { (Some(e), true) => format!("not recording: {e}"), _ => "not recording".into() } });
        }
        if cam.enabled && det.as_ref().map(|d| !d.running).unwrap_or(true) {
            issues.push(Issue { level: "warn".into(), subject: cam.name.clone(), text: "motion detector not running".into() });
        }
        cameras.push(CameraHealth {
            id: cam.id,
            name: cam.name.clone(),
            enabled: cam.enabled,
            storage_id: cam.storage_id,
            recording,
            codec: st.codec.clone(),
            width: st.width,
            height: st.height,
            fps: st.fps,
            kbps: st.kbps,
            last_frame_ms: (st.last_frame_dts > 0).then(|| dts_to_ms(st.last_frame_dts)),
            reconnects: st.reconnects,
            last_error: if admin { st.last_error.clone() } else { None },
            sub_recording: sub,
            detect_running: det.as_ref().map(|d| d.running).unwrap_or(false),
            detect_fps: det.as_ref().map(|d| d.fps).unwrap_or(0.0),
            in_event: det.and_then(|d| d.in_event),
            last_event_ms: last_events.get(&cam.id).map(|d| dts_to_ms(*d)),
            events_24h: *counts.get(&cam.id).unwrap_or(&0),
            ingest_24h: ingest.iter().filter(|(c, _, _)| *c == cam.id).map(|(_, _, b)| *b).sum(),
            oldest_ms: oldest_cam.get(&cam.id).map(|d| dts_to_ms(*d)),
        });
    }
    let mut storages = Vec::new();
    if admin {
        let oldest_st = db.oldest_segment_per_storage()?;
        for s in db.storages()? {
            let p = Path::new(&s.path);
            let available = crate::retention::storage_mounted(&s);
            let used = db.storage_used_bytes(s.id)?;
            let archive_backlog_bytes = crate::tier::backlog_bytes(db, &s).unwrap_or(0);
            let free = crate::retention::fs_free_bytes(p).unwrap_or(0);
            let ingest_24h: i64 = ingest.iter().filter(|(_, st, _)| *st == s.id).map(|(_, _, b)| *b).sum();
            // room left before the budget deletes: min(cap - used, free - reserve)
            let by_free = free as i64 - s.reserve_bytes;
            let headroom = match s.max_bytes { Some(m) => by_free.min(m - used), None => by_free }.max(0);
            let capacity = used + headroom;
            let effective_days = (ingest_24h > 0).then(|| capacity as f64 / ingest_24h as f64);
            if !available {
                let why = if p.is_dir() { format!("volume not mounted (no {} marker)", crate::retention::MARKER) } else { "volume not mounted".to_string() };
                issues.push(Issue { level: "error".into(), subject: s.path.clone(), text: why });
            } else if free < s.reserve_bytes as u64 / 2 && s.archive_to.is_some() {
                issues.push(Issue { level: "error".into(), subject: s.path.clone(), text: format!("free space {:.1} GB is under half the reserve: tiering cannot keep up, retention is deleting on the primary", free as f64 / 1e9) });
            } else if free < s.reserve_bytes as u64 {
                issues.push(Issue { level: "error".into(), subject: s.path.clone(), text: format!("free space {:.1} GB is under the {:.1} GB reserve", free as f64 / 1e9, s.reserve_bytes as f64 / 1e9) });
            } else if let Some(d) = effective_days {
                if d < 2.0 && s.archive_to.is_none() {
                    issues.push(Issue { level: "warn".into(), subject: s.path.clone(), text: format!("only {d:.1} days of footage fit at the current rate") });
                }
            }
            if let Some(a) = s.archive_to {
                let ok = db.storage(a)?.map(|x| crate::retention::storage_mounted(&x)).unwrap_or(false);
                if !ok {
                    issues.push(Issue { level: "warn".into(), subject: s.path.clone(), text: format!("archive volume (storage {a}) unavailable; retention deletes here instead") });
                } else if ingest_24h > 0 && archive_backlog_bytes > ingest_24h {
                    issues.push(Issue { level: "warn".into(), subject: s.path.clone(), text: format!("archive backlog {:.1} GB (more than a day of ingest): raise archive_rate_mbps", archive_backlog_bytes as f64 / 1e9) });
                }
            }
            storages.push(StorageHealth {
                id: s.id, path: s.path.clone(), available, used_bytes: used, free_bytes: free, max_bytes: s.max_bytes, reserve_bytes: s.reserve_bytes,
                archive_to: s.archive_to, archive_backlog_bytes, ingest_24h, headroom_bytes: headroom, effective_days, oldest_ms: oldest_st.get(&s.id).map(|d| dts_to_ms(*d)),
            });
        }
    }
    let now_ms = dts_to_ms(now);
    Ok(HealthReport {
        version: env!("CARGO_PKG_VERSION").into(),
        started_ms,
        now_ms,
        uptime_secs: (now_ms - started_ms) / 1000,
        cameras,
        storages,
        issues,
        load: loadavg(),
    })
}

/// Escape a label value per the Prometheus text format.
fn prom_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

/// Prometheus text exposition of the report.
pub fn prometheus(r: &HealthReport) -> String {
    let mut o = String::new();
    let mut line = |name: &str, labels: &str, v: f64| {
        o.push_str(name);
        if !labels.is_empty() {
            o.push('{');
            o.push_str(labels);
            o.push('}');
        }
        o.push(' ');
        o.push_str(&format!("{v}\n"));
    };
    line("zmng_uptime_seconds", "", r.uptime_secs as f64);
    line("zmng_issues", "level=\"error\"", r.issues.iter().filter(|i| i.level == "error").count() as f64);
    line("zmng_issues", "level=\"warn\"", r.issues.iter().filter(|i| i.level == "warn").count() as f64);
    for c in &r.cameras {
        let l = format!("camera=\"{}\",name=\"{}\"", c.id, prom_escape(&c.name));
        line("zmng_camera_recording", &l, c.recording as i64 as f64);
        line("zmng_camera_fps", &l, c.fps as f64);
        line("zmng_camera_kbps", &l, c.kbps as f64);
        line("zmng_camera_reconnects_total", &l, c.reconnects as f64);
        line("zmng_camera_detect_running", &l, c.detect_running as i64 as f64);
        line("zmng_camera_detect_fps", &l, c.detect_fps as f64);
        line("zmng_camera_in_event", &l, c.in_event.is_some() as i64 as f64);
        line("zmng_camera_events_24h", &l, c.events_24h as f64);
        line("zmng_camera_ingest_bytes_24h", &l, c.ingest_24h as f64);
    }
    for s in &r.storages {
        let l = format!("storage=\"{}\",path=\"{}\"", s.id, prom_escape(&s.path));
        line("zmng_storage_available", &l, s.available as i64 as f64);
        line("zmng_storage_used_bytes", &l, s.used_bytes as f64);
        line("zmng_storage_free_bytes", &l, s.free_bytes as f64);
        line("zmng_storage_headroom_bytes", &l, s.headroom_bytes as f64);
        line("zmng_storage_ingest_bytes_24h", &l, s.ingest_24h as f64);
        line("zmng_storage_effective_days", &l, s.effective_days.unwrap_or(0.0));
        line("zmng_storage_archive_backlog_bytes", &l, s.archive_backlog_bytes as f64);
    }
    o
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, PartialEq)]
pub enum Verdict {
    Pass,
    Warn,
    Fail,
}

#[derive(Clone, Debug, Serialize)]
pub struct Check {
    pub name: String,
    pub verdict: Verdict,
    pub detail: String,
}

fn check(name: &str, v: Verdict, detail: impl Into<String>) -> Check {
    Check { name: name.into(), verdict: v, detail: detail.into() }
}

/// Some(owner uid) when the file belongs to another user.
fn file_owner_mismatch(p: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    let uid = std::fs::metadata(p).ok()?.uid();
    (uid != unsafe { libc::geteuid() }).then_some(uid)
}

fn writable(dir: &Path) -> std::result::Result<(), String> {
    if !dir.is_dir() {
        return Err("not a directory".into());
    }
    let probe = dir.join(format!(".zmng-doctor-{}", std::process::id()));
    std::fs::write(&probe, b"ok").map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// Run the pre-flight checks: config paths, ffmpeg, database, storages,
/// clock, and (optionally) an RTSP DESCRIBE of every enabled camera.
pub async fn doctor(cfg: &crate::config::Config, probe_cameras: bool) -> Vec<Check> {
    let mut out = Vec::new();
    // ffmpeg
    match tokio::process::Command::new(&cfg.ffmpeg).arg("-version").output().await {
        Ok(o) if o.status.success() => {
            let first = String::from_utf8_lossy(&o.stdout).lines().next().unwrap_or("").to_string();
            out.push(check("ffmpeg", Verdict::Pass, first));
        }
        Ok(o) => out.push(check("ffmpeg", Verdict::Fail, format!("exit {}", o.status))),
        Err(e) => out.push(check("ffmpeg", Verdict::Fail, format!("{}: {e}", cfg.ffmpeg))),
    }
    // clock
    let now = crate::db::now_secs();
    if now < 1_760_000_000 {
        out.push(check("clock", Verdict::Fail, "system time is before 2025-10; recordings would be misdated (fix NTP)"));
    } else {
        out.push(check("clock", Verdict::Pass, chrono::Utc::now().to_rfc3339()));
    }
    // listen address
    match cfg.listen.parse::<std::net::SocketAddr>() {
        Ok(a) => out.push(check("listen", Verdict::Pass, a.to_string())),
        Err(e) => out.push(check("listen", Verdict::Fail, format!("{}: {e}", cfg.listen))),
    }
    // web dir + thumb dir
    if cfg.web_dir.join("index.html").is_file() {
        out.push(check("web_dir", Verdict::Pass, cfg.web_dir.display().to_string()));
    } else {
        out.push(check("web_dir", Verdict::Fail, format!("{} has no index.html", cfg.web_dir.display())));
    }
    if !cfg.thumb_dir.exists() {
        out.push(check("thumb_dir", Verdict::Warn, format!("{} does not exist yet; the service creates it at start", cfg.thumb_dir.display())));
    } else {
        match writable(&cfg.thumb_dir) {
            Ok(()) => out.push(check("thumb_dir", Verdict::Pass, cfg.thumb_dir.display().to_string())),
            Err(e) => out.push(check("thumb_dir", Verdict::Fail, format!("{}: {e}", cfg.thumb_dir.display()))),
        }
    }
    if let Some(b) = &cfg.backup_dir {
        match writable(b) {
            Ok(()) => out.push(check("backup_dir", Verdict::Pass, b.display().to_string())),
            Err(e) => out.push(check("backup_dir", Verdict::Warn, format!("{}: {e}", b.display()))),
        }
    }
    // database: opened read-only so a pre-flight run as the wrong user can
    // neither migrate the schema nor leave root-owned -wal/-shm files behind
    if !cfg.db_path.exists() {
        out.push(check("database", Verdict::Warn, format!("{} does not exist yet; the service creates it at start", cfg.db_path.display())));
        return out;
    }
    if let Some(owner) = file_owner_mismatch(&cfg.db_path) {
        out.push(check("database owner", Verdict::Warn, format!("{} is owned by uid {owner} but this runs as uid {}; run doctor as the service user", cfg.db_path.display(), unsafe { libc::geteuid() })));
    }
    let db = match Db::open_read_only(&cfg.db_path) {
        Ok(db) => {
            match db.integrity_check() {
                Ok(msg) if msg == "ok" => out.push(check("database", Verdict::Pass, cfg.db_path.display().to_string())),
                Ok(msg) => out.push(check("database", Verdict::Fail, format!("integrity_check: {msg}"))),
                Err(e) => out.push(check("database", Verdict::Fail, e.to_string())),
            }
            db
        }
        Err(e) => {
            out.push(check("database", Verdict::Fail, e.to_string()));
            return out;
        }
    };
    if db.user_count().unwrap_or(0) == 0 {
        out.push(check("users", Verdict::Warn, "no users yet: the first admin is created in the UI with the setup token"));
    }
    // storages
    let storages = db.storages().unwrap_or_default();
    if storages.is_empty() {
        out.push(check("storage", Verdict::Fail, "no storage configured (zmng add-storage ...)"));
    }
    for s in &storages {
        let p = Path::new(&s.path);
        let access = if s.read_only {
            p.is_dir().then_some(()).ok_or_else(|| "not a directory".to_string())
        } else if !crate::retention::storage_mounted(s) {
            Err(if p.is_dir() { format!("no {} marker: volume not mounted, or run add-storage again", crate::retention::MARKER) } else { "not a directory".to_string() })
        } else {
            writable(p)
        };
        match access {
            Ok(()) => {
                let free = crate::retention::fs_free_bytes(p).unwrap_or(0);
                let v = if !s.read_only && free < s.reserve_bytes as u64 { Verdict::Warn } else { Verdict::Pass };
                let what = if s.read_only { "read-only, " } else { "" };
                out.push(check(&format!("storage {}", s.id), v, format!("{} {what}free {:.1} GB (reserve {:.1} GB)", s.path, free as f64 / 1e9, s.reserve_bytes as f64 / 1e9)));
            }
            Err(e) => out.push(check(&format!("storage {}", s.id), Verdict::Fail, format!("{}: {e}", s.path))),
        }
        if let Some(a) = s.archive_to {
            let ok = storages.iter().any(|x| x.id == a && crate::retention::storage_mounted(x));
            out.push(check(&format!("storage {} archive", s.id), if ok { Verdict::Pass } else { Verdict::Warn }, format!("archive_to storage {a}{}", if ok { "" } else { " is not mounted" })));
        }
    }
    // cameras
    let cams = db.cameras().unwrap_or_default();
    if cams.is_empty() {
        out.push(check("cameras", Verdict::Warn, "no cameras configured"));
    }
    for c in cams.iter().filter(|c| c.enabled) {
        if !storages.iter().any(|s| s.id == c.storage_id) {
            out.push(check(&format!("camera {}", c.id), Verdict::Fail, format!("{}: storage {} does not exist", c.name, c.storage_id)));
        }
        if let Ok(u) = url::Url::parse(&c.main_url) {
            if u.scheme() != "rtsp" && u.scheme() != "rtsps" {
                out.push(check(&format!("camera {}", c.id), Verdict::Fail, format!("{}: main_url must be rtsp://", c.name)));
            }
        } else {
            out.push(check(&format!("camera {}", c.id), Verdict::Fail, format!("{}: main_url is not a URL", c.name)));
        }
        if probe_cameras {
            out.push(probe_rtsp(c).await);
        }
    }
    out
}

/// RTSP DESCRIBE with a 5 s timeout (credentials never appear in the output).
async fn probe_rtsp(c: &crate::db::Camera) -> Check {
    let name = format!("camera {} rtsp", c.id);
    let Ok(url) = url::Url::parse(&c.main_url) else { return check(&name, Verdict::Fail, "bad url") };
    let creds = (!url.username().is_empty()).then(|| retina::client::Credentials { username: url.username().to_string(), password: url.password().unwrap_or("").to_string() });
    let mut clean = url.clone();
    let _ = clean.set_username("");
    let _ = clean.set_password(None);
    let opts = retina::client::SessionOptions::default().creds(creds).user_agent("zmng-doctor".to_string());
    match tokio::time::timeout(std::time::Duration::from_secs(5), retina::client::Session::describe(clean.clone(), opts)).await {
        Ok(Ok(s)) => {
            let codecs: Vec<String> = s.streams().iter().map(|st| format!("{}/{}", st.media(), st.encoding_name())).collect();
            check(&name, Verdict::Pass, format!("{}: {}", c.name, codecs.join(", ")))
        }
        Ok(Err(e)) => check(&name, Verdict::Fail, format!("{}: {}", c.name, e.to_string().replace(url.password().unwrap_or("\u{0}"), "***"))),
        Err(_) => check(&name, Verdict::Fail, format!("{}: no answer from {} in 5 s", c.name, clean.host_str().unwrap_or("?"))),
    }
}

// ---------------------------------------------------------------------------
// backup / restore
// ---------------------------------------------------------------------------

/// Write a consistent copy of the index at `db_path` to `dest` (VACUUM INTO),
/// keeping the previous copy as `<dest>.1`. Uses its own read-only
/// connection: in WAL mode a reader never blocks the recorders' writer, so
/// a multi-hundred-MB copy to a slow USB disk stalls nothing. The output is
/// fsynced (VACUUM INTO itself does not sync).
pub fn backup(db_path: &Path, dest: &Path) -> Result<()> {
    let parent = dest.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    if !parent.exists() {
        // only create the final directory, never a whole tree: a missing
        // grandparent means the backup volume is not mounted
        let gp = parent.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        if !gp.exists() {
            anyhow::bail!("backup directory {} is not available (volume not mounted?)", parent.display());
        }
        std::fs::create_dir(parent)?;
    }
    let prev = dest.with_extension(format!("{}.1", dest.extension().and_then(|e| e.to_str()).unwrap_or("db")));
    if dest.exists() {
        let _ = std::fs::rename(dest, &prev);
    }
    let conn = rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_context(|| format!("opening {}", db_path.display()))?;
    conn.execute_batch("PRAGMA busy_timeout=30000;")?;
    conn.execute("VACUUM INTO ?1", [dest.to_string_lossy().as_ref()]).with_context(|| format!("VACUUM INTO {}", dest.display()))?;
    std::fs::File::open(dest)?.sync_all()?;
    if let Ok(d) = std::fs::File::open(parent) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Advisory lock on `<db>.lock` held by the running service so `restore`
/// cannot swap the index underneath it.
#[derive(Debug)]
pub struct ServiceLock {
    _file: std::fs::File,
}

impl ServiceLock {
    pub fn lock_path(db_path: &Path) -> std::path::PathBuf {
        let mut p = db_path.as_os_str().to_owned();
        p.push(".lock");
        std::path::PathBuf::from(p)
    }
    /// Non-blocking; fails when another process holds it.
    pub fn acquire(db_path: &Path) -> Result<ServiceLock> {
        use std::os::unix::io::AsRawFd;
        let path = Self::lock_path(db_path);
        let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path).with_context(|| format!("opening {}", path.display()))?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            anyhow::bail!("{} is locked: the zmng service is running (stop it first)", path.display());
        }
        Ok(ServiceLock { _file: file })
    }
}

/// Default backup location: `backup_dir/zmng.db.backup` or next to the db.
pub fn backup_path(cfg: &crate::config::Config) -> std::path::PathBuf {
    match &cfg.backup_dir {
        Some(d) => d.join(format!("{}.backup", cfg.db_path.file_name().and_then(|f| f.to_str()).unwrap_or("zmng.db"))),
        None => cfg.db_path.with_extension("db.backup"),
    }
}

/// Replace the live index with a backup after checking it is a healthy
/// zmng database. The caller holds the [`ServiceLock`] (so the service is
/// not running); the previous index is checkpointed and kept complete as
/// `*.before-restore`.
pub fn restore(src: &Path, db_path: &Path) -> Result<()> {
    if let (Ok(a), Ok(b)) = (std::fs::canonicalize(src), std::fs::canonicalize(db_path)) {
        if a == b {
            anyhow::bail!("source and destination are the same file");
        }
    }
    let conn = rusqlite::Connection::open_with_flags(src, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).with_context(|| format!("opening {}", src.display()))?;
    let ok: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if ok != "ok" {
        anyhow::bail!("{} fails integrity_check: {ok}", src.display());
    }
    for t in ["camera", "segment", "event", "user", "storage"] {
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1", [t], |r| r.get(0))?;
        if n != 1 {
            anyhow::bail!("{} is not a zmng index (no {t} table)", src.display());
        }
    }
    drop(conn);
    if db_path.exists() {
        // fold the WAL into the main file so the kept copy is complete, then
        // there is nothing in -wal/-shm worth keeping
        let old = rusqlite::Connection::open(db_path).with_context(|| format!("opening {}", db_path.display()))?;
        old.execute_batch("PRAGMA busy_timeout=5000; PRAGMA wal_checkpoint(TRUNCATE);").context("checkpointing the old index")?;
        drop(old);
        let keep = db_path.with_extension("db.before-restore");
        std::fs::rename(db_path, &keep).with_context(|| format!("moving the old index to {}", keep.display()))?;
    }
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", db_path.display()));
    }
    std::fs::copy(src, db_path).with_context(|| format!("copying to {}", db_path.display()))?;
    std::fs::File::open(db_path)?.sync_all()?;
    if let Some(d) = db_path.parent().and_then(|p| std::fs::File::open(p).ok()) {
        let _ = d.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prometheus_format() {
        let r = HealthReport {
            version: "0".into(), started_ms: 0, now_ms: 1000, uptime_secs: 1,
            cameras: vec![CameraHealth { id: 1, name: "Front \"door\"".into(), enabled: true, storage_id: 1, recording: true, codec: None, width: 0, height: 0, fps: 18.0, kbps: 2000.0, last_frame_ms: None, reconnects: 2, last_error: None, sub_recording: None, detect_running: true, detect_fps: 5.0, in_event: Some(3), last_event_ms: None, events_24h: 7, ingest_24h: 1000, oldest_ms: None }],
            storages: vec![StorageHealth { id: 1, path: "/vol".into(), available: true, used_bytes: 10, free_bytes: 90, max_bytes: None, reserve_bytes: 5, archive_to: None, archive_backlog_bytes: 0, ingest_24h: 20, headroom_bytes: 85, effective_days: Some(4.75), oldest_ms: None }],
            issues: vec![Issue { level: "warn".into(), subject: "x".into(), text: "y".into() }],
            load: [0.0; 3],
        };
        let p = prometheus(&r);
        assert!(p.contains("zmng_camera_recording{camera=\"1\",name=\"Front \\\"door\\\"\"} 1\n"), "{p}");
        assert!(p.contains("zmng_camera_in_event{camera=\"1\",name=\"Front \\\"door\\\"\"} 1\n"));
        assert!(p.contains("zmng_storage_effective_days{storage=\"1\",path=\"/vol\"} 4.75\n"));
        assert!(p.contains("zmng_issues{level=\"warn\"} 1\n"));
        assert_eq!(prom_escape("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }
}
