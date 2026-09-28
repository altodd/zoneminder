//! Import existing ZoneMinder events without touching the video files.
//!
//! ZoneMinder (VideoWriter=passthrough) writes each event as an fMP4
//! (`frag_keyframe+empty_moov`) whose `tfdt` values start near zero.  We scan
//! the file's `moof` boxes (no payload reads), shift the index to absolute
//! time using the event's StartDateTime, and store the shift as
//! `segment.dts_offset`; `video.rs` patches `tfdt` on the fly when serving.
//! The event row itself becomes an `event` (kind `zm`) with a downscaled
//! `snapshot.jpg` as thumbnail.
//!
//! Input: a TSV produced on the ZoneMinder host with
//!
//! ```sql
//! SELECT Id,MonitorId,UNIX_TIMESTAMP(StartDateTime),UNIX_TIMESTAMP(EndDateTime),
//!        DATE(StartDateTime),Length,Frames,AlarmFrames,MaxScore,Archived,IFNULL(Notes,''),
//!        DefaultVideo,StorageId FROM Events WHERE EndDateTime IS NOT NULL ORDER BY Id
//! ```
//! (`mysql -B -N -e "..." > events.tsv`).  Files are expected at
//! `<storage.path>/<MonitorId>/<DATE>/<Id>/<DefaultVideo>` (ZoneMinder's
//! "Medium" scheme), i.e. point a zmng storage row at the ZoneMinder events
//! root so the paths need no copying.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;
use tracing::{info, warn};

use crate::db::Db;
use crate::mp4::{self, TIMESCALE};

pub struct ImportOpts<'a> {
    pub tsv: &'a Path,
    pub storage_id: i64,
    /// ZoneMinder MonitorId -> zmng camera id (identity when absent)
    pub camera_map: HashMap<i64, i64>,
    pub thumb_dir: &'a Path,
    pub thumb_width: u32,
    pub dry_run: bool,
    pub skip_thumbs: bool,
}

#[derive(Default, Debug)]
pub struct ImportStats {
    pub rows: usize,
    pub imported: usize,
    pub skipped_existing: usize,
    pub missing_file: usize,
    pub failed: usize,
    pub bytes: u64,
}

pub fn import_zm(db: &Db, opts: &ImportOpts) -> Result<ImportStats> {
    let storage = db.storage(opts.storage_id)?.ok_or_else(|| anyhow::anyhow!("storage {} not found", opts.storage_id))?;
    let root = Path::new(&storage.path);
    let text = std::fs::read_to_string(opts.tsv).with_context(|| format!("reading {}", opts.tsv.display()))?;
    let mut st = ImportStats::default();
    let cameras: HashMap<i64, _> = db.cameras()?.into_iter().map(|c| (c.id, c)).collect();

    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        st.rows += 1;
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 13 {
            warn!(line, "bad TSV row");
            st.failed += 1;
            continue;
        }
        let parse = |i: usize| cols[i].parse::<i64>().unwrap_or(0);
        let (eid, mid, start_s, end_s) = (parse(0), parse(1), parse(2), parse(3));
        let date = cols[4];
        let (alarm_frames, max_score, archived) = (parse(7), parse(8), parse(9) != 0);
        let notes = cols[10];
        let default_video = cols[11];
        let cam_id = *opts.camera_map.get(&mid).unwrap_or(&mid);
        if !cameras.contains_key(&cam_id) {
            warn!(eid, mid, cam_id, "no zmng camera for this monitor; skipping (use --camera-map)");
            st.failed += 1;
            continue;
        }
        if db.zm_event_imported(eid)? {
            st.skipped_existing += 1;
            continue;
        }
        if default_video.is_empty() || default_video.starts_with("incomplete") {
            st.missing_file += 1;
            continue;
        }
        let rel = format!("{mid}/{date}/{eid}/{default_video}");
        let abs = root.join(&rel);
        if !abs.exists() {
            st.missing_file += 1;
            continue;
        }
        match import_one(db, opts, &abs, &rel, cam_id, eid, start_s, end_s, alarm_frames, max_score, archived, notes) {
            Ok(bytes) => {
                st.imported += 1;
                st.bytes += bytes;
                if st.imported % 500 == 0 {
                    info!(imported = st.imported, rows = st.rows, gb = st.bytes / 1_000_000_000, "import progress");
                }
            }
            Err(e) => {
                warn!(eid, path = %rel, "import failed: {e:#}");
                st.failed += 1;
            }
        }
    }
    Ok(st)
}

