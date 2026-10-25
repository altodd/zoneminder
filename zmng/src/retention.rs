//! Retention (delete whole segments: one unlink + one row) and re-adoption of
//! orphan segment files after a crash.

use anyhow::Result;
use std::path::{Path, PathBuf};
use tracing::{debug, error, info, warn};

use crate::db::{now_dts, Db, Storage};
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

fn remove_segment_file(db: &Db, storage_path: &str, rel: &str, id: i64, zm_event: Option<i64>) -> Result<()> {
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
    if let Some(dir) = abs.parent() {
        // an imported ZoneMinder event owns its directory
        // (<monitor>/<date>/<event id>/): the snapshot and analysis files go
        // with the video, then the day directory if it is empty
        if let Some(eid) = zm_event {
            if dir.file_name().map(|n| n.to_string_lossy() == eid.to_string()).unwrap_or(false) {
                if let Err(e) = std::fs::remove_dir_all(dir) {
                    warn!(path = %dir.display(), "removing ZoneMinder event directory: {e}");
                }
                if let Some(day) = dir.parent() {
                    let _ = std::fs::remove_dir(day);
                }
                return Ok(());
            }
        }
        // prune empty day directory
        let _ = std::fs::remove_dir(dir); // fails harmlessly if not empty
    }
    Ok(())
}

/// What retention needs to know about a volume. The real one asks the
/// kernel; tests substitute their own free space and devices.
pub trait Volumes {
    /// Bytes available to unprivileged writers on the filesystem holding `path`.
    fn free(&self, path: &Path) -> Option<u64>;
    /// Identity of the filesystem holding `path` (`st_dev`).
    fn device(&self, path: &Path) -> Option<u64>;
}

pub struct RealVolumes;

impl Volumes for RealVolumes {
    fn free(&self, path: &Path) -> Option<u64> {
        fs_free_bytes(path).ok()
    }
    fn device(&self, path: &Path) -> Option<u64> {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).ok().map(|m| m.dev())
    }
}

/// Room an archive volume keeps above its reserve while a primary waits to
/// drain into it. Tiering stops at the archive's reserve and the space
/// budget only acts below it, so without this margin each waits for the
/// other and the primary ends up deleting footage that was never archived.
pub const ARCHIVE_HEADROOM_BYTES: i64 = 10_000_000_000;

/// Writable, mounted storages on one filesystem. Free space is one number
/// for all of them (an imported ZoneMinder tree next to zmng's own
/// directory), so the space budget deletes the oldest footage across the
/// group, whichever storage row it belongs to.
pub struct FsGroup<'a> {
    pub members: Vec<&'a Storage>,
    /// the largest reserve among the members
    pub reserve: i64,
}

pub fn fs_groups<'a>(storages: &'a [Storage], vols: &dyn Volumes) -> Vec<FsGroup<'a>> {
    let mut groups: Vec<(Option<u64>, FsGroup<'a>)> = Vec::new();
    for st in storages.iter().filter(|s| !s.read_only && storage_mounted(s)) {
        let dev = vols.device(Path::new(&st.path));
        // an unknown device is a group of its own
        match groups.iter_mut().find(|(d, _)| dev.is_some() && *d == dev) {
            Some((_, g)) => {
                g.reserve = g.reserve.max(st.reserve_bytes);
                g.members.push(st);
            }
            None => groups.push((dev, FsGroup { members: vec![st], reserve: st.reserve_bytes })),
        }
    }
    groups.into_iter().map(|(_, g)| g).collect()
}

/// The reserve to keep on the filesystem holding `st`: the largest of the
/// writable storages sharing it.
pub fn fs_reserve(storages: &[Storage], st: &Storage, vols: &dyn Volumes) -> i64 {
    fs_groups(storages, vols)
        .into_iter()
        .find(|g| g.members.iter().any(|m| m.id == st.id))
        .map(|g| g.reserve)
        .unwrap_or(st.reserve_bytes)
}

/// Writable, mounted archive target of `st`, if it has one.
fn archive_of<'a>(storages: &'a [Storage], st: &Storage) -> Option<&'a Storage> {
    let a = st.archive_to?;
    storages.iter().find(|x| x.id == a && !x.read_only && storage_mounted(x))
}

