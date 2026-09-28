//! Substream motion detection.
//!
//! ffmpeg decodes the camera's low-resolution substream (640x360 H.264 costs
//! ~1-2 % of a core at 5 fps) into grayscale frames on a pipe.  A small
//! ZoneMinder-style detector (reference blend, threshold, block blobs) turns
//! them into a 0..255 score per frame.  Scores feed (a) the recorder's
//! per-fragment motion index for the timeline and (b) an event state machine
//! that writes ONE row per event and one JPEG thumbnail per event.
//!
//! Object detection (YOLO via ONNX Runtime) plugs in at the same point later:
//! it only needs to run on frames the motion detector flags.

use anyhow::{Context, Result};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tracing::{debug, error, info, warn};

use crate::db::{now_dts, Camera, Db};
use crate::mp4::TIMESCALE;
use crate::recorder::LiveHub;

pub struct DetectCtx {
    pub db: Db,
    pub hub: Arc<LiveHub>,
    pub ffmpeg: String,
    pub thumb_dir: PathBuf,
}

pub struct DetectorHandle {
    pub camera: RwLock<Camera>,
    pub stop: tokio::sync::watch::Sender<bool>,
    pub status: RwLock<DetectStatus>,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct DetectStatus {
    pub running: bool,
    pub fps: f32,
    pub score: u8,
    pub in_event: Option<i64>,
    pub last_error: Option<String>,
}

static DETECTORS: std::sync::OnceLock<RwLock<HashMap<i64, Arc<DetectorHandle>>>> = std::sync::OnceLock::new();

fn detectors() -> &'static RwLock<HashMap<i64, Arc<DetectorHandle>>> {
    DETECTORS.get_or_init(|| RwLock::new(HashMap::new()))
}

pub fn status(camera_id: i64) -> Option<DetectStatus> {
    detectors().read().get(&camera_id).map(|d| d.status.read().clone())
}

/// Relevant subset of camera config; a change restarts the detector.
fn detect_key(c: &Camera) -> String {
    format!(
        "{:?}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
        c.sub_url, c.detect_fps, c.detect_width, c.detect_height, c.pixel_threshold, c.min_area_pct,
        c.min_blob_pct, c.pre_secs, c.post_secs, c.cooldown_secs, c.zones_json, c.masks_json
    )
}

pub async fn reconcile(ctx: Arc<DetectCtx>) -> Result<()> {
    let cams = ctx.db.cameras()?;
    let mut map = detectors().write();
    let keep: Vec<i64> = cams.iter().filter(|c| c.enabled).map(|c| c.id).collect();
    for id in map.keys().filter(|id| !keep.contains(id)).copied().collect::<Vec<_>>() {
        if let Some(d) = map.remove(&id) {
            let _ = d.stop.send(true);
        }
    }
    for cam in cams.into_iter().filter(|c| c.enabled) {
        if let Some(d) = map.get(&cam.id) {
            if detect_key(&d.camera.read()) == detect_key(&cam) {
                continue;
            }
            let _ = d.stop.send(true);
            map.remove(&cam.id);
            info!(camera = cam.id, "restarting detector (config changed)");
        }
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let h = Arc::new(DetectorHandle {
            camera: RwLock::new(cam.clone()),
            stop: stop_tx,
            status: RwLock::new(DetectStatus::default()),
        });
        map.insert(cam.id, h.clone());
        let ctx2 = ctx.clone();
        tokio::spawn(async move {
            loop {
                let (c, hh, s) = (ctx2.clone(), h.clone(), stop_rx.clone());
                let res = tokio::spawn(async move { run_detector(c, hh, s).await }).await;
                if *stop_rx.borrow() {
                    return;
                }
                if let Err(e) = res {
                    error!("detector task panicked: {e}; restarting in 5 s");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                } else {
                    return;
                }
            }
        });
    }
    Ok(())
}