#[allow(clippy::too_many_arguments)]
fn import_one(
    db: &Db,
    opts: &ImportOpts,
    abs: &Path,
    rel: &str,
    cam_id: i64,
    eid: i64,
    start_s: i64,
    end_s: i64,
    alarm_frames: i64,
    max_score: i64,
    archived: bool,
    notes: &str,
) -> Result<u64> {
    let scanned = mp4::scan_segment_file(abs)?;
    if scanned.frags.is_empty() {
        anyhow::bail!("no fragments");
    }
    let init = crate::thumbs::read_range(abs, 0, scanned.init_len)?;
    let (fourcc, mut entry, w, h) = mp4::extract_sample_entry(&init).ok_or_else(|| anyhow::anyhow!("no sample entry"))?;
    let codec = mp4::Codec::parse(&fourcc).ok_or_else(|| anyhow::anyhow!("unsupported codec {fourcc}"))?;
    mp4::prefer_hvc1(&mut entry);
    let rfc = mp4::rfc6381_from_sample_entry(codec, &entry).unwrap_or_else(|| codec.to_string());
    let se = db.intern_sample_entry(codec, &rfc, w, h, &entry)?;

    // absolute start = event StartDateTime; the file's first tfdt is the origin
    let start_dts = start_s * TIMESCALE as i64;
    let first = scanned.frags[0].dts;
    let offset = start_dts - first;
    let frags: Vec<mp4::FragEntry> = scanned.frags.iter().map(|f| mp4::FragEntry { dts: f.dts + offset, ..*f }).collect();
    let bytes = frags.last().map(|f| f.offset + f.len as u64).unwrap_or(0);
    if opts.dry_run {
        return Ok(bytes);
    }
    db.insert_segment_ext(cam_id, opts.storage_id, se, rel, bytes as i64, scanned.init_len, &frags, offset, Some(eid), "main")?;

    // the ZoneMinder event becomes an event row (motion if it had alarm frames, else a marker)
    let end_dts = if end_s > start_s { end_s * TIMESCALE as i64 } else { frags.last().map(|f| f.dts + f.duration as i64).unwrap_or(start_dts) };
    let score = ((max_score.max(0) as f64).ln_1p() * 40.0).clamp(1.0, 255.0) as u8;
    let kind = if alarm_frames > 0 { "motion" } else { "zm" };
    let mut thumb_rel: Option<String> = None;
    if !opts.skip_thumbs {
        let snap = abs.parent().unwrap().join("snapshot.jpg");
        if snap.exists() {
            let rel_t = format!("{cam_id}/zm{eid}.jpg");
            let out = opts.thumb_dir.join(&rel_t);
            if let Some(p) = out.parent() {
                std::fs::create_dir_all(p)?;
            }
            match downscale_jpeg(&snap, &out, opts.thumb_width) {
                Ok(()) => thumb_rel = Some(rel_t),
                Err(e) => warn!(eid, "thumbnail: {e:#}"),
            }
        }
    }
    let meta = serde_json::json!({"zm_event_id": eid, "alarm_frames": alarm_frames, "max_score": max_score});
    db.insert_event_full(
        cam_id,
        start_dts,
        end_dts,
        kind,
        score,
        None,
        thumb_rel.as_deref(),
        &meta.to_string(),
        archived,
        if notes.is_empty() { None } else { Some(notes) },
    )?;
    Ok(bytes)
}

fn downscale_jpeg(src: &Path, dst: &Path, width: u32) -> Result<()> {
    let img = image::open(src)?;
    let w = img.width().min(width);
    let h = (img.height() as u64 * w as u64 / img.width().max(1) as u64) as u32;
    let small = img.resize_exact(w, h.max(1), image::imageops::FilterType::Triangle);
    let mut out = std::fs::File::create(dst)?;
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 78);
    enc.encode_image(&small)?;
    Ok(())
}

pub fn parse_camera_map(s: &str) -> HashMap<i64, i64> {
    s.split(',')
        .filter_map(|p| {
            let (a, b) = p.split_once('=')?;
            Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
        })
        .collect()
}