/// Rows age retention looks at per camera per pass.
const AGE_ROWS_PER_PASS: usize = 5000;
const KEY_MIN: (i64, i64) = (i64::MIN, i64::MIN);

/// Where age retention left off, per camera. A camera whose oldest footage
/// is a long run of pinned (motion) segments must not hide the unpinned
/// segments behind it, and looking at every pinned row on every pass would
/// make the pass unbounded.
static AGE_CURSOR: std::sync::Mutex<Vec<(i64, (i64, i64))>> = std::sync::Mutex::new(Vec::new());

fn age_cursor(cam: i64) -> (i64, i64) {
    AGE_CURSOR.lock().unwrap().iter().find(|(c, _)| *c == cam).map(|(_, k)| *k).unwrap_or(KEY_MIN)
}

fn set_age_cursor(cam: i64, key: (i64, i64)) {
    let mut v = AGE_CURSOR.lock().unwrap();
    v.retain(|(c, _)| *c != cam);
    v.push((cam, key));
}

/// Delete the oldest segments across `from`, oldest first whichever storage
/// holds them, until `must` bytes are freed. Footage of an event the user
/// archived is skipped when `keep_archived` (always, today: only removing
/// the archive mark, or deleting the event or camera, frees it). Motion events do not
/// pin anything here: when space runs out the oldest footage goes first.
/// Returns (bytes freed, segments deleted). Bounded per call so the 30 s
/// loop stays responsive whatever the pinned run at the front.
fn delete_oldest(db: &Db, from: &[&Storage], must: i64, keep_archived: bool) -> Result<(i64, usize)> {
    let (mut freed, mut n, mut looked) = (0i64, 0usize, 0usize);
    let mut after = KEY_MIN;
    'pages: while freed < must && looked < 4000 && n < 1000 {
        // the oldest 20 of the union are among the oldest 20 of each storage
        let mut page = Vec::new();
        for st in from {
            page.extend(db.oldest_segments_after(st.id, after, 20)?);
        }
        page.sort_by_key(|s| (s.start_dts, s.id));
        page.truncate(20);
        if page.is_empty() {
            break;
        }
        for s in page {
            if freed >= must {
                break 'pages;
            }
            looked += 1;
            after = (s.start_dts, s.id);
            if keep_archived && db.segment_in_archived_event(&s)? {
                continue;
            }
            let Some(st) = from.iter().find(|x| x.id == s.storage_id) else { continue };
            if remove_segment_file(db, &st.path, &s.path, s.id, s.zm_event_id).is_ok() {
                freed += s.bytes;
                n += 1;
            }
        }
    }
    Ok((freed, n))
}

/// One pass of retention. Cheap: a handful of indexed queries and at most a
/// few thousand unlinks.
pub fn run_once(db: &Db, thumb_dir: &Path) -> Result<()> {
    run_once_with(db, thumb_dir, &RealVolumes)
}

