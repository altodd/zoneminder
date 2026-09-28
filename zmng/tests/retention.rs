//! Retention, event pinning, tiering and crash re-adoption against real files.
mod common;
use common::*;
use zmng::mp4::TIMESCALE;

fn dts(secs: i64) -> i64 { secs * TIMESCALE as i64 }
fn now_secs() -> i64 { zmng::db::now_secs() }

#[test]
fn age_retention_deletes_old_segments_but_keeps_event_footage() {
    let fx = Fixture::new();
    let cam = fx.add_camera("c");
    fx.db.update_camera(cam, serde_json::json!({"retention_days": 1, "event_retention_days": 5}).as_object().unwrap()).unwrap();
    let enc = parse_encoded(&encode_fmp4(2, "libx264", 160, 90));
    let old = now_secs() - 3 * 86_400; // 3 days old: past retention, inside event retention
    let older = now_secs() - 10 * 86_400;
    let s_old = fx.write_segment(cam, "main", &enc, dts(old), |_| 0);
    let s_older = fx.write_segment(cam, "main", &enc, dts(older), |_| 0);
    let s_new = fx.write_segment(cam, "main", &enc, dts(now_secs() - 60), |_| 0);
    // an event overlapping the 3-day-old segment pins it
    let ev = fx.db.insert_event(cam, dts(old) + 90_000, "motion").unwrap();
    fx.db.close_event(ev, dts(old) + 180_000, None, "{}").unwrap();
    zmng::retention::run_once(&fx.db, &fx.thumb_dir).unwrap();
    assert!(fx.db.segment(s_old).unwrap().is_some(), "pinned by event");
    assert!(fx.db.segment(s_older).unwrap().is_none(), "past retention");
    assert!(fx.db.segment(s_new).unwrap().is_some());
    let files: Vec<_> = walk(&fx.storage_path());
    assert_eq!(files.len(), 2);
    // once the event is older than event_retention_days the segment goes too
    fx.db.update_camera(cam, serde_json::json!({"event_retention_days": 2}).as_object().unwrap()).unwrap();
    zmng::retention::run_once(&fx.db, &fx.thumb_dir).unwrap();
    assert!(fx.db.segment(s_old).unwrap().is_none());
    // and the event, whose footage is now gone, is removed with it (not archived)
    assert!(fx.db.event(ev).unwrap().is_none());
}

#[test]
fn archived_events_survive_when_footage_is_gone() {
    let fx = Fixture::new();
    let cam = fx.add_camera("c");
    fx.db.update_camera(cam, serde_json::json!({"retention_days": 1}).as_object().unwrap()).unwrap();
    let enc = parse_encoded(&encode_fmp4(2, "libx264", 160, 90));
    let old = now_secs() - 10 * 86_400;
    fx.write_segment(cam, "main", &enc, dts(old), |_| 0);
    let ev = fx.db.insert_event(cam, dts(old), "motion").unwrap();
    fx.db.close_event(ev, dts(old) + 90_000, None, "{}").unwrap();
    fx.db.set_event_archived(ev, true).unwrap();
    // archived => segment pinned regardless of age
    zmng::retention::run_once(&fx.db, &fx.thumb_dir).unwrap();
    assert_eq!(fx.db.segments_of_camera(cam, 10).unwrap().len(), 1);
}

#[test]
fn byte_budget_removes_oldest_first() {
    let fx = Fixture::new();
    let cam = fx.add_camera("c");
    let enc = parse_encoded(&encode_fmp4(2, "libx264", 160, 90));
    let base = now_secs() - 600;
    let ids: Vec<i64> = (0..4).map(|i| fx.write_segment(cam, "main", &enc, dts(base + i * 10), |_| 0)).collect();
    let used = fx.db.storage_used_bytes(fx.storage_id).unwrap();
    let one = fx.db.segment(ids[0]).unwrap().unwrap().bytes;
    // cap so that exactly the oldest one must go
    fx.db.update_storage(fx.storage_id, serde_json::json!({"max_bytes": used - one / 2}).as_object().unwrap()).unwrap();
    zmng::retention::run_once(&fx.db, &fx.thumb_dir).unwrap();
    assert!(fx.db.segment(ids[0]).unwrap().is_none());
    assert!(fx.db.segment(ids[1]).unwrap().is_some());
}

