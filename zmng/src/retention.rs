//! Retention (delete whole segments: one unlink + one row) and re-adoption of
//! orphan segment files after a crash.

use anyhow::Result;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

use crate::db::{now_dts, Db};
use crate::mp4::{self, TIMESCALE};

pub fn fs_free_bytes(path: &Path) -> Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes())?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(st.f_bavail as u64 * st.f_frsize as u64)
}

fn remove_segment_file(db: &Db, storage_path: &str, rel: &str, id: i64) -> Result<()> {
    let abs = Path::new(storage_path).join(rel);
    match std::fs::remove_file(&abs) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            // keep the row so the file is retried later rather than orphaned
            warn!(path = %abs.display(), "unlink failed: {e}");
            return Err(e.into());
        }
    }
    db.delete_segment(id)?;
    // prune empty day directory
    if let Some(dir) = abs.parent() {
        let _ = std::fs::remove_dir(dir); // fails harmlessly if not empty
    }
    Ok(())
}

/// One pass of retention. Cheap: a handful of indexed queries and at most a
/// few hundred unlinks.
pub fn run_once(db: &Db, thumb_dir: &Path) -> Result<()> {
    let storages = db.storages()?;
    let cameras = db.cameras()?;
    let now = now_dts();
    let mut deleted = 0usize;

    // 1. per-camera age limit, keeping segments that overlap a recent or
    //    archived event for `event_retention_days`
    for cam in &cameras {
        let cutoff = now - (cam.retention_days * 86_400.0 * TIMESCALE as f64) as i64;
        let ev_days = if cam.event_retention_days > 0.0 { cam.event_retention_days } else { cam.retention_days };
        let event_cutoff = now - (ev_days * 86_400.0 * TIMESCALE as f64) as i64;
        let pruned = crate::preview::Store::new(thumb_dir).prune(cam.id, cutoff.min(event_cutoff));
        if pruned > 0 {
            debug!(camera = cam.id, pruned, "removed old preview hours");
        }
        let mut skip_before = 0i64;
        loop {
            let old: Vec<_> = db.segments_older_than(cam.id, cutoff, 200)?.into_iter().filter(|s| s.start_dts > skip_before).collect();
            if old.is_empty() {
                break;
            }
            let mut progressed = false;
            for s in old {
                skip_before = s.start_dts;
                if db.segment_pinned(&s, event_cutoff)? {
                    continue; // footage of an event worth keeping
                }
                if let Some(st) = storages.iter().find(|x| x.id == s.storage_id) {
                    if remove_segment_file(db, &st.path, &s.path, s.id).is_ok() {
                        deleted += 1;
                        progressed = true;
                    }
                }
            }
            if !progressed {
                break;
            }
        }
    }

    // 2. per-storage space budget (oldest first, across cameras). A primary
    //    volume with a reachable archive is drained by tier.rs instead.
    for st in &storages {
        let p = Path::new(&st.path);
        if let Some(a) = st.archive_to {
            let archive_ok = storages.iter().find(|x| x.id == a).map(|x| Path::new(&x.path).is_dir()).unwrap_or(false);
            if archive_ok {
                continue;
            }
            warn!(storage = st.id, "archive volume unavailable: falling back to deleting oldest segments on the primary");
        }
        let mut used = db.storage_used_bytes(st.id)?;
        let over_budget = |used: i64| -> bool {
            let free = fs_free_bytes(p).unwrap_or(u64::MAX) as i64;
            st.max_bytes.map(|m| used > m).unwrap_or(false) || free < st.reserve_bytes
        };
        'budget: for _round in 0..50 {
            if !over_budget(used) {
                break;
            }
            let victims = db.oldest_segments(st.id, 20)?;
            if victims.is_empty() {
                warn!(storage = st.id, "space budget exceeded but no segments to delete");
                break;
            }
            for s in victims {
                if remove_segment_file(db, &st.path, &s.path, s.id).is_ok() {
                    used -= s.bytes;
                    deleted += 1;
                }
                // re-check after every unlink: never delete a whole batch past the budget
                if !over_budget(used) {
                    break 'budget;
                }
            }
        }
    }

    // 3. events whose footage is entirely gone (keep archived ones)
    for cam in &cameras {
        let earliest = db.earliest_segment_dts(Some(cam.id))?.unwrap_or(now);
        let orphans = db.orphan_events(cam.id, earliest, 500)?;
        for e in orphans {
            if let Some(t) = &e.thumb_path {
                let _ = std::fs::remove_file(thumb_dir.join(t));
            }
            db.delete_event(e.id)?;
        }
    }

    if deleted > 0 {
        info!(deleted, "retention pass removed segments");
    }
    Ok(())
}