pub fn run_once_with(db: &Db, thumb_dir: &Path, vols: &dyn Volumes) -> Result<()> {
    let storages = db.storages()?;
    let cameras = db.cameras()?;
    let now = now_dts();
    let mut deleted = 0usize;
    // per camera: events younger than this pin their footage
    let event_cutoffs: std::collections::HashMap<i64, i64> = cameras
        .iter()
        .map(|cam| {
            let ev_days = if cam.event_retention_days > 0.0 { cam.event_retention_days } else { cam.retention_days };
            (cam.id, now - (ev_days * 86_400.0 * TIMESCALE as f64) as i64)
        })
        .collect();

    // 1. per-camera age limit, keeping segments that overlap a recent or
    //    archived event for `event_retention_days`. Paged from where the last
    //    pass stopped, wrapping round to the oldest, so pinned runs of any
    //    length are stepped over.
    for cam in &cameras {
        let cutoff = now - (cam.retention_days * 86_400.0 * TIMESCALE as f64) as i64;
        let event_cutoff = event_cutoffs[&cam.id];
        let pruned = crate::preview::Store::new(thumb_dir).prune(cam.id, cutoff.min(event_cutoff));
        if pruned > 0 {
            debug!(camera = cam.id, pruned, "removed old preview hours");
        }
        let start = age_cursor(cam.id);
        let mut after = start;
        let mut wrapped = start == KEY_MIN;
        let mut looked = 0usize;
        'age: while looked < AGE_ROWS_PER_PASS {
            let page = db.segments_older_than_after(cam.id, cutoff, after, 200)?;
            if page.is_empty() {
                after = KEY_MIN;
                if wrapped {
                    break; // a full sweep: the next pass starts at the oldest
                }
                wrapped = true;
                continue;
            }
            for s in page {
                if wrapped && start != KEY_MIN && (s.start_dts, s.id) > start {
                    after = KEY_MIN; // back where this pass began
                    break 'age;
                }
                looked += 1;
                after = (s.start_dts, s.id);
                if db.segment_pinned(&s, event_cutoff)? {
                    continue; // footage of an event worth keeping
                }
                if let Some(st) = storages.iter().find(|x| x.id == s.storage_id && !x.read_only && storage_mounted(x)) {
                    if remove_segment_file(db, &st.path, &s.path, s.id, s.zm_event_id).is_ok() {
                        deleted += 1;
                    }
                }
            }
        }
        set_age_cursor(cam.id, after);
    }

    // 2a. per-storage caps (`max_bytes`), oldest first. A primary with a
    //     reachable archive is drained by tier.rs instead.
    for st in storages.iter().filter(|s| !s.read_only && storage_mounted(s)) {
        let Some(cap) = st.max_bytes else { continue };
        if archive_of(&storages, st).is_some() {
            continue;
        }
        let used = db.storage_used_bytes(st.id)?;
        if used > cap {
            let (_, n) = delete_oldest(db, &[st], used - cap, true)?;
            deleted += n;
        }
    }

    // 2b. per-filesystem reserve: the oldest footage on the volume goes first,
    //     across every writable storage on it. A primary with a reachable
    //     archive is never deleted from here: it is only drained by tier.rs,
    //     and the archive volume it drains into keeps ARCHIVE_HEADROOM_BYTES
    //     above its reserve, deleting its own oldest footage, while the primary
    //     has a backlog or is under pressure.
    let groups = fs_groups(&storages, vols);
    // per storage: bytes above its volume's reserve after this pass's deletes
    let mut room: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    for g in &groups {
        let Some(free) = vols.free(Path::new(&g.members[0].path)) else { continue };
        let free = free as i64;
        for m in &g.members {
            room.insert(m.id, free - g.reserve);
        }
        let waiting: Vec<&Storage> = storages
            .iter()
            .filter(|p| !p.read_only && storage_mounted(p) && !g.members.iter().any(|m| m.id == p.id))
            .filter(|p| archive_of(&storages, p).map(|a| g.members.iter().any(|m| m.id == a.id)).unwrap_or(false))
            .filter(|p| {
                let p_free = vols.free(Path::new(&p.path)).map(|f| f as i64).unwrap_or(i64::MAX);
                let pressure = p.max_bytes.map(|m| db.storage_used_bytes(p.id).unwrap_or(0) > m).unwrap_or(false) || p_free < p.reserve_bytes;
                pressure || crate::tier::backlog_bytes(db, p).unwrap_or(0) > 0
            })
            .collect();
        let headroom = if waiting.is_empty() { 0 } else { ARCHIVE_HEADROOM_BYTES };
        let must = (g.reserve + headroom - free).max(0);
        if must == 0 {
            continue;
        }
        let from: Vec<&Storage> = g.members.iter().copied().filter(|m| archive_of(&storages, m).is_none()).collect();
        if from.is_empty() {
            continue; // a primary waiting for tiering
        }
        let (freed, n) = delete_oldest(db, &from, must, true)?;
        deleted += n;
        for m in &g.members {
            room.insert(m.id, free + freed - g.reserve);
        }
        if n > 0 && headroom > 0 {
            debug!(storages = ?from.iter().map(|m| m.id).collect::<Vec<_>>(), mb = freed / 1_000_000, "made room on the archive volume for tiering");
        }
        if freed < must && free < g.reserve {
            warn!(storages = ?from.iter().map(|m| m.id).collect::<Vec<_>>(), "free space under the reserve but nothing more may be deleted here");
        }
    }

    // 2c. last resort for a primary: under half its reserve with its archive
    //     unavailable or empty, it deletes its own oldest footage, because
    //     ENOSPC on the recording volume stops every recorder.
    for p in storages.iter().filter(|p| p.archive_to.is_some() && !p.read_only && storage_mounted(p)) {
        let Some(free) = vols.free(Path::new(&p.path)) else { continue };
        let free = free as i64;
        if free >= p.reserve_bytes / 2 {
            continue;
        }
        // the archive volume has room, or footage it can delete to make
        // some: tiering's job, not ours
        let archive_can_take = archive_of(&storages, p)
            .and_then(|a| groups.iter().find(|g| g.members.iter().any(|m| m.id == a.id)))
            .map(|g| {
                g.members.iter().any(|m| room.get(&m.id).copied().unwrap_or(0) > 0)
                    || g.members.iter().filter(|m| archive_of(&storages, m).is_none()).any(|m| db.oldest_segments(m.id, 1).map(|v| !v.is_empty()).unwrap_or(false))
            })
            .unwrap_or(false);
        if archive_can_take {
            continue;
        }
        warn!(storage = p.id, free_gb = free / 1_000_000_000, "free space under half the reserve and the archive cannot take more: deleting the oldest footage on the primary");
        let (freed, n) = delete_oldest(db, &[p], p.reserve_bytes - free, true)?;
        deleted += n;
        if freed < p.reserve_bytes - free {
            error!(storage = p.id, "the primary is filling with footage of archived events, which is never deleted automatically: unarchive events or add space");
        }
    }

    // 3. events whose footage is entirely gone (keep archived ones): anything
    //    older than the oldest segment, plus gaps age retention opened between
    //    pinned segments (only possible past the camera's retention cutoff)
    for cam in &cameras {
        let earliest = db.earliest_segment_dts(Some(cam.id))?.unwrap_or(now);
        let age_cutoff = now - (cam.retention_days * 86_400.0 * TIMESCALE as f64) as i64;
        let orphans = db.orphan_events(cam.id, earliest.max(age_cutoff), 500)?;
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
                // a read-only storage keeps its files; only the index row goes
                let ok = if st.read_only { db.delete_segment(s.id).is_ok() } else { remove_segment_file(db, &st.path, &s.path, s.id, s.zm_event_id).is_ok() };
                if ok {
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

/// Adopt files on disk that are not in the index (crash recovery / manual
/// copy).  Only call when no recorder is writing to these directories.
/// Name of the file at a storage root that proves the volume is mounted.
pub const MARKER: &str = ".zmng-storage";

/// Write the mount marker at a storage root.
pub fn write_marker(root: &Path) -> Result<()> {
    if !root.is_dir() {
        anyhow::bail!("{} is not a directory", root.display());
    }
    std::fs::write(root.join(MARKER), b"zmng storage root; do not delete\n")?;
    Ok(())
}

/// A writable storage is usable only when its marker is present: a bare
/// mount point (volume not mounted at boot, USB disk unplugged) has none,
/// and recording or deleting there would hit the root filesystem instead.
/// Read-only trees (imported ZoneMinder events) only need to exist.
pub fn storage_mounted(st: &crate::db::Storage) -> bool {
    let p = Path::new(&st.path);
    if st.read_only { p.is_dir() } else { p.join(MARKER).is_file() }
}

pub fn reindex(db: &Db) -> Result<usize> {
    let mut adopted = 0;
    for st in db.storages()? {
        if !storage_mounted(&st) {
            warn!(storage = st.id, path = %st.path, "not mounted (no {MARKER}); skipping reindex there");
            continue;
        }
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
    let audio = mp4::extract_audio_entry(&init).map(|(e, rate, ch)| mp4::AudioParams { sample_entry: bytes::Bytes::from(e), sample_rate: rate, channels: ch, rfc6381: "mp4a.40.2".into() });
    let se = db.intern_sample_entry(codec, &rfc, w, h, &entry, audio.as_ref())?;
    let valid_len = scanned.frags.last().map(|f| f.offset + f.len as u64).unwrap_or(0);
    let stream = if rel.contains("/sub/") { "sub" } else { "main" };
    db.insert_segment_ext(cam_id, st.id, se, rel, valid_len as i64, scanned.init_len, &scanned.frags, 0, None, stream)?;
    info!(path = %rel, frags = scanned.frags.len(), "adopted segment");
    Ok(true)
}
