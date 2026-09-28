//! Schema upgrades: a database created by the first phase-1 build must open
//! and work with today's code.
mod common;

/// The camera/segment/storage tables as the first build created them
/// (before stream, tags, record_sub, event_retention_days, tiering,
/// objects and read_only were added).
const V1: &str = r#"
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE storage (id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, max_bytes INTEGER, reserve_bytes INTEGER NOT NULL DEFAULT 21474836480);
CREATE TABLE camera (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, main_url TEXT NOT NULL, sub_url TEXT, enabled INTEGER NOT NULL DEFAULT 1,
  storage_id INTEGER NOT NULL REFERENCES storage(id), retention_days REAL NOT NULL DEFAULT 30, detect_fps REAL NOT NULL DEFAULT 5,
  detect_width INTEGER NOT NULL DEFAULT 320, detect_height INTEGER NOT NULL DEFAULT 180, pixel_threshold INTEGER NOT NULL DEFAULT 25,
  min_area_pct REAL NOT NULL DEFAULT 1.0, min_blob_pct REAL NOT NULL DEFAULT 0.5, pre_secs REAL NOT NULL DEFAULT 5, post_secs REAL NOT NULL DEFAULT 8,
  cooldown_secs REAL NOT NULL DEFAULT 10, zones_json TEXT NOT NULL DEFAULT '[]', masks_json TEXT NOT NULL DEFAULT '[]', sort_order INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL);
CREATE TABLE sample_entry (id INTEGER PRIMARY KEY, sha256 TEXT NOT NULL UNIQUE, codec TEXT NOT NULL, rfc6381 TEXT NOT NULL, width INTEGER NOT NULL, height INTEGER NOT NULL, data BLOB NOT NULL);
CREATE TABLE segment (id INTEGER PRIMARY KEY, camera_id INTEGER NOT NULL REFERENCES camera(id), storage_id INTEGER NOT NULL REFERENCES storage(id),
  sample_entry_id INTEGER NOT NULL REFERENCES sample_entry(id), start_dts INTEGER NOT NULL, end_dts INTEGER NOT NULL, path TEXT NOT NULL, bytes INTEGER NOT NULL,
  init_len INTEGER NOT NULL, frames INTEGER NOT NULL, motion_max INTEGER NOT NULL DEFAULT 0, index_blob BLOB NOT NULL, created_at INTEGER NOT NULL);
CREATE TABLE event (id INTEGER PRIMARY KEY, camera_id INTEGER NOT NULL REFERENCES camera(id), start_dts INTEGER NOT NULL, end_dts INTEGER, kind TEXT NOT NULL DEFAULT 'motion',
  score INTEGER NOT NULL DEFAULT 0, peak_dts INTEGER, thumb_path TEXT, meta_json TEXT NOT NULL DEFAULT '{}', archived INTEGER NOT NULL DEFAULT 0, notes TEXT, created_at INTEGER NOT NULL);
CREATE TABLE user (id INTEGER PRIMARY KEY, username TEXT NOT NULL UNIQUE, pass_hash TEXT NOT NULL, role TEXT NOT NULL DEFAULT 'viewer', created_at INTEGER NOT NULL);
CREATE TABLE user_camera (user_id INTEGER NOT NULL REFERENCES user(id) ON DELETE CASCADE, camera_id INTEGER NOT NULL REFERENCES camera(id) ON DELETE CASCADE, PRIMARY KEY (user_id, camera_id));
CREATE TABLE session (token TEXT PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES user(id) ON DELETE CASCADE, created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL);
INSERT INTO storage(path) VALUES('/old');
INSERT INTO camera(name, main_url, storage_id, created_at) VALUES('old cam', 'rtsp://x/1', 1, 1);
INSERT INTO event(camera_id, start_dts, end_dts, score, created_at) VALUES(1, 100, 200, 50, 1);
"#;

#[test]
fn phase1_database_upgrades_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.db");
    rusqlite::Connection::open(&path).unwrap().execute_batch(V1).unwrap();
    let db = zmng::db::Db::open(&path).unwrap();
    // old rows are intact and get the new defaults
    let cams = db.cameras().unwrap();
    assert_eq!(cams.len(), 1);
    let c = &cams[0];
    assert_eq!(c.name, "old cam");
    assert!(c.record_sub && c.objects && !c.require_object);
    assert_eq!(c.tags, "");
    assert_eq!(c.event_retention_days, 0.0);
    let st = db.storage(1).unwrap().unwrap();
    assert!(!st.read_only && st.archive_to.is_none());
    assert_eq!(db.event(1).unwrap().unwrap().score, 50);
    // every new column is writable and the new indexes exist
    db.update_camera(1, serde_json::json!({"tags": "Outside", "record_sub": false, "objects": false, "require_object": true, "object_labels": "person", "event_retention_days": 3}).as_object().unwrap()).unwrap();
    db.update_storage(1, serde_json::json!({"read_only": true, "archive_after_days": 2.0}).as_object().unwrap()).unwrap();
    let se = db.intern_sample_entry(zmng::mp4::Codec::H264, "avc1.64001e", 640, 360, b"x").unwrap();
    let idx = vec![zmng::mp4::FragEntry { dts: 1000, duration: 90_000, offset: 0, len: 10, samples: 1, motion: 0 }];
    db.insert_segment_ext(1, 1, se, "1/sub/x.mp4", 10, 0, &idx, 0, None, "sub").unwrap();
    assert_eq!(db.segments_in_range_stream(1, "sub", 0, i64::MAX).unwrap().len(), 1);
    assert_eq!(db.segments_in_range(1, 0, i64::MAX).unwrap().len(), 0);
    db.update_event_objects(1, "person", "{}").unwrap();
    assert_eq!(db.events(Some(&[1]), None, None, 0, false, None, 10, false, Some(&["person".to_string()])).unwrap().len(), 1);
    // opening again is idempotent
    drop(db);
    let db = zmng::db::Db::open(&path).unwrap();
    assert_eq!(db.cameras().unwrap()[0].tags, "Outside");
}