async fn run_detector(ctx: Arc<DetectCtx>, h: Arc<DetectorHandle>, mut stop: tokio::sync::watch::Receiver<bool>) {
    let mut backoff = Duration::from_secs(2);
    loop {
        if *stop.borrow() {
            return;
        }
        let cam = h.camera.read().clone();
        let started = Instant::now();
        let res = tokio::select! {
            r = detect_session(&ctx, &h, &cam) => r,
            _ = stop.changed() => return,
        };
        h.status.write().running = false;
        if let Err(e) = res {
            warn!(camera = cam.id, "detector session failed: {e:#}");
            h.status.write().last_error = Some(format!("{e:#}"));
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(2);
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {},
            _ = stop.changed() => return,
        }
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

// ---------------------------------------------------------------------------
// Detector core (pure, testable)
// ---------------------------------------------------------------------------

pub struct MotionDetector {
    w: usize,
    h: usize,
    reference: Vec<u8>,
    mask: Vec<u8>, // 1 = analyse, 0 = ignore
    mask_pixels: usize,
    threshold: u8,
    min_area_pct: f64,
    min_blob_pct: f64,
    frames_seen: u32,
    // block grid (8x8 px) reused between frames
    bw: usize,
    bh: usize,
    blocks: Vec<u8>,
}

impl MotionDetector {
    pub fn new(w: usize, h: usize, threshold: u8, min_area_pct: f64, min_blob_pct: f64, zones: &[Vec<(f64, f64)>], masks: &[Vec<(f64, f64)>]) -> Self {
        let mut mask = if zones.is_empty() { vec![1u8; w * h] } else { vec![0u8; w * h] };
        for z in zones {
            rasterize(&mut mask, w, h, z, 1);
        }
        for m in masks {
            rasterize(&mut mask, w, h, m, 0);
        }
        let mask_pixels = mask.iter().filter(|m| **m == 1).count().max(1);
        let bw = w.div_ceil(8);
        let bh = h.div_ceil(8);
        MotionDetector {
            w, h, reference: vec![0; w * h], mask, mask_pixels, threshold, min_area_pct, min_blob_pct,
            frames_seen: 0, bw, bh, blocks: vec![0; bw * bh],
        }
    }

    /// Feed one grayscale frame; returns a 0..255 motion score (0 = none).
    pub fn feed(&mut self, frame: &[u8]) -> u8 {
        debug_assert_eq!(frame.len(), self.w * self.h);
        if self.frames_seen == 0 {
            self.reference.copy_from_slice(frame);
            self.frames_seen = 1;
            return 0;
        }
        // 1. count changed pixels per 8x8 block (within mask)
        self.blocks.iter_mut().for_each(|b| *b = 0);
        let mut changed = 0usize;
        let thr = self.threshold as i16;
        for y in 0..self.h {
            let row = y * self.w;
            let brow = (y / 8) * self.bw;
            for x in 0..self.w {
                let i = row + x;
                if self.mask[i] == 0 {
                    continue;
                }
                let d = (frame[i] as i16 - self.reference[i] as i16).abs();
                if d > thr {
                    changed += 1;
                    self.blocks[brow + x / 8] += 1;
                }
            }
        }
        // 2. blend reference (slow adaptation so slow lighting changes are absorbed,
        //    fast enough that a parked car becomes background within ~30 s at 5 fps)
        for (r, f) in self.reference.iter_mut().zip(frame) {
            *r = ((*r as u16 * 15 + *f as u16) / 16) as u8;
        }
        self.frames_seen = self.frames_seen.saturating_add(1);
        if self.frames_seen < 10 {
            return 0; // warm-up
        }
        let area_pct = changed as f64 * 100.0 / self.mask_pixels as f64;
        if area_pct < self.min_area_pct {
            return 0;
        }
        // 3. largest connected blob of "active" blocks (>= 1/3 of block pixels changed)
        let active: Vec<bool> = self.blocks.iter().map(|b| *b >= 21).collect();
        let largest = largest_component(&active, self.bw, self.bh);
        let blob_pct = largest as f64 * 64.0 * 100.0 / self.mask_pixels as f64;
        if blob_pct < self.min_blob_pct {
            return 0;
        }
        // score: percentage of zone changed, compressed: 1% -> ~40, 5% -> ~120, 20%+ -> 255
        let s = (area_pct.ln_1p() * 80.0).clamp(1.0, 255.0);
        s as u8
    }
}

fn largest_component(active: &[bool], bw: usize, bh: usize) -> usize {
    let mut seen = vec![false; active.len()];
    let mut best = 0;
    let mut stack = Vec::new();
    for start in 0..active.len() {
        if !active[start] || seen[start] {
            continue;
        }
        let mut size = 0;
        stack.push(start);
        seen[start] = true;
        while let Some(i) = stack.pop() {
            size += 1;
            let (x, y) = (i % bw, i / bw);
            let mut push = |nx: usize, ny: usize| {
                let j = ny * bw + nx;
                if active[j] && !seen[j] {
                    seen[j] = true;
                    stack.push(j);
                }
            };
            if x > 0 { push(x - 1, y); }
            if x + 1 < bw { push(x + 1, y); }
            if y > 0 { push(x, y - 1); }
            if y + 1 < bh { push(x, y + 1); }
        }
        best = best.max(size);
    }
    best
}

/// Scanline polygon fill into a w*h mask with `value` (coords normalized 0..1).
fn rasterize(mask: &mut [u8], w: usize, h: usize, poly: &[(f64, f64)], value: u8) {
    if poly.len() < 3 {
        return;
    }
    let pts: Vec<(f64, f64)> = poly.iter().map(|(x, y)| (x * w as f64, y * h as f64)).collect();
    for y in 0..h {
        let fy = y as f64 + 0.5;
        let mut xs: Vec<f64> = Vec::new();
        for i in 0..pts.len() {
            let (x0, y0) = pts[i];
            let (x1, y1) = pts[(i + 1) % pts.len()];
            if (y0 <= fy && y1 > fy) || (y1 <= fy && y0 > fy) {
                xs.push(x0 + (fy - y0) * (x1 - x0) / (y1 - y0));
            }
        }
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        for pair in xs.chunks(2) {
            if pair.len() == 2 {
                let a = pair[0].max(0.0).round() as usize;
                let b = pair[1].min(w as f64).round() as usize;
                for x in a..b.min(w) {
                    mask[y * w + x] = value;
                }
            }
        }
    }
}

pub fn parse_polys(json: &str) -> Vec<Vec<(f64, f64)>> {
    serde_json::from_str::<Vec<Vec<(f64, f64)>>>(json).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Session: ffmpeg pipe + event state machine
// ---------------------------------------------------------------------------

struct TempFile(std::path::PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn detect_session(ctx: &DetectCtx, h: &DetectorHandle, cam: &Camera) -> Result<()> {
    let url = cam.sub_url.clone().unwrap_or_else(|| cam.main_url.clone());
    let (w, hh) = (cam.detect_width as usize, cam.detect_height as usize);
    let fps = cam.detect_fps.max(0.5);
    let mut cmd = tokio::process::Command::new(&ctx.ffmpeg);
    cmd.args(["-nostdin", "-loglevel", "error", "-threads", "1"]);
    let _list_file; // keeps the temp file alive for the session
    if let Some(src) = url.strip_prefix("lavfi:") {
        // test hook: e.g. sub_url = "lavfi:testsrc=size=640x360:rate=5"
        cmd.args(["-re", "-f", "lavfi", "-i", src]);
    } else {
        // Keep the RTSP password off the command line (visible in `ps`): hand
        // ffmpeg a 0600 concat list that names the URL.
        let dir = std::env::temp_dir().join("zmng");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("cam{}-{}.txt", cam.id, std::process::id()));
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(&path)?;
            // per-entry demuxer options: TCP interleaved RTSP, low latency
            writeln!(f, "file '{}'\noption rtsp_transport tcp\noption fflags nobuffer", url.replace('\'', "'\\''"))?;
        }
        _list_file = TempFile(path.clone());
        cmd.args(["-f", "concat", "-safe", "0", "-protocol_whitelist", "file,rtsp,rtp,tcp,udp",
                  "-i", path.to_str().unwrap_or_default()]);
    }
    let mut child = cmd
        .args([
            "-an", "-sn", "-dn",
            "-vf", &format!("fps={fps},scale={w}:{hh}:flags=fast_bilinear,format=gray"),
            "-f", "rawvideo", "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawning ffmpeg for substream")?;
    let mut stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    tokio::spawn(async move {
        let mut r = tokio::io::BufReader::new(stderr);
        let mut line = String::new();
        use tokio::io::AsyncBufReadExt;
        while let Ok(n) = r.read_line(&mut line).await {
            if n == 0 { break; }
            debug!(ffmpeg = %line.trim());
            line.clear();
        }
    });
    info!(camera = cam.id, name = %cam.name, w, h = hh, fps, "detector started");
    h.status.write().running = true;

    let zones = parse_polys(&cam.zones_json);
    let masks = parse_polys(&cam.masks_json);
    let mut det = MotionDetector::new(w, hh, cam.pixel_threshold, cam.min_area_pct, cam.min_blob_pct, &zones, &masks);
    let mut frame = vec![0u8; w * hh];
    let mut ev = EventState::default();
    let mut fps_t = Instant::now();
    let mut fps_n = 0u32;
    // pipeline latency of the substream decode is small (< 300 ms); we stamp frames with wallclock at read time
    loop {
        tokio::time::timeout(Duration::from_secs(30), stdout.read_exact(&mut frame))
            .await
            .map_err(|_| anyhow::anyhow!("no frames for 30 s"))?
            .context("ffmpeg pipe closed")?;
        let dts = now_dts() - TIMESCALE as i64 / 4; // ~250 ms decode/pipe latency
        let score = det.feed(&frame);
        fps_n += 1;
        if fps_t.elapsed() >= Duration::from_secs(5) {
            let mut st = h.status.write();
            st.fps = fps_n as f32 / fps_t.elapsed().as_secs_f32();
            fps_t = Instant::now();
            fps_n = 0;
        }
        h.status.write().score = score;
        // look the recorder handle up each time: it is replaced on recorder restart
        if let Some(hh2) = ctx.hub.get(cam.id) {
            hh2.motion.push(dts, score);
        }
        ev.step(ctx, cam, dts, score, h).await?;
    }
}

#[derive(Default)]
struct EventState {
    event_id: Option<i64>,
    start_dts: i64,
    last_motion_dts: i64,
    peak: u8,
    peak_dts: i64,
    consecutive: u8,
    last_db_update: i64,
    motion_frames: u32,
    frames: u32,
}

impl EventState {
    async fn step(&mut self, ctx: &DetectCtx, cam: &Camera, dts: i64, score: u8, h: &DetectorHandle) -> Result<()> {
        let ts = TIMESCALE as i64;
        if score > 0 {
            self.consecutive = self.consecutive.saturating_add(1);
        } else {
            self.consecutive = 0;
        }
        match self.event_id {
            None => {
                if self.consecutive >= 2 {
                    let start = dts - (cam.pre_secs * ts as f64) as i64;
                    let id = ctx.db.insert_event(cam.id, start, "motion")?;
                    info!(camera = cam.id, event = id, score, "event start");
                    self.event_id = Some(id);
                    self.start_dts = dts;
                    self.peak = score;
                    self.peak_dts = dts;
                    self.last_motion_dts = dts;
                    self.last_db_update = dts;
                    self.motion_frames = 1;
                    self.frames = 1;
                    h.status.write().in_event = Some(id);
                    ctx.db.update_event_progress(id, score, dts)?;
                }
            }
            Some(id) => {
                self.frames += 1;
                if score > 0 {
                    self.last_motion_dts = dts;
                    self.motion_frames += 1;
                    if score > self.peak {
                        self.peak = score;
                        self.peak_dts = dts;
                    }
                    if dts - self.last_db_update > ts * 2 {
                        ctx.db.update_event_progress(id, self.peak, self.peak_dts)?;
                        self.last_db_update = dts;
                    }
                }
                let quiet = dts - self.last_motion_dts;
                let max_len = ts * 600; // hard cap 10 min from event start; a new event starts if motion continues
                if quiet > (cam.cooldown_secs * ts as f64) as i64 || dts - self.start_dts > max_len {
                    let end = self.last_motion_dts + (cam.post_secs * ts as f64) as i64;
                    let meta = serde_json::json!({
                        "frames": self.frames,
                        "motion_frames": self.motion_frames,
                        "peak": self.peak,
                    });
                    ctx.db.close_event(id, end, None, &meta.to_string())?;
                    info!(camera = cam.id, event = id, peak = self.peak, secs = (end - (self.peak_dts)) / ts, "event end");
                    h.status.write().in_event = None;
                    let peak_dts = self.peak_dts;
                    *self = EventState::default();
                    // thumbnail in the background (waits for the segment to close if needed)
                    let db = ctx.db.clone();
                    let ffmpeg = ctx.ffmpeg.clone();
                    let thumb_dir = ctx.thumb_dir.clone();
                    let cam_id = cam.id;
                    tokio::spawn(async move {
                        if let Err(e) = make_thumbnail(&db, &ffmpeg, &thumb_dir, cam_id, id, peak_dts).await {
                            error!(camera = cam_id, event = id, "thumbnail: {e:#}");
                        }
                    });
                }
            }
        }
        Ok(())
    }
}

/// Find the fragment containing `dts` (retrying until the segment holding it
/// has been closed and indexed), decode its keyframe and store a JPEG.
pub async fn make_thumbnail(db: &Db, ffmpeg: &str, thumb_dir: &std::path::Path, cam_id: i64, event_id: i64, dts: i64) -> Result<()> {
    let mut attempt = 0;
    loop {
        let segs = db.segments_in_range(cam_id, dts, dts + 1)?;
        if let Some(seg) = segs.first() {
            let frag = seg
                .index
                .iter()
                .find(|f| f.dts <= dts && dts < f.dts + f.duration as i64)
                .or_else(|| seg.index.first())
                .copied()
                .ok_or_else(|| anyhow::anyhow!("segment has no fragments"))?;
            let storage = db.storage(seg.storage_id)?.ok_or_else(|| anyhow::anyhow!("storage missing"))?;
            let path = PathBuf::from(&storage.path).join(&seg.path);
            let init = crate::thumbs::read_range(&path, 0, seg.init_len)?;
            let data = crate::mp4::rebase_fragment(&crate::thumbs::read_range(&path, frag.offset, frag.len)?, seg.dts_offset);
            let jpeg = crate::thumbs::frag_to_jpeg(ffmpeg, &init, &data, 640, 5).await?;
            let rel = format!("{cam_id}/{event_id}.jpg");
            let abs = thumb_dir.join(&rel);
            std::fs::create_dir_all(abs.parent().unwrap())?;
            std::fs::write(&abs, &jpeg)?;
            db.set_event_thumb(event_id, &rel)?;
            debug!(camera = cam_id, event = event_id, bytes = jpeg.len(), "thumbnail written");
            return Ok(());
        }
        attempt += 1;
        if attempt > 6 {
            anyhow::bail!("no segment covers dts {dts} after waiting");
        }
        tokio::time::sleep(Duration::from_secs(20)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_a_moving_block_and_ignores_noise() {
        let (w, h) = (160usize, 90usize);
        let mut det = MotionDetector::new(w, h, 25, 0.5, 0.2, &[], &[]);
        let base = vec![100u8; w * h];
        for _ in 0..12 {
            assert_eq!(det.feed(&base), 0);
        }
        // tiny 2x2 change: below min area
        let mut f = base.clone();
        for y in 10..12 { for x in 10..12 { f[y * w + x] = 250; } }
        assert_eq!(det.feed(&f), 0);
        // a 30x30 object: clearly motion
        let mut f = base.clone();
        for y in 20..50 { for x in 40..70 { f[y * w + x] = 250; } }
        let s = det.feed(&f);
        assert!(s > 0, "score {s}");
        // scattered single-pixel noise covering 2% of the frame but no blob
        let mut f = base.clone();
        let mut i = 0;
        while i < w * h { f[i] = 250; i += 47; }
        // reset reference first
        let mut det2 = MotionDetector::new(w, h, 25, 0.5, 0.2, &[], &[]);
        for _ in 0..12 { det2.feed(&base); }
        assert_eq!(det2.feed(&f), 0);
    }

    #[test]
    fn zone_mask_limits_detection() {
        let (w, h) = (160usize, 90usize);
        // zone = left half only
        let zone = vec![(0.0, 0.0), (0.5, 0.0), (0.5, 1.0), (0.0, 1.0)];
        let mut det = MotionDetector::new(w, h, 25, 0.5, 0.2, &[zone], &[]);
        let base = vec![100u8; w * h];
        for _ in 0..12 { det.feed(&base); }
        let mut f = base.clone();
        for y in 20..50 { for x in 100..130 { f[y * w + x] = 250; } } // right half
        assert_eq!(det.feed(&f), 0);
        let mut f = base.clone();
        for y in 20..50 { for x in 10..40 { f[y * w + x] = 250; } } // left half
        assert!(det.feed(&f) > 0);
    }
}
