//! Status report, Prometheus metrics, doctor and backup/restore.
mod common;
use common::*;
use serde_json::json;
use zmng::mp4::TIMESCALE;

fn dts(secs: i64) -> i64 { secs * TIMESCALE as i64 }

#[tokio::test]
async fn status_reports_cameras_storage_headroom_and_issues() {
    let fx = Fixture::new();
    let cam = fx.add_camera("front");
    let cam2 = fx.add_camera("back");
    let enc = parse_encoded(&encode_fmp4(2, "libx264", 160, 90));
    let now = zmng::db::now_secs();
    // 3 segments in the last 24 h for cam 1, one older
    for i in 0..3 { fx.write_segment(cam, "main", &enc, dts(now - 3600 * (i + 1)), |_| 0); }
    fx.write_segment(cam, "main", &enc, dts(now - 3 * 86_400), |_| 0);
    let ev = fx.db.insert_event(cam, dts(now - 600), "motion").unwrap();
    fx.db.close_event(ev, dts(now - 590), None, "{}").unwrap();
    let admin = fx.add_admin("admin", "password123");
    let viewer = fx.add_viewer("v", "password123", &[cam2]);
    let (at, vt) = (fx.token_for(admin), fx.token_for(viewer));
    let r = fx.router();
    let s = get(&r, "/api/status", &at).await;
    assert_eq!(s.status, 200, "{}", s.text());
    let s = s.json();
    assert_eq!(s["cameras"].as_array().unwrap().len(), 2);
    let c = &s["cameras"][0];
    assert_eq!(c["name"], "front");
    assert_eq!(c["recording"], false);
    assert_eq!(c["events_24h"], 1);
    assert!(c["last_event_ms"].as_i64().unwrap() > 0);
    let day_ago = dts(now - 86_400);
    let recent: i64 = fx.db.segments_in_range(cam, day_ago, i64::MAX).unwrap().iter().filter(|s| s.start_dts >= day_ago).map(|s| s.bytes).sum();
    let total: i64 = fx.db.segments_in_range(cam, 0, i64::MAX).unwrap().iter().map(|s| s.bytes).sum();
    assert!(recent > 0 && recent < total);
    assert_eq!(c["ingest_24h"].as_i64().unwrap(), recent);
    assert!(c["oldest_ms"].as_i64().unwrap() < (now - 2 * 86_400) * 1000);
    // storage: headroom = free - reserve (no cap); effective days = (used + headroom) / ingest
    let st = &s["storages"][0];
    assert_eq!(st["available"], true);
    assert_eq!(st["ingest_24h"].as_i64().unwrap(), recent);
    assert_eq!(st["used_bytes"].as_i64().unwrap(), total);
    let expect_days = (total + st["headroom_bytes"].as_i64().unwrap()) as f64 / recent as f64;
    assert!((st["effective_days"].as_f64().unwrap() - expect_days).abs() < 1e-6);
    // both cameras are enabled but not recording (no RTSP here) => two errors; detectors not running => two warnings
    let issues = s["issues"].as_array().unwrap();
    assert_eq!(issues.iter().filter(|i| i["level"] == "error").count(), 2);
    assert_eq!(issues.iter().filter(|i| i["level"] == "warn").count(), 2);
    assert!(s["uptime_secs"].as_i64().unwrap() >= 0);
    // viewers: only their cameras, no storages, no error strings
    let v = get(&r, "/api/status", &vt).await.json();
    assert_eq!(v["cameras"].as_array().unwrap().len(), 1);
    assert_eq!(v["cameras"][0]["name"], "back");
    assert!(v["storages"].as_array().unwrap().is_empty());
    assert_eq!(get(&r, "/api/metrics", &vt).await.status, 403);
    let m = get(&r, "/api/metrics", &at).await;
    assert_eq!(m.status, 200);
    assert!(m.header("content-type").unwrap().starts_with("text/plain"));
    let text = m.text();
    assert!(text.contains(&format!("zmng_camera_events_24h{{camera=\"{cam}\",name=\"front\"}} 1\n")), "{text}");
    assert!(text.contains("zmng_storage_effective_days{storage=\"1\""));
}