#[test]
fn reindex_adopts_orphan_files_including_substream() {
    let fx = Fixture::new();
    let cam = fx.add_camera("c");
    let enc = parse_encoded(&encode_fmp4(3, "libx264", 160, 90));
    let a = fx.write_segment(cam, "main", &enc, dts(1_800_000_000), |_| 0);
    let b = fx.write_segment(cam, "sub", &enc, dts(1_800_000_000), |_| 0);
    let (sa, sb) = (fx.db.segment(a).unwrap().unwrap(), fx.db.segment(b).unwrap().unwrap());
    // forget the rows, keep the files; also drop a file for an unknown camera and a truncated one
    fx.db.delete_segment(a).unwrap();
    fx.db.delete_segment(b).unwrap();
    std::fs::create_dir_all(fx.storage_path().join("99/20270115")).unwrap();
    std::fs::write(fx.storage_path().join("99/20270115/000000.mp4"), b"junk").unwrap();
    let trunc = fx.storage_path().join(format!("{cam}/20270115/000001.mp4"));
    let full = std::fs::read(fx.storage_path().join(&sa.path)).unwrap();
    std::fs::write(&trunc, &full[..full.len() - 100]).unwrap();
    let n = zmng::retention::reindex(&fx.db).unwrap();
    assert_eq!(n, 3);
    let main: Vec<_> = fx.db.segments_in_range(cam, 0, i64::MAX).unwrap();
    assert_eq!(main.len(), 2);
    let adopted = main.iter().find(|s| s.path == sa.path).unwrap();
    assert_eq!(adopted.index.len(), sa.index.len());
    assert_eq!(adopted.start_dts, sa.start_dts);
    assert_eq!(adopted.end_dts, sa.end_dts);
    assert_eq!(adopted.init_len, sa.init_len);
    let truncated = main.iter().find(|s| s.path.ends_with("000001.mp4")).unwrap();
    assert_eq!(truncated.index.len(), sa.index.len() - 1, "truncated trailing fragment dropped");
    let subs = fx.db.segments_in_range_stream(cam, "sub", 0, i64::MAX).unwrap();
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0].path, sb.path);
}

#[test]
fn tiering_moves_old_segments_to_the_archive_volume() {
    let fx = Fixture::new();
    let cam = fx.add_camera("c");
    let archive = fx.dir.path().join("archive");
    std::fs::create_dir_all(&archive).unwrap();
    let arch_id = fx.db.add_storage(archive.to_str().unwrap(), None, 0).unwrap();
    fx.db.update_storage(fx.storage_id, serde_json::json!({"archive_to": arch_id, "archive_after_days": 1, "archive_rate_mbps": 1000}).as_object().unwrap()).unwrap();
    let enc = parse_encoded(&encode_fmp4(2, "libx264", 160, 90));
    let old = fx.write_segment(cam, "main", &enc, dts(now_secs() - 3 * 86_400), |_| 0);
    let new = fx.write_segment(cam, "main", &enc, dts(now_secs() - 60), |_| 0);
    let st = zmng::tier::run_once(&fx.db, 10).unwrap();
    assert_eq!(st.moved, 1);
    let moved = fx.db.segment(old).unwrap().unwrap();
    assert_eq!(moved.storage_id, arch_id);
    assert!(archive.join(&moved.path).exists());
    assert!(!fx.storage_path().join(&moved.path).exists());
    assert_eq!(fx.db.segment(new).unwrap().unwrap().storage_id, fx.storage_id);
    // the archived file still serves: the index is byte-identical
    assert_eq!(std::fs::read(archive.join(&moved.path)).unwrap().len() as i64, moved.bytes);
    // missing archive volume: nothing moves, no error
    std::fs::remove_dir_all(&archive).unwrap();
    let st = zmng::tier::run_once(&fx.db, 10).unwrap();
    assert_eq!(st.moved, 0);
}

/// A read-only storage (imported ZoneMinder events) is never deleted from or
/// drained by tiering, whatever its age or budget says.
#[test]
fn read_only_storage_is_never_touched() {
    let fx = Fixture::new();
    let cam = fx.add_camera("c");
    fx.db.update_camera(cam, serde_json::json!({"retention_days": 1}).as_object().unwrap()).unwrap();
    let legacy = fx.dir.path().join("legacy");
    std::fs::create_dir_all(&legacy).unwrap();
    let ro = fx.db.add_storage(legacy.to_str().unwrap(), Some(1), 0).unwrap();
    fx.db.update_storage(ro, serde_json::json!({"read_only": true, "archive_to": fx.storage_id, "archive_after_days": 0.5}).as_object().unwrap()).unwrap();
    assert!(fx.db.storage(ro).unwrap().unwrap().read_only);
    assert!(fx.db.update_storage(ro, serde_json::json!({"read_only": "yes"}).as_object().unwrap()).is_err());
    let enc = parse_encoded(&encode_fmp4(2, "libx264", 160, 90));
    // write the file under the legacy root and index it there, 10 days old
    let old = now_secs() - 10 * 86_400;
    let tmp = fx.write_segment(cam, "main", &enc, dts(old), |_| 0);
    let seg = fx.db.segment(tmp).unwrap().unwrap();
    let dst = legacy.join(&seg.path);
    std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
    std::fs::rename(fx.storage_path().join(&seg.path), &dst).unwrap();
    fx.db.move_segment(tmp, ro).unwrap();
    zmng::retention::run_once(&fx.db, &fx.thumb_dir).unwrap();
    let st = zmng::tier::run_once(&fx.db, 10).unwrap();
    assert_eq!(st.moved, 0);
    assert!(fx.db.segment(tmp).unwrap().is_some(), "row kept");
    assert!(dst.exists(), "file kept");
    // deleting the camera drops the row but leaves the read-only file alone
    let n = zmng::retention::delete_camera_files(&fx.db, &fx.thumb_dir, cam).unwrap();
    assert_eq!(n, 1);
    assert!(dst.exists());
    assert!(fx.db.segment(tmp).unwrap().is_none());
}

