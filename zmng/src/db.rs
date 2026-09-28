//! SQLite index. One row per *segment* (a ~60 s fMP4 file) and one row per
//! *event*; never a row per frame. See docs/ARCHITECTURE.md §Data model.

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

use crate::mp4::{self, FragEntry};

pub const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

CREATE TABLE IF NOT EXISTS storage (
  id            INTEGER PRIMARY KEY,
  path          TEXT NOT NULL UNIQUE,
  -- hard cap on bytes of segments kept on this storage (NULL = use free-space reserve only)
  max_bytes     INTEGER,
  -- always keep at least this many bytes free on the filesystem
  reserve_bytes INTEGER NOT NULL DEFAULT 21474836480,
  -- tiering: move segments older than archive_after_days (or under space
  -- pressure) to this storage, oldest first, rate limited; NULL = never
  archive_to          INTEGER REFERENCES storage(id),
  archive_after_days  REAL,
  archive_rate_mbps   INTEGER NOT NULL DEFAULT 40,
  -- never delete, move or write here (imported ZoneMinder event trees)
  read_only           INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS camera (
  id               INTEGER PRIMARY KEY,
  name             TEXT NOT NULL UNIQUE,
  main_url         TEXT NOT NULL,
  sub_url          TEXT,
  enabled          INTEGER NOT NULL DEFAULT 1,
  storage_id       INTEGER NOT NULL REFERENCES storage(id),
  retention_days   REAL NOT NULL DEFAULT 30,
  -- substream analysis
  detect_fps       REAL NOT NULL DEFAULT 5,
  detect_width     INTEGER NOT NULL DEFAULT 320,
  detect_height    INTEGER NOT NULL DEFAULT 180,
  pixel_threshold  INTEGER NOT NULL DEFAULT 25,
  min_area_pct     REAL NOT NULL DEFAULT 1.0,
  min_blob_pct     REAL NOT NULL DEFAULT 0.5,
  pre_secs         REAL NOT NULL DEFAULT 5,
  post_secs        REAL NOT NULL DEFAULT 8,
  cooldown_secs    REAL NOT NULL DEFAULT 10,
  -- JSON: [[[x,y],...], ...] polygons in 0..1 normalized coords (empty = whole frame)
  zones_json       TEXT NOT NULL DEFAULT '[]',
  -- JSON: [[[x,y],...], ...] polygons in 0..1 to ignore (privacy / swaying trees)
  masks_json       TEXT NOT NULL DEFAULT '[]',
  sort_order       INTEGER NOT NULL DEFAULT 0,
  -- also record the substream (multi-camera review, non-HEVC clients)
  record_sub       INTEGER NOT NULL DEFAULT 1,
  -- segments overlapping an event are kept this long (0 = same as retention_days)
  event_retention_days REAL NOT NULL DEFAULT 0,
  -- comma separated group names (Outside, Inside, ...), used for filtering
  tags             TEXT NOT NULL DEFAULT '',
  -- object detection: on/off, label override (comma list, empty = server default),
  -- and whether an event without a detected object is discarded
  objects          INTEGER NOT NULL DEFAULT 1,
  object_labels    TEXT NOT NULL DEFAULT '',
  require_object   INTEGER NOT NULL DEFAULT 0,
  -- ONVIF PTZ: device service URL (default http://<rtsp host>/onvif/device_service),
  -- credentials (default: the RTSP URL's), and what `probe` discovered
  onvif_url        TEXT NOT NULL DEFAULT '',
  onvif_user       TEXT NOT NULL DEFAULT '',
  onvif_pass       TEXT NOT NULL DEFAULT '',
  ptz              INTEGER NOT NULL DEFAULT 0,
  ptz_url          TEXT NOT NULL DEFAULT '',
  ptz_profile      TEXT NOT NULL DEFAULT '',
  -- record the main stream's AAC audio track too (opt-in)
  record_audio     INTEGER NOT NULL DEFAULT 0,
  created_at       INTEGER NOT NULL
);

-- codec configuration (the mp4 sample entry) deduplicated across segments
CREATE TABLE IF NOT EXISTS sample_entry (
  id       INTEGER PRIMARY KEY,
  sha256   TEXT NOT NULL UNIQUE,
  codec    TEXT NOT NULL,
  rfc6381  TEXT NOT NULL,
  width    INTEGER NOT NULL,
  height   INTEGER NOT NULL,
  data     BLOB NOT NULL,
  -- optional AAC track recorded with the video (mp4a box, rate, channels, codec string)
  audio_entry    BLOB,
  audio_rate     INTEGER,
  audio_channels INTEGER,
  audio_rfc6381  TEXT
);

CREATE TABLE IF NOT EXISTS segment (
  id               INTEGER PRIMARY KEY,
  camera_id        INTEGER NOT NULL REFERENCES camera(id),
  storage_id       INTEGER NOT NULL REFERENCES storage(id),
  sample_entry_id  INTEGER NOT NULL REFERENCES sample_entry(id),
  start_dts        INTEGER NOT NULL,   -- 90 kHz ticks since Unix epoch
  end_dts          INTEGER NOT NULL,
  path             TEXT NOT NULL,      -- relative to storage.path
  bytes            INTEGER NOT NULL,
  init_len         INTEGER NOT NULL,
  frames           INTEGER NOT NULL,
  motion_max       INTEGER NOT NULL DEFAULT 0,
  index_blob       BLOB NOT NULL,      -- mp4::encode_index (dts already absolute)
  -- non-zero for imported files whose tfdt values are relative: add this when serving
  dts_offset       INTEGER NOT NULL DEFAULT 0,
  -- provenance for imported ZoneMinder events
  zm_event_id      INTEGER,
  -- 'main' (full resolution) or 'sub' (substream recording)
  stream           TEXT NOT NULL DEFAULT 'main',
  created_at       INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS event (
  id          INTEGER PRIMARY KEY,
  camera_id   INTEGER NOT NULL REFERENCES camera(id),
  start_dts   INTEGER NOT NULL,
  end_dts     INTEGER,                 -- NULL while in progress
  kind        TEXT NOT NULL DEFAULT 'motion',
  score       INTEGER NOT NULL DEFAULT 0,   -- peak 0..255
  peak_dts    INTEGER,
  thumb_path  TEXT,                    -- relative to thumb_dir
  meta_json   TEXT NOT NULL DEFAULT '{}',
  archived    INTEGER NOT NULL DEFAULT 0,
  notes       TEXT,
  created_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS user (
  id         INTEGER PRIMARY KEY,
  username   TEXT NOT NULL UNIQUE,
  pass_hash  TEXT NOT NULL,
  role       TEXT NOT NULL DEFAULT 'viewer',   -- admin | viewer
  created_at INTEGER NOT NULL
);
-- viewers see only the cameras listed here (fails closed); admins see all
CREATE TABLE IF NOT EXISTS user_camera (
  user_id   INTEGER NOT NULL REFERENCES user(id) ON DELETE CASCADE,
  camera_id INTEGER NOT NULL REFERENCES camera(id) ON DELETE CASCADE,
  PRIMARY KEY (user_id, camera_id)
);
CREATE TABLE IF NOT EXISTS push_token (
  id           INTEGER PRIMARY KEY,
  user_id      INTEGER NOT NULL REFERENCES user(id) ON DELETE CASCADE,
  token        TEXT NOT NULL UNIQUE,
  platform     TEXT NOT NULL DEFAULT '',
  monitor_list TEXT NOT NULL DEFAULT '',
  interval     INTEGER NOT NULL DEFAULT 0,
  push_state   TEXT NOT NULL DEFAULT 'enabled',
  app_version  TEXT NOT NULL DEFAULT '',
  profile      TEXT NOT NULL DEFAULT '',
  created_at   INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS session (
  token      TEXT PRIMARY KEY,
  user_id    INTEGER NOT NULL REFERENCES user(id) ON DELETE CASCADE,
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL
);
"#;

/// Indexes are created after `migrate()` so a database from an older build
/// (whose tables lack the newer columns) can be opened and upgraded.
const INDEXES: &str = r#"
CREATE INDEX IF NOT EXISTS segment_cam_start ON segment(camera_id, start_dts);
CREATE INDEX IF NOT EXISTS segment_cam_stream_start ON segment(camera_id, stream, start_dts);
CREATE INDEX IF NOT EXISTS segment_cam_end   ON segment(camera_id, end_dts);
CREATE INDEX IF NOT EXISTS segment_storage   ON segment(storage_id, start_dts);
CREATE INDEX IF NOT EXISTS event_cam_start ON event(camera_id, start_dts);
CREATE INDEX IF NOT EXISTS event_start     ON event(start_dts);
"#;

fn validate_camera_field(k: &str, v: &serde_json::Value) -> Result<()> {
    let num = |lo: f64, hi: f64| -> Result<()> {
        let f = v.as_f64().ok_or_else(|| anyhow::anyhow!("{k} must be a number"))?;
        if f < lo || f > hi {
            anyhow::bail!("{k} must be between {lo} and {hi}");
        }
        Ok(())
    };
    match k {
        "name" | "main_url" => {
            if v.as_str().map(|s| s.trim().is_empty()).unwrap_or(true) {
                anyhow::bail!("{k} must be a non-empty string");
            }
        }
        "tags" | "object_labels" | "onvif_url" | "onvif_user" | "onvif_pass" | "ptz_url" | "ptz_profile" => {
            if !v.is_string() {
                anyhow::bail!("{k} must be a string");
            }
        }
        "sub_url" => {
            if !(v.is_null() || v.is_string()) {
                anyhow::bail!("sub_url must be a string or null");
            }
        }
        "event_retention_days" => num(0.0, 3650.0)?,
        "enabled" | "record_sub" | "objects" | "require_object" | "ptz" | "record_audio" => {
            if !(v.is_boolean() || v.as_i64().map(|i| i == 0 || i == 1).unwrap_or(false)) {
                anyhow::bail!("enabled must be true/false");
            }
        }
        "storage_id" | "sort_order" => num(-1e9, 1e9)?,
        "retention_days" => num(0.01, 3650.0)?,
        "detect_fps" => num(0.2, 30.0)?,
        "detect_width" => num(64.0, 1920.0)?,
        "detect_height" => num(36.0, 1080.0)?,
        "pixel_threshold" => num(1.0, 255.0)?,
        "min_area_pct" | "min_blob_pct" => num(0.0, 100.0)?,
        "pre_secs" | "post_secs" | "cooldown_secs" => num(0.0, 600.0)?,
        "zones_json" | "masks_json" => {
            let s = v.as_str().ok_or_else(|| anyhow::anyhow!("{k} must be a JSON string"))?;
            serde_json::from_str::<Vec<Vec<(f64, f64)>>>(s).map_err(|e| anyhow::anyhow!("{k}: {e}"))?;
        }
        _ => {}
    }
    Ok(())
}

/// Additive migrations for databases created by earlier builds.
fn migrate(c: &Connection) -> Result<()> {
    let has_col = |table: &str, col: &str| -> Result<bool> {
        let mut st = c.prepare(&format!("PRAGMA table_info({table})"))?;
        let cols: Vec<String> = st.query_map([], |r| r.get::<_, String>(1))?.collect::<std::result::Result<_, _>>()?;
        Ok(cols.iter().any(|x| x == col))
    };
    if !has_col("segment", "dts_offset")? {
        c.execute_batch("ALTER TABLE segment ADD COLUMN dts_offset INTEGER NOT NULL DEFAULT 0;")?;
    }
    if !has_col("segment", "zm_event_id")? {
        c.execute_batch("ALTER TABLE segment ADD COLUMN zm_event_id INTEGER;")?;
    }
    if !has_col("segment", "stream")? {
        c.execute_batch("ALTER TABLE segment ADD COLUMN stream TEXT NOT NULL DEFAULT 'main'; CREATE INDEX IF NOT EXISTS segment_cam_stream_start ON segment(camera_id, stream, start_dts);")?;
    }
    if !has_col("storage", "archive_to")? {
        c.execute_batch("ALTER TABLE storage ADD COLUMN archive_to INTEGER REFERENCES storage(id); ALTER TABLE storage ADD COLUMN archive_after_days REAL; ALTER TABLE storage ADD COLUMN archive_rate_mbps INTEGER NOT NULL DEFAULT 40;")?;
    }
    if !has_col("storage", "read_only")? {
        c.execute_batch("ALTER TABLE storage ADD COLUMN read_only INTEGER NOT NULL DEFAULT 0;")?;
    }
    if !has_col("camera", "tags")? {
        c.execute_batch("ALTER TABLE camera ADD COLUMN tags TEXT NOT NULL DEFAULT '';")?;
    }
    if !has_col("sample_entry", "audio_entry")? {
        c.execute_batch("ALTER TABLE sample_entry ADD COLUMN audio_entry BLOB; ALTER TABLE sample_entry ADD COLUMN audio_rate INTEGER; ALTER TABLE sample_entry ADD COLUMN audio_channels INTEGER; ALTER TABLE sample_entry ADD COLUMN audio_rfc6381 TEXT;")?;
    }
    if !has_col("camera", "record_audio")? {
        c.execute_batch("ALTER TABLE camera ADD COLUMN record_audio INTEGER NOT NULL DEFAULT 0;")?;
    }
    if !has_col("camera", "ptz")? {
        c.execute_batch("ALTER TABLE camera ADD COLUMN onvif_url TEXT NOT NULL DEFAULT ''; ALTER TABLE camera ADD COLUMN onvif_user TEXT NOT NULL DEFAULT ''; ALTER TABLE camera ADD COLUMN onvif_pass TEXT NOT NULL DEFAULT ''; ALTER TABLE camera ADD COLUMN ptz INTEGER NOT NULL DEFAULT 0; ALTER TABLE camera ADD COLUMN ptz_url TEXT NOT NULL DEFAULT ''; ALTER TABLE camera ADD COLUMN ptz_profile TEXT NOT NULL DEFAULT '';")?;
    }
    if !has_col("camera", "objects")? {
        c.execute_batch("ALTER TABLE camera ADD COLUMN objects INTEGER NOT NULL DEFAULT 1; ALTER TABLE camera ADD COLUMN object_labels TEXT NOT NULL DEFAULT ''; ALTER TABLE camera ADD COLUMN require_object INTEGER NOT NULL DEFAULT 0;")?;
    }
    if !has_col("camera", "record_sub")? {
        c.execute_batch("ALTER TABLE camera ADD COLUMN record_sub INTEGER NOT NULL DEFAULT 1; ALTER TABLE camera ADD COLUMN event_retention_days REAL NOT NULL DEFAULT 0;")?;
    }
    Ok(())
}

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Storage {
    pub id: i64,
    pub path: String,
    pub max_bytes: Option<i64>,
    pub reserve_bytes: i64,
    pub archive_to: Option<i64>,
    pub archive_after_days: Option<f64>,
    pub archive_rate_mbps: i64,
    pub read_only: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Camera {
    pub id: i64,
    pub name: String,
    pub main_url: String,
    pub sub_url: Option<String>,
    pub enabled: bool,
    pub storage_id: i64,
    pub retention_days: f64,
    pub detect_fps: f64,
    pub detect_width: u32,
    pub detect_height: u32,
    pub pixel_threshold: u8,
    pub min_area_pct: f64,
    pub min_blob_pct: f64,
    pub pre_secs: f64,
    pub post_secs: f64,
    pub cooldown_secs: f64,
    pub zones_json: String,
    pub masks_json: String,
    pub sort_order: i64,
    pub record_sub: bool,
    pub event_retention_days: f64,
    pub tags: String,
    pub objects: bool,
    pub object_labels: String,
    pub require_object: bool,
    pub onvif_url: String,
    pub onvif_user: String,
    /// never serialized: viewers must not see it
    #[serde(skip_serializing)]
    pub onvif_pass: String,
    pub ptz: bool,
    pub ptz_url: String,
    pub ptz_profile: String,
    pub record_audio: bool,
}

impl Camera {
    /// A camera with default settings (tests, examples).
    pub fn example() -> Camera {
        Camera {
            id: 1, name: "cam".into(), main_url: "rtsp://u:p@10.0.0.1/1".into(), sub_url: None, enabled: true, storage_id: 1,
            retention_days: 30.0, detect_fps: 5.0, detect_width: 320, detect_height: 180, pixel_threshold: 25, min_area_pct: 1.0,
            min_blob_pct: 0.5, pre_secs: 5.0, post_secs: 8.0, cooldown_secs: 10.0, zones_json: "[]".into(), masks_json: "[]".into(),
            sort_order: 0, record_sub: true, event_retention_days: 0.0, tags: String::new(), objects: true, object_labels: String::new(),
            require_object: false, onvif_url: String::new(), onvif_user: String::new(), onvif_pass: String::new(), ptz: false,
            ptz_url: String::new(), ptz_profile: String::new(), record_audio: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SampleEntry {
    pub id: i64,
    pub codec: String,
    pub rfc6381: String,
    pub width: u32,
    pub height: u32,
    #[serde(skip)]
    pub data: Vec<u8>,
    #[serde(skip)]
    pub audio: Option<mp4::AudioParams>,
}

impl SampleEntry {
    /// The parameters needed to rebuild this entry's init segment.
    pub fn video_params(&self) -> mp4::VideoParams {
        mp4::VideoParams {
            codec: mp4::Codec::parse(&self.codec).unwrap_or(mp4::Codec::H264),
            width: self.width,
            height: self.height,
            sample_entry: bytes::Bytes::from(self.data.clone()),
            rfc6381: self.rfc6381.clone(),
            audio: self.audio.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Segment {
    pub id: i64,
    pub camera_id: i64,
    pub storage_id: i64,
    pub sample_entry_id: i64,
    pub start_dts: i64,
    pub end_dts: i64,
    pub path: String,
    pub bytes: i64,
    pub init_len: u32,
    pub frames: i64,
    pub motion_max: u8,
    pub dts_offset: i64,
    pub zm_event_id: Option<i64>,
    pub stream: String,
    #[serde(skip)]
    pub index: Vec<FragEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    pub camera_id: i64,
    pub start_dts: i64,
    pub end_dts: Option<i64>,
    pub kind: String,
    pub score: u8,
    pub peak_dts: Option<i64>,
    pub thumb_path: Option<String>,
    pub archived: bool,
    pub notes: Option<String>,
    /// JSON object: detector stats, detected objects (`objects`), import provenance
    pub meta_json: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub role: String,
    #[serde(skip)]
    pub pass_hash: String,
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 90 kHz ticks since epoch for "now".
pub fn now_dts() -> i64 {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (d.as_nanos() as i128 * 90_000 / 1_000_000_000) as i64
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON; \
             PRAGMA busy_timeout=5000; PRAGMA temp_store=MEMORY;",
        )?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        conn.execute_batch(INDEXES)?;
        conn.execute(
            "INSERT OR IGNORE INTO meta(key,value) VALUES('schema_version', ?1)",
            params![SCHEMA_VERSION.to_string()],
        )?;
        Ok(Db { conn: Arc::new(Mutex::new(conn)) })
    }

    /// Open without creating, migrating or writing anything (doctor, tools
    /// run as another user).
    pub fn open_read_only(path: &Path) -> Result<Db> {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .with_context(|| format!("opening {}", path.display()))?;
        conn.execute_batch("PRAGMA busy_timeout=5000;")?;
        Ok(Db { conn: Arc::new(Mutex::new(conn)) })
    }

    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let c = self.conn.lock();
        f(&c)
    }

    // ----- storage -----------------------------------------------------------

    pub fn storages(&self) -> Result<Vec<Storage>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT id,path,max_bytes,reserve_bytes,archive_to,archive_after_days,archive_rate_mbps,read_only FROM storage ORDER BY id")?;
            let rows = st.query_map([], |r| {
                Ok(Storage {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    max_bytes: r.get(2)?,
                    reserve_bytes: r.get(3)?,
                    archive_to: r.get(4)?,
                    archive_after_days: r.get(5)?,
                    archive_rate_mbps: r.get(6)?,
                    read_only: r.get::<_, i64>(7)? != 0,
                })
            })?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    pub fn storage(&self, id: i64) -> Result<Option<Storage>> {
        Ok(self.storages()?.into_iter().find(|s| s.id == id))
    }

    pub fn add_storage(&self, path: &str, max_bytes: Option<i64>, reserve_bytes: i64) -> Result<i64> {
        self.with(|c| {
            c.execute(
                "INSERT INTO storage(path,max_bytes,reserve_bytes) VALUES(?1,?2,?3)",
                params![path, max_bytes, reserve_bytes],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    pub fn update_storage(&self, id: i64, patch: &serde_json::Map<String, serde_json::Value>) -> Result<()> {
        self.with(|c| {
            for (k, v) in patch {
                let sql = match k.as_str() {
                    "max_bytes" | "reserve_bytes" | "archive_to" | "archive_after_days" | "archive_rate_mbps" | "read_only" => format!("UPDATE storage SET {k}=?1 WHERE id=?2"),
                    _ => anyhow::bail!("field {k} is not updatable"),
                };
                let val: rusqlite::types::Value = match v {
                    serde_json::Value::Null if k != "read_only" => rusqlite::types::Value::Null,
                    serde_json::Value::Bool(b) if k == "read_only" => rusqlite::types::Value::Integer(*b as i64),
                    serde_json::Value::Number(n) if k != "read_only" => n.as_i64().map(rusqlite::types::Value::Integer).unwrap_or_else(|| rusqlite::types::Value::Real(n.as_f64().unwrap_or(0.0))),
                    _ if k == "read_only" => anyhow::bail!("read_only must be true/false"),
                    _ => anyhow::bail!("{k} must be a number or null"),
                };
                if k == "archive_to" {
                    if let rusqlite::types::Value::Integer(t) = val {
                        if t == id {
                            anyhow::bail!("a storage cannot archive to itself");
                        }
                    }
                }
                c.execute(&sql, params![val, id])?;
            }
            Ok(())
        })
    }

    /// Move a segment's index row to another storage (file already copied to
    /// the same relative path there).
    pub fn move_segment(&self, id: i64, storage_id: i64) -> Result<()> {
        self.with(|c| {
            c.execute("UPDATE segment SET storage_id=?1 WHERE id=?2", params![storage_id, id])?;
            Ok(())
        })
    }

    /// Oldest segments on a storage older than `before_dts` (for archiving).
    pub fn archive_candidates(&self, storage_id: i64, before_dts: i64, limit: usize) -> Result<Vec<Segment>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT * FROM segment WHERE storage_id=?1 AND end_dts<?2 ORDER BY start_dts LIMIT ?3")?;
            let rows = st.query_map(params![storage_id, before_dts, limit as i64], Self::row_segment)?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    /// True if any event overlaps the segment and is archived or ended after `event_cutoff`.
    pub fn segment_pinned(&self, seg: &Segment, event_cutoff: i64) -> Result<bool> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT 1 FROM event WHERE camera_id=?1 AND end_dts IS NOT NULL AND end_dts>?2 AND start_dts<?3 AND (archived=1 OR end_dts>?4) LIMIT 1",
                params![seg.camera_id, seg.start_dts, seg.end_dts, event_cutoff],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
        })
    }

    pub fn storage_used_bytes(&self, storage_id: i64) -> Result<i64> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT COALESCE(SUM(bytes),0) FROM segment WHERE storage_id=?1",
                params![storage_id],
                |r| r.get(0),
            )?)
        })
    }

    // ----- cameras -----------------------------------------------------------

    fn row_camera(r: &rusqlite::Row) -> rusqlite::Result<Camera> {
        Ok(Camera {
            id: r.get("id")?,
            name: r.get("name")?,
            main_url: r.get("main_url")?,
            sub_url: r.get("sub_url")?,
            enabled: r.get::<_, i64>("enabled")? != 0,
            storage_id: r.get("storage_id")?,
            retention_days: r.get("retention_days")?,
            detect_fps: r.get("detect_fps")?,
            detect_width: r.get::<_, i64>("detect_width")? as u32,
            detect_height: r.get::<_, i64>("detect_height")? as u32,
            pixel_threshold: r.get::<_, i64>("pixel_threshold")? as u8,
            min_area_pct: r.get("min_area_pct")?,
            min_blob_pct: r.get("min_blob_pct")?,
            pre_secs: r.get("pre_secs")?,
            post_secs: r.get("post_secs")?,
            cooldown_secs: r.get("cooldown_secs")?,
            zones_json: r.get("zones_json")?,
            masks_json: r.get("masks_json")?,
            sort_order: r.get("sort_order")?,
            record_sub: r.get::<_, i64>("record_sub")? != 0,
            event_retention_days: r.get("event_retention_days")?,
            tags: r.get("tags")?,
            objects: r.get::<_, i64>("objects")? != 0,
            object_labels: r.get("object_labels")?,
            require_object: r.get::<_, i64>("require_object")? != 0,
            onvif_url: r.get("onvif_url")?,
            onvif_user: r.get("onvif_user")?,
            onvif_pass: r.get("onvif_pass")?,
            ptz: r.get::<_, i64>("ptz")? != 0,
            ptz_url: r.get("ptz_url")?,
            ptz_profile: r.get("ptz_profile")?,
            record_audio: r.get::<_, i64>("record_audio")? != 0,
        })
    }

    pub fn cameras(&self) -> Result<Vec<Camera>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT * FROM camera ORDER BY sort_order, id")?;
            let rows = st.query_map([], Self::row_camera)?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    pub fn camera(&self, id: i64) -> Result<Option<Camera>> {
        self.with(|c| {
            Ok(c.query_row("SELECT * FROM camera WHERE id=?1", params![id], Self::row_camera).optional()?)
        })
    }

    pub fn add_camera(&self, name: &str, main_url: &str, sub_url: Option<&str>, storage_id: i64) -> Result<i64> {
        self.with(|c| {
            c.execute(
                "INSERT INTO camera(name,main_url,sub_url,storage_id,created_at) VALUES(?1,?2,?3,?4,?5)",
                params![name, main_url, sub_url, storage_id, now_secs()],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    /// Update a subset of camera fields from a JSON object (admin API).
    pub fn update_camera(&self, id: i64, patch: &serde_json::Map<String, serde_json::Value>) -> Result<()> {
        const ALLOWED: &[&str] = &[
            "name", "main_url", "sub_url", "enabled", "storage_id", "retention_days", "detect_fps",
            "detect_width", "detect_height", "pixel_threshold", "min_area_pct", "min_blob_pct",
            "pre_secs", "post_secs", "cooldown_secs", "zones_json", "masks_json", "sort_order",
            "record_sub", "event_retention_days", "tags", "objects", "object_labels", "require_object",
            "onvif_url", "onvif_user", "onvif_pass", "ptz", "ptz_url", "ptz_profile", "record_audio",
        ];
        self.with(|c| {
            for (k, v) in patch {
                if !ALLOWED.contains(&k.as_str()) {
                    anyhow::bail!("field {k} is not updatable");
                }
                validate_camera_field(k, v)?;
                let sql = format!("UPDATE camera SET {k}=?1 WHERE id=?2");
                let val: rusqlite::types::Value = match v {
                    serde_json::Value::Null => rusqlite::types::Value::Null,
                    serde_json::Value::Bool(b) => rusqlite::types::Value::Integer(*b as i64),
                    serde_json::Value::Number(n) => {
                        if let Some(i) = n.as_i64() {
                            rusqlite::types::Value::Integer(i)
                        } else {
                            rusqlite::types::Value::Real(n.as_f64().unwrap_or(0.0))
                        }
                    }
                    serde_json::Value::String(s) => rusqlite::types::Value::Text(s.clone()),
                    other => rusqlite::types::Value::Text(other.to_string()),
                };
                c.execute(&sql, params![val, id])?;
            }
            Ok(())
        })
    }

    /// Delete a camera and its index rows; the caller removes files first
    /// (see retention::delete_camera_files).
    pub fn delete_camera(&self, id: i64) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM event WHERE camera_id=?1", params![id])?;
            c.execute("DELETE FROM segment WHERE camera_id=?1", params![id])?;
            c.execute("DELETE FROM user_camera WHERE camera_id=?1", params![id])?;
            c.execute("DELETE FROM camera WHERE id=?1", params![id])?;
            Ok(())
        })
    }

    pub fn segments_of_camera(&self, camera_id: i64, limit: usize) -> Result<Vec<Segment>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT * FROM segment WHERE camera_id=?1 ORDER BY start_dts LIMIT ?2")?;
            let rows = st.query_map(params![camera_id, limit as i64], Self::row_segment)?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    /// Atomically create the first admin; fails if any user already exists.
    pub fn add_first_admin(&self, username: &str, pass_hash: &str) -> Result<bool> {
        self.with(|c| {
            let n = c.execute(
                "INSERT INTO user(username,pass_hash,role,created_at) SELECT ?1,?2,'admin',?3 WHERE NOT EXISTS (SELECT 1 FROM user)",
                params![username, pass_hash, now_secs()],
            )?;
            Ok(n == 1)
        })
    }

    // ----- sample entries ------------------------------------------------------

    /// Deduplicated codec configuration; the hash covers the audio entry
    /// too, so video-only and audio+video variants are distinct rows.
    pub fn intern_sample_entry(&self, codec: mp4::Codec, rfc6381: &str, width: u32, height: u32, data: &[u8], audio: Option<&mp4::AudioParams>) -> Result<i64> {
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        h.update(data);
        if let Some(a) = audio {
            h.update(b"|audio|");
            h.update(&a.sample_entry);
        }
        let sha = hex::encode(h.finalize());
        self.with(|c| {
            if let Some(id) = c
                .query_row("SELECT id FROM sample_entry WHERE sha256=?1", params![sha], |r| r.get::<_, i64>(0))
                .optional()?
            {
                return Ok(id);
            }
            c.execute(
                "INSERT INTO sample_entry(sha256,codec,rfc6381,width,height,data,audio_entry,audio_rate,audio_channels,audio_rfc6381) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![sha, codec.to_string(), rfc6381, width, height, data,
                    audio.map(|a| a.sample_entry.to_vec()), audio.map(|a| a.sample_rate as i64), audio.map(|a| a.channels as i64), audio.map(|a| a.rfc6381.clone())],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    pub fn sample_entry(&self, id: i64) -> Result<Option<SampleEntry>> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT id,codec,rfc6381,width,height,data,audio_entry,audio_rate,audio_channels,audio_rfc6381 FROM sample_entry WHERE id=?1",
                params![id],
                |r| {
                    let audio_entry: Option<Vec<u8>> = r.get(6)?;
                    let audio = audio_entry.map(|e| mp4::AudioParams {
                        sample_entry: bytes::Bytes::from(e),
                        sample_rate: r.get::<_, Option<i64>>(7).ok().flatten().unwrap_or(48_000) as u32,
                        channels: r.get::<_, Option<i64>>(8).ok().flatten().unwrap_or(1) as u16,
                        rfc6381: r.get::<_, Option<String>>(9).ok().flatten().unwrap_or_else(|| "mp4a.40.2".into()),
                    });
                    Ok(SampleEntry {
                        id: r.get(0)?,
                        codec: r.get(1)?,
                        rfc6381: r.get(2)?,
                        width: r.get::<_, i64>(3)? as u32,
                        height: r.get::<_, i64>(4)? as u32,
                        data: r.get(5)?,
                        audio,
                    })
                },
            )
            .optional()?)
        })
    }

    // ----- segments ------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn insert_segment(
        &self,
        camera_id: i64,
        storage_id: i64,
        sample_entry_id: i64,
        path: &str,
        bytes: i64,
        init_len: u32,
        index: &[FragEntry],
    ) -> Result<i64> {
        self.insert_segment_ext(camera_id, storage_id, sample_entry_id, path, bytes, init_len, index, 0, None, "main")
    }

    /// `index` must already carry absolute dts values; `dts_offset` is what
    /// must be added to the file's own tfdt values to get there.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_segment_ext(
        &self,
        camera_id: i64,
        storage_id: i64,
        sample_entry_id: i64,
        path: &str,
        bytes: i64,
        init_len: u32,
        index: &[FragEntry],
        dts_offset: i64,
        zm_event_id: Option<i64>,
        stream: &str,
    ) -> Result<i64> {
        let start = index.first().map(|f| f.dts).unwrap_or(0);
        let end = index.last().map(|f| f.dts + f.duration as i64).unwrap_or(start);
        let frames: i64 = index.iter().map(|f| f.samples as i64).sum();
        let motion_max = index.iter().map(|f| f.motion).max().unwrap_or(0);
        self.with(|c| {
            c.execute(
                "INSERT INTO segment(camera_id,storage_id,sample_entry_id,start_dts,end_dts,path,bytes,init_len,frames,motion_max,index_blob,dts_offset,zm_event_id,stream,created_at) \
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
                params![camera_id, storage_id, sample_entry_id, start, end, path, bytes, init_len, frames, motion_max, mp4::encode_index(index), dts_offset, zm_event_id, stream, now_secs()],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    pub fn zm_event_imported(&self, zm_event_id: i64) -> Result<bool> {
        self.with(|c| {
            Ok(c.query_row("SELECT 1 FROM segment WHERE zm_event_id=?1 LIMIT 1", params![zm_event_id], |_| Ok(()))
                .optional()?
                .is_some())
        })
    }

    pub fn insert_event_full(&self, camera_id: i64, start_dts: i64, end_dts: i64, kind: &str, score: u8, peak_dts: Option<i64>, thumb_path: Option<&str>, meta_json: &str, archived: bool, notes: Option<&str>) -> Result<i64> {
        self.with(|c| {
            c.execute(
                "INSERT INTO event(camera_id,start_dts,end_dts,kind,score,peak_dts,thumb_path,meta_json,archived,notes,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![camera_id, start_dts, end_dts, kind, score as i64, peak_dts, thumb_path, meta_json, archived as i64, notes, now_secs()],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    fn row_segment(r: &rusqlite::Row) -> rusqlite::Result<Segment> {
        let blob: Vec<u8> = r.get("index_blob")?;
        Ok(Segment {
            id: r.get("id")?,
            camera_id: r.get("camera_id")?,
            storage_id: r.get("storage_id")?,
            sample_entry_id: r.get("sample_entry_id")?,
            start_dts: r.get("start_dts")?,
            end_dts: r.get("end_dts")?,
            path: r.get("path")?,
            bytes: r.get("bytes")?,
            init_len: r.get::<_, i64>("init_len")? as u32,
            frames: r.get("frames")?,
            motion_max: r.get::<_, i64>("motion_max")? as u8,
            dts_offset: r.get("dts_offset")?,
            zm_event_id: r.get("zm_event_id")?,
            stream: r.get("stream")?,
            index: mp4::decode_index(&blob),
        })
    }

    /// Main-stream segments of a camera overlapping [start, end).
    pub fn segments_in_range(&self, camera_id: i64, start: i64, end: i64) -> Result<Vec<Segment>> {
        self.segments_in_range_stream(camera_id, "main", start, end)
    }

    pub fn segments_in_range_stream(&self, camera_id: i64, stream: &str, start: i64, end: i64) -> Result<Vec<Segment>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT * FROM segment WHERE camera_id=?1 AND stream=?2 AND end_dts>?3 AND start_dts<?4 ORDER BY start_dts",
            )?;
            let rows = st.query_map(params![camera_id, stream, start, end], Self::row_segment)?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    pub fn segment(&self, id: i64) -> Result<Option<Segment>> {
        self.with(|c| Ok(c.query_row("SELECT * FROM segment WHERE id=?1", params![id], Self::row_segment).optional()?))
    }

    pub fn segment_paths_for_storage(&self, storage_id: i64) -> Result<std::collections::HashSet<String>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT path FROM segment WHERE storage_id=?1")?;
            let rows = st.query_map(params![storage_id], |r| r.get::<_, String>(0))?;
            Ok(rows.collect::<std::result::Result<_, _>>()?)
        })
    }

    /// Oldest segments on a storage (for retention).
    pub fn oldest_segments(&self, storage_id: i64, limit: usize) -> Result<Vec<Segment>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT * FROM segment WHERE storage_id=?1 ORDER BY start_dts LIMIT ?2")?;
            let rows = st.query_map(params![storage_id, limit as i64], Self::row_segment)?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    pub fn segments_older_than(&self, camera_id: i64, before_dts: i64, limit: usize) -> Result<Vec<Segment>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT * FROM segment WHERE camera_id=?1 AND end_dts<?2 ORDER BY start_dts LIMIT ?3",
            )?;
            let rows = st.query_map(params![camera_id, before_dts, limit as i64], Self::row_segment)?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    pub fn delete_segment(&self, id: i64) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM segment WHERE id=?1", params![id])?;
            Ok(())
        })
    }

    /// Coverage summary for the timeline: (start,end,motion_max) tuples.
    pub fn coverage(&self, camera_id: i64, start: i64, end: i64) -> Result<Vec<(i64, i64, u8)>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT start_dts,end_dts,motion_max FROM segment WHERE camera_id=?1 AND end_dts>?2 AND start_dts<?3 ORDER BY start_dts",
            )?;
            let rows = st.query_map(params![camera_id, start, end], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? as u8))
            })?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    pub fn earliest_segment_dts(&self, camera_id: Option<i64>) -> Result<Option<i64>> {
        self.with(|c| {
            let v: Option<i64> = match camera_id {
                Some(id) => c.query_row("SELECT MIN(start_dts) FROM segment WHERE camera_id=?1 AND stream='main'", params![id], |r| r.get(0))?,
                None => c.query_row("SELECT MIN(start_dts) FROM segment WHERE stream='main'", [], |r| r.get(0))?,
            };
            Ok(v)
        })
    }

    // ----- health queries (all index range scans / small group-bys) ------------

    /// Bytes recorded since `dts` per (camera, storage), main + sub.
    pub fn ingest_since(&self, dts: i64) -> Result<Vec<(i64, i64, i64)>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT camera_id, storage_id, SUM(bytes) FROM segment WHERE start_dts>=?1 GROUP BY camera_id, storage_id")?;
            let rows = st.query_map(params![dts], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    pub fn last_event_per_camera(&self) -> Result<std::collections::HashMap<i64, i64>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT camera_id, MAX(start_dts) FROM event GROUP BY camera_id")?;
            let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
            Ok(rows.collect::<std::result::Result<_, _>>()?)
        })
    }

    pub fn event_counts_since(&self, dts: i64) -> Result<std::collections::HashMap<i64, i64>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT camera_id, COUNT(*) FROM event WHERE start_dts>=?1 GROUP BY camera_id")?;
            let rows = st.query_map(params![dts], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
            Ok(rows.collect::<std::result::Result<_, _>>()?)
        })
    }

    pub fn oldest_segment_per_camera(&self) -> Result<std::collections::HashMap<i64, i64>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT camera_id, MIN(start_dts) FROM segment WHERE stream='main' GROUP BY camera_id")?;
            let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
            Ok(rows.collect::<std::result::Result<_, _>>()?)
        })
    }

    pub fn oldest_segment_per_storage(&self) -> Result<std::collections::HashMap<i64, i64>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT storage_id, MIN(start_dts) FROM segment GROUP BY storage_id")?;
            let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
            Ok(rows.collect::<std::result::Result<_, _>>()?)
        })
    }

    /// `PRAGMA integrity_check` result ("ok" when healthy).
    pub fn integrity_check(&self) -> Result<String> {
        self.with(|c| Ok(c.query_row("PRAGMA integrity_check", [], |r| r.get(0))?))
    }

    // ----- events --------------------------------------------------------------

    pub fn insert_event(&self, camera_id: i64, start_dts: i64, kind: &str) -> Result<i64> {
        self.with(|c| {
            c.execute(
                "INSERT INTO event(camera_id,start_dts,kind,created_at) VALUES(?1,?2,?3,?4)",
                params![camera_id, start_dts, kind, now_secs()],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    pub fn update_event_progress(&self, id: i64, score: u8, peak_dts: i64) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE event SET score=MAX(score,?1), peak_dts=CASE WHEN ?1>=score THEN ?2 ELSE peak_dts END WHERE id=?3",
                params![score as i64, peak_dts, id],
            )?;
            Ok(())
        })
    }

    pub fn close_event(&self, id: i64, end_dts: i64, thumb_path: Option<&str>, meta_json: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE event SET end_dts=?1, thumb_path=COALESCE(?2,thumb_path), meta_json=?3 WHERE id=?4",
                params![end_dts, thumb_path, meta_json, id],
            )?;
            Ok(())
        })
    }

    /// Record detected objects on an open or closed event.
    pub fn update_event_objects(&self, id: i64, kind: &str, meta_json: &str) -> Result<()> {
        self.with(|c| {
            c.execute("UPDATE event SET kind=?1, meta_json=?2 WHERE id=?3", params![kind, meta_json, id])?;
            Ok(())
        })
    }

    pub fn set_event_thumb(&self, id: i64, thumb_path: &str) -> Result<()> {
        self.with(|c| {
            c.execute("UPDATE event SET thumb_path=?1 WHERE id=?2", params![thumb_path, id])?;
            Ok(())
        })
    }

    fn row_event(r: &rusqlite::Row) -> rusqlite::Result<Event> {
        Ok(Event {
            id: r.get("id")?,
            camera_id: r.get("camera_id")?,
            start_dts: r.get("start_dts")?,
            end_dts: r.get("end_dts")?,
            kind: r.get("kind")?,
            score: r.get::<_, i64>("score")? as u8,
            peak_dts: r.get("peak_dts")?,
            thumb_path: r.get("thumb_path")?,
            archived: r.get::<_, i64>("archived")? != 0,
            notes: r.get("notes")?,
            meta_json: r.get::<_, Option<String>>("meta_json")?.unwrap_or_else(|| "{}".into()),
        })
    }

    pub fn event(&self, id: i64) -> Result<Option<Event>> {
        self.with(|c| Ok(c.query_row("SELECT * FROM event WHERE id=?1", params![id], Self::row_event).optional()?))
    }

    /// Event listing with keyset pagination (newest first).
    #[allow(clippy::too_many_arguments)]
    pub fn events(
        &self,
        cameras: Option<&[i64]>,
        start: Option<i64>,
        end: Option<i64>,
        min_score: u8,
        archived_only: bool,
        before_id: Option<i64>,
        limit: usize,
        ascending: bool,
        kinds: Option<&[String]>,
    ) -> Result<Vec<Event>> {
        let mut sql = String::from("SELECT * FROM event WHERE end_dts IS NOT NULL AND score>=?1");
        let mut args: Vec<rusqlite::types::Value> = vec![(min_score as i64).into()];
        if let Some(ks) = kinds {
            if ks.is_empty() {
                return Ok(Vec::new());
            }
            let mut ph = Vec::new();
            for k in ks {
                args.push(k.to_lowercase().into());
                ph.push(format!("?{}", args.len()));
            }
            sql.push_str(&format!(" AND kind IN ({})", ph.join(",")));
        }
        if let Some(cs) = cameras {
            if cs.is_empty() {
                return Ok(Vec::new());
            }
            sql.push_str(" AND camera_id IN (");
            sql.push_str(&cs.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(","));
            sql.push(')');
        }
        if let Some(s) = start {
            args.push(s.into());
            sql.push_str(&format!(" AND end_dts>=?{}", args.len()));
        }
        if let Some(e) = end {
            args.push(e.into());
            sql.push_str(&format!(" AND start_dts<?{}", args.len()));
        }
        if archived_only {
            sql.push_str(" AND archived=1");
        }
        if let Some(b) = before_id {
            // keyset on (start_dts, id) so ORDER BY start_dts never skips/duplicates
            args.push(b.into());
            let n = args.len();
            let op = if ascending { ">" } else { "<" };
            sql.push_str(&format!(
                " AND (start_dts {op} (SELECT start_dts FROM event WHERE id=?{n}) OR (start_dts = (SELECT start_dts FROM event WHERE id=?{n}) AND id {op} ?{n}))"
            ));
        }
        sql.push_str(if ascending { " ORDER BY start_dts ASC, id ASC" } else { " ORDER BY start_dts DESC, id DESC" });
        args.push((limit as i64).into());
        sql.push_str(&format!(" LIMIT ?{}", args.len()));
        self.with(|c| {
            let mut st = c.prepare(&sql)?;
            let rows = st.query_map(rusqlite::params_from_iter(args.iter()), Self::row_event)?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    pub fn events_in_range(&self, camera_id: i64, start: i64, end: i64) -> Result<Vec<Event>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT * FROM event WHERE camera_id=?1 AND end_dts IS NOT NULL AND end_dts>?2 AND start_dts<?3 ORDER BY start_dts",
            )?;
            let rows = st.query_map(params![camera_id, start, end], Self::row_event)?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    pub fn set_event_archived(&self, id: i64, archived: bool) -> Result<()> {
        self.with(|c| {
            c.execute("UPDATE event SET archived=?1 WHERE id=?2", params![archived as i64, id])?;
            Ok(())
        })
    }

    pub fn set_event_notes(&self, id: i64, notes: &str) -> Result<()> {
        self.with(|c| {
            c.execute("UPDATE event SET notes=?1 WHERE id=?2", params![notes, id])?;
            Ok(())
        })
    }

    pub fn delete_event(&self, id: i64) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM event WHERE id=?1", params![id])?;
            Ok(())
        })
    }

    /// Events whose recording is entirely gone (older than the camera's
    /// oldest segment) and that are not archived.
    pub fn orphan_events(&self, camera_id: i64, before_dts: i64, limit: usize) -> Result<Vec<Event>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT * FROM event WHERE camera_id=?1 AND archived=0 AND end_dts IS NOT NULL AND end_dts<?2 ORDER BY start_dts LIMIT ?3",
            )?;
            let rows = st.query_map(params![camera_id, before_dts, limit as i64], Self::row_event)?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    /// Close any events left open by a crash.
    pub fn close_stale_events(&self) -> Result<usize> {
        self.with(|c| {
            Ok(c.execute(
                "UPDATE event SET end_dts=COALESCE(peak_dts,start_dts)+90000*5 WHERE end_dts IS NULL",
                [],
            )?)
        })
    }

    /// Offset-paged event query for the ZoneMinder-compatible API.
    pub fn events_page(&self, cameras: &[i64], f: &crate::zmapi::EventFilter, page: usize, limit: usize, asc: bool) -> Result<(Vec<Event>, usize)> {
        if cameras.is_empty() {
            return Ok((Vec::new(), 0));
        }
        let mut wh = format!("end_dts IS NOT NULL AND camera_id IN ({})", cameras.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(","));
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(v) = f.start_ge { args.push(v.into()); wh.push_str(&format!(" AND start_dts>=?{}", args.len())); }
        if let Some(v) = f.start_le { args.push(v.into()); wh.push_str(&format!(" AND start_dts<=?{}", args.len())); }
        if let Some(v) = f.end_le { args.push(v.into()); wh.push_str(&format!(" AND end_dts<=?{}", args.len())); }
        if f.min_score > 0 { args.push((f.min_score as i64).into()); wh.push_str(&format!(" AND score>=?{}", args.len())); }
        if let Some(a) = f.archived { args.push((a as i64).into()); wh.push_str(&format!(" AND archived=?{}", args.len())); }
        if !f.ids.is_empty() { wh.push_str(&format!(" AND id IN ({})", f.ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(","))); }
        if let Some(re) = &f.notes_re { args.push(format!("%{}%", re.replace("detected:", "")).into()); wh.push_str(&format!(" AND (COALESCE(notes,'') LIKE ?{n} OR kind LIKE ?{n})", n = args.len())); }
        self.with(|c| {
            let total: i64 = c.query_row(&format!("SELECT COUNT(*) FROM event WHERE {wh}"), rusqlite::params_from_iter(args.iter()), |r| r.get(0))?;
            let sql = format!(
                "SELECT * FROM event WHERE {wh} ORDER BY start_dts {}, id {} LIMIT {} OFFSET {}",
                if asc { "ASC" } else { "DESC" }, if asc { "ASC" } else { "DESC" }, limit, (page - 1) * limit
            );
            let mut st = c.prepare(&sql)?;
            let rows = st.query_map(rusqlite::params_from_iter(args.iter()), Self::row_event)?;
            Ok((rows.collect::<std::result::Result<Vec<_>, _>>()?, total as usize))
        })
    }

    // ----- push tokens (zmNinjaNg direct notifications) -------------------------

    pub fn push_tokens(&self, user_id: i64) -> Result<Vec<serde_json::Value>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT id,token,platform,monitor_list,interval,push_state,app_version,profile,created_at,updated_at FROM push_token WHERE user_id=?1 ORDER BY id")?;
            let rows = st.query_map(params![user_id], |r| {
                Ok(serde_json::json!({
                    "Id": r.get::<_, i64>(0)?.to_string(), "UserId": user_id.to_string(), "Token": r.get::<_, String>(1)?,
                    "Platform": r.get::<_, String>(2)?, "MonitorList": r.get::<_, String>(3)?, "Interval": r.get::<_, i64>(4)?.to_string(),
                    "PushState": r.get::<_, String>(5)?, "AppVersion": r.get::<_, String>(6)?, "Profile": r.get::<_, String>(7)?,
                    "BadgeCount": "0", "LastNotifiedAt": null, "CreatedOn": r.get::<_, i64>(8)?.to_string(), "UpdatedOn": r.get::<_, i64>(9)?.to_string()
                }))
            })?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn upsert_push_token(&self, user_id: i64, token: &str, platform: &str, monitor_list: &str, interval: i64, push_state: &str, app_version: &str, profile: &str) -> Result<i64> {
        if token.is_empty() {
            anyhow::bail!("token required");
        }
        self.with(|c| {
            let now = now_secs();
            c.execute(
                "INSERT INTO push_token(user_id,token,platform,monitor_list,interval,push_state,app_version,profile,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?9) \
                 ON CONFLICT(token) DO UPDATE SET user_id=excluded.user_id, platform=excluded.platform, monitor_list=excluded.monitor_list, interval=excluded.interval, push_state=excluded.push_state, app_version=excluded.app_version, profile=excluded.profile, updated_at=excluded.updated_at",
                params![user_id, token, platform, monitor_list, interval, push_state, app_version, profile, now],
            )?;
            Ok(c.query_row("SELECT id FROM push_token WHERE token=?1", params![token], |r| r.get(0))?)
        })
    }

    pub fn update_push_token(&self, user_id: i64, id: i64, monitor_list: Option<String>, interval: Option<i64>, push_state: Option<String>, profile: Option<String>) -> Result<()> {
        self.with(|c| {
            if let Some(v) = monitor_list { c.execute("UPDATE push_token SET monitor_list=?1, updated_at=?2 WHERE id=?3 AND user_id=?4", params![v, now_secs(), id, user_id])?; }
            if let Some(v) = interval { c.execute("UPDATE push_token SET interval=?1, updated_at=?2 WHERE id=?3 AND user_id=?4", params![v, now_secs(), id, user_id])?; }
            if let Some(v) = push_state { c.execute("UPDATE push_token SET push_state=?1, updated_at=?2 WHERE id=?3 AND user_id=?4", params![v, now_secs(), id, user_id])?; }
            if let Some(v) = profile { c.execute("UPDATE push_token SET profile=?1, updated_at=?2 WHERE id=?3 AND user_id=?4", params![v, now_secs(), id, user_id])?; }
            Ok(())
        })
    }

    pub fn delete_push_token(&self, user_id: i64, id: i64) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM push_token WHERE id=?1 AND user_id=?2", params![id, user_id])?;
            Ok(())
        })
    }

    // ----- users / sessions ----------------------------------------------------

    pub fn user_count(&self) -> Result<i64> {
        self.with(|c| Ok(c.query_row("SELECT COUNT(*) FROM user", [], |r| r.get(0))?))
    }

    pub fn add_user(&self, username: &str, pass_hash: &str, role: &str) -> Result<i64> {
        self.with(|c| {
            c.execute(
                "INSERT INTO user(username,pass_hash,role,created_at) VALUES(?1,?2,?3,?4)",
                params![username, pass_hash, role, now_secs()],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    pub fn users(&self) -> Result<Vec<User>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT id,username,role,pass_hash FROM user ORDER BY id")?;
            let rows = st.query_map([], |r| {
                Ok(User { id: r.get(0)?, username: r.get(1)?, role: r.get(2)?, pass_hash: r.get(3)? })
            })?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    pub fn user_by_name(&self, username: &str) -> Result<Option<User>> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT id,username,role,pass_hash FROM user WHERE username=?1",
                params![username],
                |r| Ok(User { id: r.get(0)?, username: r.get(1)?, role: r.get(2)?, pass_hash: r.get(3)? }),
            )
            .optional()?)
        })
    }

    pub fn delete_user(&self, id: i64) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM user WHERE id=?1", params![id])?;
            Ok(())
        })
    }

    pub fn set_user_cameras(&self, user_id: i64, cameras: &[i64]) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM user_camera WHERE user_id=?1", params![user_id])?;
            for cam in cameras {
                c.execute("INSERT OR IGNORE INTO user_camera(user_id,camera_id) VALUES(?1,?2)", params![user_id, cam])?;
            }
            Ok(())
        })
    }

    pub fn user_cameras(&self, user_id: i64) -> Result<Vec<i64>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT camera_id FROM user_camera WHERE user_id=?1")?;
            let rows = st.query_map(params![user_id], |r| r.get::<_, i64>(0))?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    pub fn create_session(&self, user_id: i64, token: &str, ttl_secs: i64) -> Result<()> {
        self.with(|c| {
            let now = now_secs();
            c.execute(
                "INSERT INTO session(token,user_id,created_at,expires_at) VALUES(?1,?2,?3,?4)",
                params![token, user_id, now, now + ttl_secs],
            )?;
            c.execute("DELETE FROM session WHERE expires_at<?1", params![now])?;
            Ok(())
        })
    }

    pub fn session_user(&self, token: &str) -> Result<Option<User>> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT u.id,u.username,u.role,u.pass_hash FROM session s JOIN user u ON u.id=s.user_id WHERE s.token=?1 AND s.expires_at>?2",
                params![token, now_secs()],
                |r| Ok(User { id: r.get(0)?, username: r.get(1)?, role: r.get(2)?, pass_hash: r.get(3)? }),
            )
            .optional()?)
        })
    }

    /// Current row of a user (None once deleted).
    pub fn session_user_by_id(&self, id: i64) -> Result<Option<User>> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT id,username,role,pass_hash FROM user WHERE id=?1",
                params![id],
                |r| Ok(User { id: r.get(0)?, username: r.get(1)?, role: r.get(2)?, pass_hash: r.get(3)? }),
            )
            .optional()?)
        })
    }

    pub fn delete_session(&self, token: &str) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM session WHERE token=?1", params![token])?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_and_basic_roundtrip() {
        let dir = tempdir();
        let db = Db::open(&dir.join("t.db")).unwrap();
        let sid = db.add_storage("/tmp/x", None, 0).unwrap();
        let cid = db.add_camera("cam", "rtsp://x/1", Some("rtsp://x/2"), sid).unwrap();
        let se = db.intern_sample_entry(mp4::Codec::H265, "hvc1.1.6.L153.B0", 2688, 1520, b"abc", None).unwrap();
        let se2 = db.intern_sample_entry(mp4::Codec::H265, "hvc1.1.6.L153.B0", 2688, 1520, b"abc", None).unwrap();
        assert_eq!(se, se2);
        let audio = mp4::AudioParams { sample_entry: bytes::Bytes::from_static(b"mp4a"), sample_rate: 16_000, channels: 2, rfc6381: "mp4a.40.2".into() };
        let se3 = db.intern_sample_entry(mp4::Codec::H265, "hvc1.1.6.L153.B0", 2688, 1520, b"abc", Some(&audio)).unwrap();
        assert_ne!(se, se3);
        let got = db.sample_entry(se3).unwrap().unwrap();
        assert_eq!(got.audio.as_ref().map(|a| (a.sample_rate, a.channels)), Some((16_000, 2)));
        assert!(db.sample_entry(se).unwrap().unwrap().audio.is_none());
        let idx = vec![
            FragEntry { dts: 1000, duration: 180_000, offset: 900, len: 5000, samples: 40, motion: 3 },
            FragEntry { dts: 181_000, duration: 180_000, offset: 5900, len: 5000, samples: 40, motion: 200 },
        ];
        let seg = db.insert_segment(cid, sid, se, "1/x.mp4", 10900, 900, &idx).unwrap();
        let got = db.segment(seg).unwrap().unwrap();
        assert_eq!(got.start_dts, 1000);
        assert_eq!(got.end_dts, 361_000);
        assert_eq!(got.frames, 80);
        assert_eq!(got.motion_max, 200);
        assert_eq!(got.index, idx);
        assert_eq!(db.segments_in_range(cid, 0, 1500).unwrap().len(), 1);
        assert_eq!(db.segments_in_range(cid, 0, 500).unwrap().len(), 0);
        assert_eq!(db.segments_in_range(cid, 361_000, 400_000).unwrap().len(), 0);
        let ev = db.insert_event(cid, 5000, "motion").unwrap();
        db.update_event_progress(ev, 50, 6000).unwrap();
        db.update_event_progress(ev, 40, 7000).unwrap();
        db.close_event(ev, 9000, Some("1/e.jpg"), "{}").unwrap();
        let e = db.event(ev).unwrap().unwrap();
        assert_eq!(e.score, 50);
        assert_eq!(e.peak_dts, Some(6000));
        assert_eq!(db.events(Some(&[cid]), None, None, 0, false, None, 10, false, None).unwrap().len(), 1);
        assert_eq!(db.events(Some(&[cid]), None, None, 60, false, None, 10, false, None).unwrap().len(), 0);
        db.update_event_objects(ev, "person", r#"{"objects":[{"label":"person","confidence":0.9,"bbox":[0,0,1,1]}]}"#).unwrap();
        assert_eq!(db.events(Some(&[cid]), None, None, 0, false, None, 10, false, Some(&["person".to_string()])).unwrap().len(), 1);
        assert_eq!(db.events(Some(&[cid]), None, None, 0, false, None, 10, false, Some(&["motion".to_string()])).unwrap().len(), 0);
        assert_eq!(db.events(Some(&[cid]), None, None, 0, false, None, 10, false, Some(&[])).unwrap().len(), 0);
        assert_eq!(db.event(ev).unwrap().unwrap().kind, "person");
        let _ = std::fs::remove_dir_all(dir);
    }

    fn tempdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("zmng-test-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