#[tokio::test]
async fn doctor_checks_the_environment() {
    let fx = Fixture::new();
    // no storage writable problem, no cameras yet
    let checks = zmng::health::doctor(&fx.cfg, false).await;
    let by = |n: &str| checks.iter().find(|c| c.name == n).unwrap_or_else(|| panic!("no check {n}: {checks:?}"));
    assert_eq!(by("ffmpeg").verdict, zmng::health::Verdict::Pass);
    assert_eq!(by("clock").verdict, zmng::health::Verdict::Pass);
    assert_eq!(by("database").verdict, zmng::health::Verdict::Pass);
    assert_eq!(by("web_dir").verdict, zmng::health::Verdict::Pass);
    assert_eq!(by("storage 1").verdict, zmng::health::Verdict::Pass);
    assert_eq!(by("users").verdict, zmng::health::Verdict::Warn);
    assert_eq!(by("cameras").verdict, zmng::health::Verdict::Warn);
    // a camera with a non-RTSP URL, a missing ffmpeg and a missing web dir
    fx.db.add_camera("bad", "http://not-rtsp/", None, fx.storage_id).unwrap();
    let mut cfg = fx.cfg.clone();
    cfg.ffmpeg = "/nonexistent/ffmpeg".into();
    cfg.web_dir = fx.dir.path().join("nope");
    let checks = zmng::health::doctor(&cfg, false).await;
    let fails: Vec<&str> = checks.iter().filter(|c| c.verdict == zmng::health::Verdict::Fail).map(|c| c.name.as_str()).collect();
    assert!(fails.contains(&"ffmpeg"), "{fails:?}");
    assert!(fails.contains(&"web_dir"));
    assert!(fails.iter().any(|f| f.starts_with("camera ")));
    let cam_fail: Vec<_> = checks.iter().filter(|c| c.name.starts_with("camera ") && c.verdict == zmng::health::Verdict::Fail).collect();
    assert_eq!(cam_fail.len(), 1, "{cam_fail:?}");
    assert!(cam_fail[0].detail.contains("must be rtsp"));
    // an unreachable camera probe fails fast and never prints the password
    let cam = fx.db.add_camera("probe", "rtsp://user:secretpw@127.0.0.1:1/stream", None, fx.storage_id).unwrap();
    let checks = zmng::health::doctor(&fx.cfg, true).await;
    let p = checks.iter().find(|c| c.name == format!("camera {cam} rtsp")).unwrap();
    assert_eq!(p.verdict, zmng::health::Verdict::Fail);
    assert!(!p.detail.contains("secretpw"), "{}", p.detail);
}

#[test]
fn backup_and_restore_roundtrip() {
    let fx = Fixture::new();
    let cam = fx.add_camera("front");
    fx.add_admin("admin", "password123");
    let dest = fx.dir.path().join("backups").join("zmng.db.backup");
    zmng::health::backup(&fx.cfg.db_path, &dest).unwrap();
    assert!(dest.exists());
    // second backup keeps the previous copy
    fx.add_camera("second");
    zmng::health::backup(&fx.cfg.db_path, &dest).unwrap();
    assert!(fx.dir.path().join("backups").join("zmng.db.backup.1").exists());
    // restore into a fresh location and read it back
    let target = fx.dir.path().join("restored.db");
    zmng::health::restore(&dest, &target).unwrap();
    let db2 = zmng::db::Db::open(&target).unwrap();
    assert_eq!(db2.cameras().unwrap().len(), 2);
    assert_eq!(db2.camera(cam).unwrap().unwrap().name, "front");
    assert_eq!(db2.user_count().unwrap(), 1);
    // restoring over an existing index keeps the old one aside
    zmng::health::restore(&dest, &target).unwrap();
    assert!(fx.dir.path().join("restored.db.before-restore").exists());
    // garbage is refused
    let junk = fx.dir.path().join("junk.db");
    std::fs::write(&junk, b"not a database").unwrap();
    assert!(zmng::health::restore(&junk, &target).is_err());
    let other = fx.dir.path().join("other.db");
    rusqlite::Connection::open(&other).unwrap().execute_batch("CREATE TABLE x(a);").unwrap();
    let e = zmng::health::restore(&other, &target).unwrap_err().to_string();
    assert!(e.contains("not a zmng index"), "{e}");
    // backup_path honours backup_dir
    let mut cfg = fx.cfg.clone();
    assert!(zmng::health::backup_path(&cfg).ends_with("zmng.db.backup"));
    cfg.backup_dir = Some(std::path::PathBuf::from("/mnt/archive/zmng"));
    assert_eq!(zmng::health::backup_path(&cfg), std::path::PathBuf::from("/mnt/archive/zmng/zmng.db.backup"));
    // an unmounted backup volume is an error, not a silent copy onto the root filesystem
    let e = zmng::health::backup(&fx.cfg.db_path, &fx.dir.path().join("not-mounted").join("zmng-backups").join("zmng.db.backup")).unwrap_err().to_string();
    assert!(e.contains("not available"), "{e}");
    let _ = json!({});
}