/// Remove every segment file and thumbnail of a camera (before deleting it).
pub fn delete_camera_files(db: &Db, thumb_dir: &Path, camera_id: i64) -> Result<usize> {
    let storages = db.storages()?;
    let mut n = 0;
    loop {
        let segs = db.segments_of_camera(camera_id, 500)?;
        if segs.is_empty() {
            break;
        }
        let mut progressed = false;
        for s in segs {
            if let Some(st) = storages.iter().find(|x| x.id == s.storage_id) {
                if remove_segment_file(db, &st.path, &s.path, s.id).is_ok() {
                    n += 1;
                    progressed = true;
                }
            }
        }
        if !progressed {
            anyhow::bail!("could not remove some segment files");
        }
    }
    let _ = std::fs::remove_dir_all(thumb_dir.join(camera_id.to_string()));
    Ok(n)
}

/// Online backup of the index next to the database file (see `health::backup`).
pub fn backup_db(db: &Db, db_path: &Path) -> Result<()> {
    let bak = db_path.with_extension("db.backup");
    crate::health::backup(db, &bak)?;
    info!(path = %bak.display(), "index backup written");
    Ok(())
}

/// Adopt files on disk that are not in the index (crash recovery / manual
/// copy).  Only call when no recorder is writing to these directories.
pub fn reindex(db: &Db) -> Result<usize> {
    let mut adopted = 0;
    for st in db.storages()? {
        let known = db.segment_paths_for_storage(st.id)?;
        let root = PathBuf::from(&st.path);
        let Ok(cams) = std::fs::read_dir(&root) else { continue };
        for cam_dir in cams.flatten() {
            let Ok(cam_id) = cam_dir.file_name().to_string_lossy().parse::<i64>() else { continue };
            if db.camera(cam_id)?.is_none() {
                continue;
            }
            let mut roots = vec![(cam_dir.path(), cam_id.to_string())];
            let sub = cam_dir.path().join("sub");
            if sub.is_dir() {
                roots.push((sub, format!("{cam_id}/sub")));
            }
            for (root_dir, prefix) in roots {
            let Ok(days) = std::fs::read_dir(&root_dir) else { continue };
            for day in days.flatten() {
                if day.file_name() == "sub" {
                    continue;
                }
                let Ok(files) = std::fs::read_dir(day.path()) else { continue };
                for f in files.flatten() {
                    let name = f.file_name().to_string_lossy().to_string();
                    if !name.ends_with(".mp4") {
                        continue;
                    }
                    let rel = format!("{}/{}/{}", prefix, day.file_name().to_string_lossy(), name);
                    if known.contains(&rel) {
                        continue;
                    }
                    match adopt_file(db, &st, cam_id, &rel, &f.path()) {
                        Ok(true) => adopted += 1,
                        Ok(false) => {}
                        Err(e) => warn!(path = %rel, "could not adopt: {e:#}"),
                    }
                }
            }
            }
        }
    }
    Ok(adopted)
}

fn adopt_file(db: &Db, st: &crate::db::Storage, cam_id: i64, rel: &str, abs: &Path) -> Result<bool> {
    let scanned = mp4::scan_segment_file(abs)?;
    if scanned.frags.is_empty() {
        debug!(path = %rel, "empty segment file removed");
        let _ = std::fs::remove_file(abs);
        return Ok(false);
    }
    let init = crate::thumbs::read_range(abs, 0, scanned.init_len)?;
    let (fourcc, entry, w, h) = mp4::extract_sample_entry(&init)
        .ok_or_else(|| anyhow::anyhow!("no sample entry in moov"))?;
    let codec = mp4::Codec::parse(&fourcc).ok_or_else(|| anyhow::anyhow!("unknown codec {fourcc}"))?;
    let rfc = mp4::rfc6381_from_sample_entry(codec, &entry).unwrap_or_else(|| codec.to_string());
    let se = db.intern_sample_entry(codec, &rfc, w, h, &entry)?;
    let valid_len = scanned.frags.last().map(|f| f.offset + f.len as u64).unwrap_or(0);
    let stream = if rel.contains("/sub/") { "sub" } else { "main" };
    db.insert_segment_ext(cam_id, st.id, se, rel, valid_len as i64, scanned.init_len, &scanned.frags, 0, None, stream)?;
    info!(path = %rel, frags = scanned.frags.len(), "adopted segment");
    Ok(true)
}