fn walk(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(root).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() { out.extend(walk(&p)); } else if p.extension().map(|x| x == "mp4").unwrap_or(false) { out.push(p); }
    }
    out
}

/// A storage root without its marker file is a bare mount point: nothing is
/// recorded, deleted, moved or adopted there, and Status says why.
#[test]
fn unmarked_storage_root_is_treated_as_unmounted() {
    let fx = Fixture::new();
    let cam = fx.add_camera("c");
    let enc = parse_encoded(&encode_fmp4(2, "libx264", 160, 90));
    let old = fx.write_segment(cam, "main", &enc, dts(now_secs() - 10 * 86_400), |_| 0);
    fx.db.update_camera(cam, serde_json::json!({"retention_days": 1}).as_object().unwrap()).unwrap();
    let marker = fx.storage_path().join(zmng::retention::MARKER);
    assert!(marker.is_file(), "add_storage writes the marker");
    std::fs::remove_file(&marker).unwrap();
    let st = fx.db.storage(fx.storage_id).unwrap().unwrap();
    assert!(!zmng::retention::storage_mounted(&st));
    zmng::retention::run_once(&fx.db, &fx.thumb_dir).unwrap();
    assert!(fx.db.segment(old).unwrap().is_some(), "nothing deleted on an unmounted volume");
    assert_eq!(zmng::retention::reindex(&fx.db).unwrap(), 0);
    let r = zmng::health::report(&fx.db, &fx.hub, 0, 0, &[cam], true).unwrap();
    assert!(!r.storages[0].available);
    assert!(r.issues.iter().any(|i| i.level == "error" && i.text.contains("not mounted")), "{:?}", r.issues);
    // the marker back (add-storage again, or the mount): business as usual
    zmng::retention::write_marker(&fx.storage_path()).unwrap();
    zmng::retention::run_once(&fx.db, &fx.thumb_dir).unwrap();
    assert!(fx.db.segment(old).unwrap().is_none());
    assert!(zmng::health::report(&fx.db, &fx.hub, 0, 0, &[cam], true).unwrap().storages[0].available);
    // a read-only tree only has to exist
    let ro = fx.dir.path().join("zm-events");
    std::fs::create_dir_all(&ro).unwrap();
    let ro_id = fx.db.add_storage(ro.to_str().unwrap(), None, 0).unwrap();
    fx.db.update_storage(ro_id, serde_json::json!({"read_only": true}).as_object().unwrap()).unwrap();
    std::fs::remove_file(ro.join(zmng::retention::MARKER)).unwrap();
    assert!(zmng::retention::storage_mounted(&fx.db.storage(ro_id).unwrap().unwrap()));
}