/// The kept copy must contain everything, including rows that were still in
/// the WAL, and restore must refuse while the service holds its lock.
#[test]
fn restore_keeps_wal_contents_and_respects_the_service_lock() {
    let fx = Fixture::new();
    let dest = fx.dir.path().join("zmng.db.backup");
    zmng::health::backup(&fx.cfg.db_path, &dest).unwrap();
    // rows written after the backup live in the WAL until a checkpoint
    fx.add_camera("after-backup");
    assert!(std::fs::metadata(format!("{}-wal", fx.cfg.db_path.display())).map(|m| m.len() > 0).unwrap_or(false), "expected a non-empty WAL");
    // running service => locked
    let lock = zmng::health::ServiceLock::acquire(&fx.cfg.db_path).unwrap();
    let e = zmng::health::ServiceLock::acquire(&fx.cfg.db_path).unwrap_err().to_string();
    assert!(e.contains("service is running"), "{e}");
    drop(lock);
    let _lock = zmng::health::ServiceLock::acquire(&fx.cfg.db_path).unwrap();
    drop(fx.db.clone());
    zmng::health::restore(&dest, &fx.cfg.db_path).unwrap();
    // the restored index has no cameras; the kept copy has the WAL-only camera
    let restored = zmng::db::Db::open(&fx.cfg.db_path).unwrap();
    assert!(restored.cameras().unwrap().is_empty());
    let kept = zmng::db::Db::open(&fx.cfg.db_path.with_extension("db.before-restore")).unwrap();
    assert_eq!(kept.cameras().unwrap().len(), 1);
    assert_eq!(kept.cameras().unwrap()[0].name, "after-backup");
    // same file as source and destination is refused
    assert!(zmng::health::restore(&fx.cfg.db_path, &fx.cfg.db_path).is_err());
}

/// doctor never writes: a database it cannot migrate is reported, not changed.
#[tokio::test]
async fn doctor_is_read_only() {
    let fx = Fixture::new();
    fx.add_camera("c");
    let wal = format!("{}-wal", fx.cfg.db_path.display());
    // checkpoint so the WAL is empty, snapshot the file, then make sure doctor changes neither
    fx.db.with(|c| { c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?; Ok(()) }).unwrap();
    let before = std::fs::read(&fx.cfg.db_path).unwrap();
    let wal_len = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
    let mut cfg = fx.cfg.clone();
    cfg.thumb_dir = fx.dir.path().join("no-thumbs-yet");
    let checks = zmng::health::doctor(&cfg, false).await;
    assert!(!cfg.thumb_dir.exists(), "doctor must not create directories");
    assert_eq!(std::fs::read(&fx.cfg.db_path).unwrap(), before);
    assert_eq!(std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0), wal_len);
    let t = checks.iter().find(|c| c.name == "thumb_dir").unwrap();
    assert_eq!(t.verdict, zmng::health::Verdict::Warn);
    // a missing database is a warning (fresh install), not a crash or a created file
    cfg.db_path = fx.dir.path().join("nope.db");
    let checks = zmng::health::doctor(&cfg, false).await;
    assert_eq!(checks.iter().find(|c| c.name == "database").unwrap().verdict, zmng::health::Verdict::Warn);
    assert!(!cfg.db_path.exists());
}