/// The primary's space loop leaves a volume with a reachable archive to
/// tiering, down to a hard floor: under half the reserve it deletes the
/// oldest segments itself rather than let the recorders hit ENOSPC.
#[test]
fn retention_hard_floor_deletes_on_the_primary_when_tiering_cannot_keep_up() {
    let fx = Fixture::new();
    let cam = fx.add_camera("c");
    let archive = fx.dir.path().join("archive");
    std::fs::create_dir_all(&archive).unwrap();
    let free = zmng::retention::fs_free_bytes(&fx.storage_path()).unwrap() as i64;
    // the archive is full (reserve above what is free): tiering cannot move anything
    let arch_id = fx.db.add_storage(archive.to_str().unwrap(), None, free * 2).unwrap();
    let enc = parse_encoded(&encode_fmp4(2, "libx264", 160, 90));
    let oldest = fx.write_segment(cam, "main", &enc, dts(now_secs() - 3 * 86_400), |_| 0);
    let newer = fx.write_segment(cam, "main", &enc, dts(now_secs() - 60), |_| 0);
    // under the reserve but above half of it: tiering's job, nothing deleted here
    fx.db.update_storage(fx.storage_id, serde_json::json!({"archive_to": arch_id, "archive_after_days": 1, "reserve_bytes": free + 1_000_000_000}).as_object().unwrap()).unwrap();
    assert_eq!(zmng::tier::run_once(&fx.db, 10).unwrap().moved, 0);
    zmng::retention::run_once(&fx.db, &fx.thumb_dir).unwrap();
    assert!(fx.db.segment(oldest).unwrap().is_some());
    // under half the reserve: the floor is hit, the oldest goes
    fx.db.update_storage(fx.storage_id, serde_json::json!({"reserve_bytes": free * 4}).as_object().unwrap()).unwrap();
    let r = zmng::health::report(&fx.db, &fx.hub, 0, 0, &[cam], true).unwrap();
    assert!(r.issues.iter().any(|i| i.text.contains("half the reserve")), "{:?}", r.issues);
    assert!(r.storages[0].archive_backlog_bytes > 0, "the 3-day-old segment is past the archive cutoff");
    zmng::retention::run_once(&fx.db, &fx.thumb_dir).unwrap();
    assert!(fx.db.segment(oldest).unwrap().is_none(), "floor: oldest deleted on the primary");
    // (the test filesystem's free space does not move with a 20 KB unlink, so
    // the loop goes on to the newer segment too; on a real volume it stops as
    // soon as the reserve is met again)
    assert!(fx.db.segment(newer).unwrap().is_none());
}

/// Under space pressure the archive copy runs unthrottled; a 1 Mbit/s
/// limit would take eight seconds for this megabyte.
#[test]
fn space_pressure_moves_segments_unthrottled() {
    let fx = Fixture::new();
    let cam = fx.add_camera("c");
    let archive = fx.dir.path().join("archive");
    std::fs::create_dir_all(&archive).unwrap();
    let arch_id = fx.db.add_storage(archive.to_str().unwrap(), None, 0).unwrap();
    let mut enc = parse_encoded(&encode_fmp4(2, "libx264", 160, 90));
    for s in enc.samples.iter_mut() {
        let mut d = s.2.to_vec();
        d.resize(d.len() + 60_000, 0); // ~1.2 MB of padding in 20 frames
        s.2 = bytes::Bytes::from(d);
    }
    let seg = fx.write_segment(cam, "main", &enc, dts(now_secs() - 60), |_| 0);
    assert!(fx.db.segment(seg).unwrap().unwrap().bytes > 1_000_000);
    // no age rule, a 1 byte cap => pressure, and a crawl of a rate limit
    fx.db.update_storage(fx.storage_id, serde_json::json!({"archive_to": arch_id, "max_bytes": 1, "archive_rate_mbps": 1}).as_object().unwrap()).unwrap();
    let t = std::time::Instant::now();
    let st = zmng::tier::run_once(&fx.db, 10).unwrap();
    assert_eq!(st.moved, 1);
    assert!(t.elapsed().as_secs_f64() < 3.0, "took {:?}: throttled under pressure", t.elapsed());
    assert_eq!(fx.db.segment(seg).unwrap().unwrap().storage_id, arch_id);
    // the counters followed the move
    assert_eq!(fx.db.storage_used_bytes(fx.storage_id).unwrap(), 0);
    assert_eq!(fx.db.storage_used_bytes(arch_id).unwrap(), fx.db.segment(seg).unwrap().unwrap().bytes);
}

/// The space budget skips footage pinned by an archived event while
/// unpinned segments remain.
#[test]
fn space_budget_deletes_unpinned_footage_first() {
    let fx = Fixture::new();
    let cam = fx.add_camera("c");
    let enc = parse_encoded(&encode_fmp4(2, "libx264", 160, 90));
    let pinned = fx.write_segment(cam, "main", &enc, dts(now_secs() - 3000), |_| 0);
    let plain = fx.write_segment(cam, "main", &enc, dts(now_secs() - 2000), |_| 0);
    let ev = fx.db.insert_event(cam, dts(now_secs() - 3000) + 90_000, "motion").unwrap();
    fx.db.close_event(ev, dts(now_secs() - 3000) + 180_000, None, "{}").unwrap();
    fx.db.set_event_archived(ev, true).unwrap();
    let one = fx.db.segment(plain).unwrap().unwrap().bytes;
    // budget fits exactly one segment: the newer, unpinned one is the survivor's neighbour, not the victim
    fx.db.update_storage(fx.storage_id, serde_json::json!({"max_bytes": one + 10}).as_object().unwrap()).unwrap();
    zmng::retention::run_once(&fx.db, &fx.thumb_dir).unwrap();
    assert!(fx.db.segment(pinned).unwrap().is_some(), "archived event footage kept");
    assert!(fx.db.segment(plain).unwrap().is_none(), "the unpinned segment paid for the budget");
}
