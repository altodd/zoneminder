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
    pub bus: Arc<crate::notify::Bus>,
    /// seconds between preview tiles (0 = off)
    pub preview_secs: u32,
    /// external object detector (None = motion only)
    pub objects: Option<Arc<crate::objects::ObjectDetector>>,
}

pub struct DetectorHandle {
    pub camera: RwLock<Camera>,
    pub stop: tokio::sync::watch::Sender<bool>,
    pub status: RwLock<DetectStatus>,
    /// per-frame analysis for the live tuning view; built only while
    /// someone subscribes
    pub analysis: tokio::sync::broadcast::Sender<Arc<Analysis>>,
}

impl DetectorHandle {
    pub fn new(camera: Camera) -> Self {
        DetectorHandle {
            camera: RwLock::new(camera),
            stop: tokio::sync::watch::channel(false).0,
            status: RwLock::new(DetectStatus::default()),
            analysis: tokio::sync::broadcast::channel(8).0,
        }
    }
}

/// What the detector saw in one frame: the cells that changed and, per
/// zone, how much changed against what the zone needs.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Analysis {
    pub t_ms: i64,
    pub score: u8,
    /// analysed frame size and its 8x8 cell grid
    pub w: usize,
    pub h: usize,
    pub bw: usize,
    pub bh: usize,
    /// cells where at least 1/8 of the pixels changed, 4 cells per hex digit
    pub changed: String,
    pub zones: Vec<ZoneAnalysis>,
    pub threshold: u8,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ZoneAnalysis {
    /// empty: the whole frame (a camera without zones)
    pub name: String,
    pub score: u8,
    pub changed_pct: f64,
    pub blob_pct: f64,
    pub min_area_pct: f64,
    pub min_blob_pct: f64,
    pub fired: bool,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct DetectStatus {
    pub running: bool,
    pub fps: f32,
    pub score: u8,
    pub in_event: Option<i64>,
    pub last_error: Option<String>,
    /// wall-clock ms when this detector session started decoding
    pub started_ms: i64,
}

pub fn status(hub: &LiveHub, camera_id: i64) -> Option<DetectStatus> {
    hub.detectors.read().get(&camera_id).map(|d| d.status.read().clone())
}

/// Relevant subset of camera config; a change restarts the detector
/// (object-detection settings included: the gate is built per session).
pub fn detect_key(c: &Camera) -> String {
    format!(
        "{:?}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
        c.sub_url, c.detect_fps, c.detect_width, c.detect_height, c.pixel_threshold, c.min_area_pct,
        c.min_blob_pct, c.pre_secs, c.post_secs, c.cooldown_secs, c.zones_json, c.masks_json,
        c.record_sub, c.objects, c.object_labels, c.require_object
    )
}

pub async fn reconcile(ctx: Arc<DetectCtx>) -> Result<()> {
    let cams = ctx.db.cameras()?;
    let mut map = ctx.hub.detectors.write();
    let keep: Vec<i64> = cams.iter().filter(|c| c.enabled).map(|c| c.id).collect();
    for id in map.keys().filter(|id| !keep.contains(id)).copied().collect::<Vec<_>>() {
        if let Some(d) = map.remove(&id) {
            let _ = d.stop.send(true);
        }
    }
    for cam in cams.into_iter().filter(|c| c.enabled) {
        if let Some(d) = map.get(&cam.id) {
            if detect_key(&d.camera.read()) == detect_key(&cam) {
                *d.camera.write() = cam.clone(); // name etc. may have changed
                continue;
            }
            let _ = d.stop.send(true);
            map.remove(&cam.id);
            info!(camera = cam.id, "restarting detector (config changed)");
        }
        let h = Arc::new(DetectorHandle::new(cam.clone()));
        let stop_rx = h.stop.subscribe();
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

/// Which 8x8 cells of the analysed frame changed: lets object detection
/// ignore things that did not move (a school bus parked in the background is
/// a real "bus" in every frame, but never the reason for an event).
#[derive(Clone, Debug, PartialEq)]
pub struct MotionCells {
    /// frame size in pixels the grid was taken from
    pub w: usize,
    pub h: usize,
    /// cells per row / column (8x8 px each, the last ones may be partial)
    pub bw: usize,
    pub bh: usize,
    pub bits: Vec<bool>,
}

impl MotionCells {
    pub fn any(&self) -> bool {
        self.bits.iter().any(|b| *b)
    }

    /// Whether a normalized box (x0, y0, x1, y1 in 0..1) touches a changed
    /// cell, allowing `margin` cells of slack around the box (the object may
    /// have moved a little between the analysed frame and the detected one).
    pub fn overlaps(&self, bbox: [f64; 4], margin: usize) -> bool {
        let cx = |x: f64| ((x * self.w as f64) / 8.0).floor().max(0.0) as usize;
        let cy = |y: f64| ((y * self.h as f64) / 8.0).floor().max(0.0) as usize;
        let (x0, y0) = (cx(bbox[0]).saturating_sub(margin), cy(bbox[1]).saturating_sub(margin));
        let (x1, y1) = ((cx(bbox[2]) + margin).min(self.bw.saturating_sub(1)), (cy(bbox[3]) + margin).min(self.bh.saturating_sub(1)));
        (y0..=y1).any(|y| (x0..=x1).any(|x| self.bits.get(y * self.bw + x).copied().unwrap_or(false)))
    }

    /// Compact JSON for `event.meta_json`: `{"w","h","bw","bh","hex"}`.
    pub fn to_json(&self) -> serde_json::Value {
        let mut hex = String::with_capacity(self.bits.len() / 4 + 1);
        for chunk in self.bits.chunks(4) {
            let n = chunk.iter().enumerate().fold(0u8, |a, (i, b)| a | ((*b as u8) << (3 - i)));
            hex.push(char::from_digit(n as u32, 16).unwrap_or('0'));
        }
        serde_json::json!({"w": self.w, "h": self.h, "bw": self.bw, "bh": self.bh, "hex": hex})
    }

    pub fn from_json(v: &serde_json::Value) -> Option<MotionCells> {
        let g = |k: &str| v.get(k).and_then(|x| x.as_u64()).map(|x| x as usize);
        let (w, h, bw, bh) = (g("w")?, g("h")?, g("bw")?, g("bh")?);
        let mut bits = Vec::with_capacity(bw * bh);
        for c in v.get("hex")?.as_str()?.chars() {
            let n = c.to_digit(16)?;
            for i in 0..4 {
                bits.push(n & (1 << (3 - i)) != 0);
            }
        }
        bits.truncate(bw * bh);
        (bits.len() == bw * bh).then_some(MotionCells { w, h, bw, bh, bits })
    }
}

/// What the detector needs of one zone.
#[derive(Clone, Debug)]
pub struct ZoneSpec {
    /// empty for the implicit whole-frame zone of a camera without zones
    pub name: String,
    pub min_area_pct: f64,
    pub min_blob_pct: f64,
}

/// ZoneMinder-style motion detector over one or more zones. Every zone is
/// scored on its own pixels with its own minimum changed area and minimum
/// blob, so a doorway can be sensitive while the road behind it is not; a
/// frame's score is the best zone's. Masks are never analysed.
pub struct MotionDetector {
    w: usize,
    h: usize,
    reference: Vec<u8>,
    /// per pixel: bit z set = pixel belongs to zone z (masks cleared); 0 = ignored
    zmap: Vec<u16>,
    zones: Vec<ZoneSpec>,
    /// pixels per zone (>= 1)
    zone_px: Vec<usize>,
    /// per zone, per 8x8 cell: how many of the cell's pixels are in the zone
    zone_cell_px: Vec<Vec<u8>>,
    threshold: u8,
    frames_seen: u32,
    // block grid (8x8 px) reused between frames
    bw: usize,
    bh: usize,
    /// changed pixels per cell, all zones together (for `cells`)
    blocks: Vec<u8>,
    zone_blocks: Vec<Vec<u8>>,
    zone_changed: Vec<usize>,
    /// largest moving object per zone on the last frame, % of the zone
    zone_blob: Vec<f64>,
    /// zones that fired on the last frame, with their scores
    fired: Vec<(usize, u8)>,
}

impl MotionDetector {
    /// Bare polygons with the camera's thresholds (tests, older callers).
    pub fn new(w: usize, h: usize, threshold: u8, min_area_pct: f64, min_blob_pct: f64, zones: &[Vec<(f64, f64)>], masks: &[Vec<(f64, f64)>]) -> Self {
        let z: Vec<crate::zones::Zone> = zones.iter().enumerate().map(|(i, p)| crate::zones::Zone { name: format!("Zone {}", i + 1), points: p.clone(), min_area_pct: None, min_blob_pct: None }).collect();
        let m: Vec<crate::zones::Zone> = masks.iter().map(|p| crate::zones::Zone { name: String::new(), points: p.clone(), min_area_pct: None, min_blob_pct: None }).collect();
        Self::with_zones(w, h, threshold, min_area_pct, min_blob_pct, &z, &m)
    }

    /// Named zones, each with its own sensitivity (`None` = the camera's
    /// `min_area_pct` / `min_blob_pct`). No zones = the whole frame.
    pub fn with_zones(w: usize, h: usize, threshold: u8, min_area_pct: f64, min_blob_pct: f64, zones: &[crate::zones::Zone], masks: &[crate::zones::Zone]) -> Self {
        let mut specs: Vec<ZoneSpec> = Vec::new();
        let mut zmap = vec![0u16; w * h];
        if zones.is_empty() {
            zmap.iter_mut().for_each(|p| *p = 1);
            specs.push(ZoneSpec { name: String::new(), min_area_pct, min_blob_pct });
        }
        for (i, z) in zones.iter().take(crate::zones::MAX_ZONES).enumerate() {
            crate::zones::rasterize(w, h, &z.points, |p| zmap[p] |= 1 << i);
            specs.push(ZoneSpec { name: z.name.clone(), min_area_pct: z.min_area_pct.unwrap_or(min_area_pct), min_blob_pct: z.min_blob_pct.unwrap_or(min_blob_pct) });
        }
        for m in masks {
            crate::zones::rasterize(w, h, &m.points, |p| zmap[p] = 0);
        }
        let bw = w.div_ceil(8);
        let bh = h.div_ceil(8);
        let n = specs.len();
        let mut zone_px = vec![0usize; n];
        let mut zone_cell_px = vec![vec![0u8; bw * bh]; n];
        for y in 0..h {
            for x in 0..w {
                let mut bits = zmap[y * w + x];
                while bits != 0 {
                    let z = bits.trailing_zeros() as usize;
                    zone_px[z] += 1;
                    zone_cell_px[z][(y / 8) * bw + x / 8] += 1;
                    bits &= bits - 1;
                }
            }
        }
        zone_px.iter_mut().for_each(|p| *p = (*p).max(1));
        MotionDetector {
            w, h, reference: vec![0; w * h], zmap, zones: specs, zone_px, zone_cell_px, threshold,
            frames_seen: 0, bw, bh, blocks: vec![0; bw * bh], zone_blocks: vec![vec![0; bw * bh]; n], zone_changed: vec![0; n], zone_blob: vec![0.0; n], fired: Vec::new(),
        }
    }

    /// Feed one grayscale frame; returns a 0..255 motion score (0 = none):
    /// the highest score of the zones that fired.
    pub fn feed(&mut self, frame: &[u8]) -> u8 {
        debug_assert_eq!(frame.len(), self.w * self.h);
        self.fired.clear();
        if self.frames_seen == 0 {
            self.reference.copy_from_slice(frame);
            self.frames_seen = 1;
            return 0;
        }
        // 1. count changed pixels per 8x8 block, overall and per zone
        self.blocks.iter_mut().for_each(|b| *b = 0);
        self.zone_blocks.iter_mut().for_each(|zb| zb.iter_mut().for_each(|b| *b = 0));
        self.zone_changed.iter_mut().for_each(|c| *c = 0);
        let thr = self.threshold as i16;
        for y in 0..self.h {
            let row = y * self.w;
            let brow = (y / 8) * self.bw;
            for x in 0..self.w {
                let i = row + x;
                let mut bits = self.zmap[i];
                if bits == 0 {
                    continue;
                }
                let d = (frame[i] as i16 - self.reference[i] as i16).abs();
                if d > thr {
                    let b = brow + x / 8;
                    self.blocks[b] = self.blocks[b].saturating_add(1);
                    while bits != 0 {
                        let z = bits.trailing_zeros() as usize;
                        self.zone_changed[z] += 1;
                        self.zone_blocks[z][b] += 1;
                        bits &= bits - 1;
                    }
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
        // 3. per zone: enough of it changed, and as one object (largest blob of
        //    cells where >= 1/3 of the zone's pixels in the cell changed)
        let mut best = 0u8;
        for z in 0..self.zones.len() {
            self.zone_blob[z] = 0.0;
            if self.zone_changed[z] == 0 {
                continue;
            }
            let area_pct = self.zone_changed[z] as f64 * 100.0 / self.zone_px[z] as f64;
            let (zb, cp) = (&self.zone_blocks[z], &self.zone_cell_px[z]);
            let active: Vec<bool> = zb.iter().zip(cp).map(|(c, p)| *p > 0 && *c as u32 * 64 >= *p as u32 * 21).collect();
            let blob_px = largest_component(&active, cp, self.bw, self.bh);
            let blob_pct = blob_px as f64 * 100.0 / self.zone_px[z] as f64;
            self.zone_blob[z] = blob_pct;
            if area_pct < self.zones[z].min_area_pct || blob_pct < self.zones[z].min_blob_pct {
                continue;
            }
            // score: percentage of the zone changed, compressed: 1% -> ~40, 5% -> ~120, 20%+ -> 255
            let s = (area_pct.ln_1p() * 80.0).clamp(1.0, 255.0) as u8;
            self.fired.push((z, s));
            best = best.max(s);
        }
        best
    }

    /// Cells of the last fed frame where at least 1/8 of the pixels changed.
    pub fn cells(&self) -> MotionCells {
        MotionCells { w: self.w, h: self.h, bw: self.bw, bh: self.bh, bits: self.blocks.iter().map(|b| *b >= 8).collect() }
    }

    /// Names of the zones that fired on the last frame, best first (empty
    /// for a camera without zones).
    pub fn fired_zones(&self) -> Vec<String> {
        let mut f = self.fired.clone();
        f.sort_by(|a, b| b.1.cmp(&a.1));
        f.into_iter().map(|(z, _)| self.zones[z].name.clone()).filter(|n| !n.is_empty()).collect()
    }

    /// The last frame as the live tuning view shows it.
    pub fn analysis(&self, t_ms: i64, score: u8) -> Analysis {
        let changed = self.cells().to_json()["hex"].as_str().unwrap_or_default().to_string();
        let zones = (0..self.zones.len())
            .map(|z| {
                let s = self.fired.iter().find(|(i, _)| *i == z).map(|(_, s)| *s).unwrap_or(0);
                let r = |v: f64| (v * 100.0).round() / 100.0;
                ZoneAnalysis {
                    name: self.zones[z].name.clone(), score: s, fired: s > 0,
                    changed_pct: r(self.zone_changed[z] as f64 * 100.0 / self.zone_px[z] as f64), blob_pct: r(self.zone_blob[z]),
                    min_area_pct: self.zones[z].min_area_pct, min_blob_pct: self.zones[z].min_blob_pct,
                }
            })
            .collect();
        Analysis { t_ms, score, w: self.w, h: self.h, bw: self.bw, bh: self.bh, changed, zones, threshold: self.threshold }
    }

    /// Per-zone result of the last frame: (name, score, changed %), for the
    /// live tuning view.
    pub fn zone_report(&self) -> Vec<(String, u8, f64)> {
        (0..self.zones.len())
            .map(|z| {
                let s = self.fired.iter().find(|(i, _)| *i == z).map(|(_, s)| *s).unwrap_or(0);
                (self.zones[z].name.clone(), s, self.zone_changed[z] as f64 * 100.0 / self.zone_px[z] as f64)
            })
            .collect()
    }
}

/// Size in pixels of the largest 4-connected group of active cells, each
/// cell counting the pixels it holds (cells at a zone's edge are partial).
fn largest_component(active: &[bool], weight: &[u8], bw: usize, bh: usize) -> usize {
    let mut seen = vec![false; active.len()];
    let mut best = 0;
    let mut stack = Vec::new();
    for start in 0..active.len() {
        if !active[start] || seen[start] {
            continue;
        }
        let mut size = 0usize;
        stack.push(start);
        seen[start] = true;
        while let Some(i) = stack.pop() {
            size += weight[i] as usize;
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

/// Zones' polygons of a camera (either JSON form, see [`crate::zones`]).
pub fn parse_polys(json: &str) -> Vec<Vec<(f64, f64)>> {
    crate::zones::parse_polys(json)
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

async fn detect_session(ctx: &Arc<DetectCtx>, h: &Arc<DetectorHandle>, cam: &Camera) -> Result<()> {
    if cam.record_sub {
        if let Some(sh) = ctx.hub.get_stream(cam.id, crate::recorder::StreamKind::Sub) {
            return detect_from_recorder(ctx, h, cam, sh).await;
        }
    }
    let url = cam.sub_url.clone().unwrap_or_else(|| cam.main_url.clone());
    let (w, hh) = (cam.detect_width as usize, cam.detect_height as usize);
    let fps = cam.detect_fps.max(0.5);
    let mut cmd = crate::thumbs::ffmpeg_command(&ctx.ffmpeg);
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
            "-an", "-sn", "-dn", "-threads", "1", "-filter_threads", "1",
            "-vf", &format!("fps={fps},scale={w}:{hh}:flags=fast_bilinear,format=yuv420p"),
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
    {
        let mut st = h.status.write();
        st.running = true;
        st.started_ms = crate::db::now_dts() / 90;
    }

    let mut run = DetectRun::new(ctx.clone(), cam.clone(), h.clone());
    let mut frame = vec![0u8; w * hh * 3 / 2];
    // pipeline latency of the substream decode is small (< 300 ms); we stamp frames with wallclock at read time
    // (dropping `run` at any exit closes an open event)
    loop {
        tokio::time::timeout(Duration::from_secs(30), stdout.read_exact(&mut frame))
            .await
            .map_err(|_| anyhow::anyhow!("no frames for 30 s"))?
            .context("ffmpeg pipe closed")?;
        let dts = now_dts() - TIMESCALE as i64 / 4; // ~250 ms decode/pipe latency
        run.on_frame(dts, &frame).await?;
    }
}

/// Per-session detector state shared by both feeding modes.
struct DetectRun {
    ctx: Arc<DetectCtx>,
    cam: Camera,
    handle: Arc<DetectorHandle>,
    det: MotionDetector,
    ev: EventState,
    fps_t: Instant,
    fps_n: u32,
    preview: Option<crate::preview::PreviewWriter>,
    gate: Option<crate::objects::ObjectGate>,
    w: usize,
    h: usize,
    last_dts: i64,
}

impl DetectRun {
    fn new(ctx: Arc<DetectCtx>, cam: Camera, handle: Arc<DetectorHandle>) -> Self {
        let zones = crate::zones::parse(&cam.zones_json, "Zone");
        let masks = crate::zones::parse(&cam.masks_json, "Mask");
        DetectRun {
            det: MotionDetector::with_zones(cam.detect_width as usize, cam.detect_height as usize, cam.pixel_threshold, cam.min_area_pct, cam.min_blob_pct, &zones, &masks),
            fps_t: Instant::now(),
            fps_n: 0,
            preview: (ctx.preview_secs > 0).then(|| crate::preview::PreviewWriter::new(&ctx.thumb_dir, cam.id, ctx.preview_secs, cam.detect_width, cam.detect_height)),
            gate: ctx.objects.as_ref().filter(|_| cam.objects).map(|d| crate::objects::ObjectGate::new(d.clone(), &cam)),
            ev: EventState { labels: ctx.objects.as_ref().filter(|_| cam.objects).map(|d| crate::objects::ObjectGate::new(d.clone(), &cam).labels().to_vec()).unwrap_or_default(), ..EventState::default() },
            w: cam.detect_width as usize,
            h: cam.detect_height as usize,
            last_dts: 0,
            ctx,
            cam,
            handle,
        }
    }

    /// `frame` is a planar yuv420p picture; the motion detector uses its Y
    /// plane, the preview writer and the object detector the whole thing.
    async fn on_frame(&mut self, dts: i64, frame: &[u8]) -> Result<()> {
        let (ctx, cam, h) = (self.ctx.clone(), self.cam.clone(), self.handle.clone());
        self.last_dts = dts;
        let score = self.det.feed(&frame[..self.w * self.h]);
        if let Some(p) = self.preview.as_mut() {
            if let Err(e) = p.maybe(dts, frame, self.w, self.h) {
                warn!(camera = cam.id, "preview tile: {e:#}");
            }
        }
        let mut in_flight = false;
        let cells = (score > 0).then(|| self.det.cells());
        self.ev.frame_cells = cells.clone();
        self.ev.frame_zones = if score > 0 { self.det.fired_zones() } else { Vec::new() };
        if let Some(g) = self.gate.as_mut() {
            if let Some(cells) = cells {
                g.maybe_send(dts, frame, self.w, self.h, self.ev.event_id, cells);
            }
            for (d, sent_for, objs) in g.poll() {
                self.ev.on_objects(&ctx, &cam, d, sent_for, objs)?;
            }
            in_flight = g.in_flight();
            // a close deferred until the last detection answered
            if let Some(last) = self.ev.deferred_close {
                if !in_flight {
                    self.ev.deferred_close = None;
                    self.ev.close(&ctx, &cam, last, &h, false)?;
                }
            }
        }
        self.fps_n += 1;
        if self.fps_t.elapsed() >= Duration::from_secs(5) {
            let mut st = h.status.write();
            st.fps = self.fps_n as f32 / self.fps_t.elapsed().as_secs_f32();
            self.fps_t = Instant::now();
            self.fps_n = 0;
        }
        h.status.write().score = score;
        if h.analysis.receiver_count() > 0 {
            let _ = h.analysis.send(Arc::new(self.det.analysis(dts / 90, score)));
        }
        // look the recorder handle up each time: it is replaced on recorder restart
        if let Some(hh2) = ctx.hub.get(cam.id) {
            hh2.motion.push(dts, score);
        }
        self.ev.step(&ctx, &cam, dts, score, &h, in_flight)
    }
}

impl Drop for DetectRun {
    /// A session ending for any reason (stream error, config change, stop)
    /// must not leave its event open until the next process restart.
    fn drop(&mut self) {
        if self.ev.event_id.is_some() {
            let end_dts = if self.last_dts > 0 { self.last_dts } else { now_dts() };
            if let Err(e) = self.ev.close(&self.ctx, &self.cam, end_dts, &self.handle, false) {
                error!(camera = self.cam.id, "closing event at session end: {e:#}");
            }
        }
    }
}

/// Frame times from ffmpeg's `showinfo` lines: the `config in time_base: a/b`
/// line, then one `pts:N` per frame. ffmpeg must run with `-copyts` so `N`
/// is the recording's absolute time. (`pts_time` is printed with six
/// significant digits, which cannot hold epoch seconds.)
#[derive(Default, Debug)]
pub struct ShowinfoClock {
    num: i64,
    den: i64,
}

impl ShowinfoClock {
    /// Feed one stderr line; a frame line yields its time in 90 kHz ticks.
    pub fn line(&mut self, line: &str) -> Option<i64> {
        if let Some(i) = line.find("config in time_base: ") {
            let rest = &line[i + 21..];
            let tb: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '/').collect();
            if let Some((a, b)) = tb.split_once('/') {
                if let (Ok(a), Ok(b)) = (a.parse::<i64>(), b.parse::<i64>()) {
                    if a > 0 && b > 0 {
                        self.num = a;
                        self.den = b;
                    }
                }
            }
            return None;
        }
        if self.den == 0 || !line.contains("showinfo") {
            return None;
        }
        let i = line.find(" pts:")?;
        let digits: String = line[i + 5..].trim_start().chars().take_while(|c| c.is_ascii_digit() || *c == '-').collect();
        let pts: i64 = digits.parse().ok()?;
        Some((pts as i128 * self.num as i128 * TIMESCALE as i128 / self.den as i128) as i64)
    }
}

/// Feed ffmpeg from the substream RECORDER's live fMP4 fragments instead of/// Feed ffmpeg from the substream RECORDER's live fMP4 fragments instead of
/// opening a second RTSP session: no extra camera connection, credentials
/// never leave the process, and every decoded frame carries the recording's
/// own absolute timestamp (read back from ffmpeg's `showinfo` filter).
async fn detect_from_recorder(ctx: &Arc<DetectCtx>, h: &Arc<DetectorHandle>, cam: &Camera, sh: Arc<crate::recorder::CamHandle>) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let (w, hh) = (cam.detect_width as usize, cam.detect_height as usize);
    let fps = cam.detect_fps.max(0.5);
    // wait for the recorder to have an init segment
    let mut waited = 0;
    let (init, first_frag) = loop {
        if let Some(r) = sh.recent.read().as_ref() {
            if let Some((_, _, f)) = r.frags.back() {
                break (r.init.clone(), f.clone());
            }
        }
        waited += 1;
        if waited > 60 {
            anyhow::bail!("substream recorder has no fragments yet");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    let mut child = crate::thumbs::ffmpeg_command(&ctx.ffmpeg)
        .args([
            "-nostdin", "-loglevel", "info", "-nostats", "-threads", "1", "-copyts",
            "-fflags", "nobuffer", "-flags", "low_delay", "-probesize", "65536", "-analyzeduration", "0",
            "-f", "mp4", "-i", "pipe:0", "-an", "-sn", "-dn", "-threads", "1", "-filter_threads", "1",
            "-vf", &format!("fps={fps},scale={w}:{hh}:flags=fast_bilinear,format=yuv420p,showinfo"),
            "-f", "rawvideo", "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawning ffmpeg (recorder-fed)")?;
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    // writer: init + latest fragment now, then every fragment as it closes
    let mut rx = sh.live.subscribe();
    let writer_init = init.clone();
    let cam_id = cam.id;
    let mut writer = tokio::spawn(async move {
        if stdin.write_all(&writer_init).await.is_err() || stdin.write_all(&first_frag).await.is_err() {
            return "pipe closed";
        }
        loop {
            match rx.recv().await {
                Ok(f) => {
                    if f.init != writer_init {
                        return "codec parameters changed";
                    }
                    if stdin.write_all(&f.data).await.is_err() {
                        return "pipe closed";
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    debug!(camera = cam_id, lagged = n, "detector feed lagged");
                    continue;
                }
                Err(_) => return "recorder stopped",
            }
        }
    });
    // stderr: showinfo lines give the time of each output frame, in order
    let (pts_tx, mut pts_rx) = tokio::sync::mpsc::unbounded_channel::<i64>();
    tokio::spawn(async move {
        let mut r = tokio::io::BufReader::new(stderr);
        let mut line = String::new();
        let mut clock = ShowinfoClock::default();
        while let Ok(n) = r.read_line(&mut line).await {
            if n == 0 {
                break;
            }
            if let Some(dts) = clock.line(&line) {
                let _ = pts_tx.send(dts);
            } else if !line.contains("showinfo") {
                debug!(ffmpeg = %line.trim());
            }
            line.clear();
        }
    });
    info!(camera = cam.id, name = %cam.name, w, h = hh, fps, "detector started (fed by substream recorder)");
    {
        let mut st = h.status.write();
        st.running = true;
        st.started_ms = crate::db::now_dts() / 90;
    }

    let mut run = DetectRun::new(ctx.clone(), cam.clone(), h.clone());
    let mut frame = vec![0u8; w * hh * 3 / 2];
    loop {
        tokio::select! {
            r = tokio::time::timeout(Duration::from_secs(30), stdout.read_exact(&mut frame)) => {
                r.map_err(|_| anyhow::anyhow!("no frames for 30 s"))?.context("ffmpeg pipe closed")?;
            }
            why = &mut writer => {
                anyhow::bail!("feed ended: {}", why.unwrap_or("writer task failed"));
            }
        }
        // time of this output frame (absolute, because tfdt is absolute and ffmpeg keeps it)
        let dts = match tokio::time::timeout(Duration::from_millis(300), pts_rx.recv()).await {
            Ok(Some(t)) if t > 1_000_000_000 * TIMESCALE as i64 => t,
            _ => now_dts() - TIMESCALE as i64 / 4,
        };
        run.on_frame(dts, &frame).await?;
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
    /// objects seen during this event
    objects: crate::objects::EventObjects,
    /// EventStart was published (deferred until the first object when the
    /// camera requires one)
    announced: bool,
    /// detections that arrived before the event row existed
    pending: Vec<(i64, Vec<crate::notify::DetectedObject>)>,
    /// label priority list for this camera (gate override or server default)
    labels: Vec<String>,
    /// `require_object` close postponed while a detection is in flight:
    /// the last-motion dts to close at once the answer arrives
    deferred_close: Option<i64>,
    /// changed cells of the frame being stepped (set by `on_frame`)
    frame_cells: Option<MotionCells>,
    /// changed cells at the peak: where the thumbnail's objects must be
    peak_cells: Option<MotionCells>,
    /// zones that fired on the frame being stepped (set by `on_frame`)
    frame_zones: Vec<String>,
    /// every zone that fired during this event, in the order first seen
    zones: Vec<String>,
}

impl EventState {
    fn meta(&self) -> serde_json::Value {
        let mut m = serde_json::json!({
            "frames": self.frames,
            "motion_frames": self.motion_frames,
            "peak": self.peak,
            "detections": self.objects.detections,
            "objects": self.objects.list(),
            "zones": self.zones,
        });
        if let Some(c) = &self.peak_cells {
            m["peak_cells"] = c.to_json();
        }
        m
    }

    /// Merge a detection result into the event it was taken for. A result
    /// for an event that has since closed is dropped; one taken before the
    /// event row existed is kept for the event that is about to open.
    fn on_objects(&mut self, ctx: &DetectCtx, cam: &Camera, dts: i64, sent_for: Option<i64>, objs: Vec<crate::notify::DetectedObject>) -> Result<()> {
        let Some(id) = self.event_id else {
            if sent_for.is_none() && !objs.is_empty() {
                self.pending.retain(|(d, _)| dts - *d < 10 * TIMESCALE as i64);
                self.pending.push((dts, objs));
            }
            return Ok(());
        };
        // a result taken before any event opened belongs here only if its frame is within the pre-roll
        let pre = (cam.pre_secs * TIMESCALE as f64) as i64;
        if sent_for.is_some_and(|s| s != id) || (sent_for.is_none() && dts < self.start_dts - pre) {
            debug!(camera = cam.id, event = id, "dropping detection for a closed event");
            return Ok(());
        }
        let changed = self.objects.merge(&objs);
        if !changed {
            return Ok(());
        }
        let labels = self.labels.clone();
        let kind = self.objects.kind(&labels);
        ctx.db.update_event_objects(id, &kind, &self.meta().to_string())?;
        info!(camera = cam.id, event = id, %kind, objects = ?self.objects.list().iter().map(|o| format!("{}:{:.2}", o.label, o.confidence)).collect::<Vec<_>>(), "objects");
        if self.announced {
            ctx.bus.publish_event(&ctx.db, id, crate::notify::Notification::EventUpdate);
        } else {
            self.announced = true;
            ctx.bus.publish_event(&ctx.db, id, crate::notify::Notification::EventStart);
        }
        Ok(())
    }

    /// Close the open event at `end_dts` (post-roll included), or discard it
    /// when the camera requires an object and none was seen. With a
    /// detection still in flight the decision waits for its answer.
    fn close(&mut self, ctx: &DetectCtx, cam: &Camera, last_motion_dts: i64, h: &DetectorHandle, in_flight: bool) -> Result<()> {
        let ts = TIMESCALE as i64;
        let Some(id) = self.event_id else { return Ok(()) };
        if cam.require_object && self.objects.is_empty() && in_flight {
            self.deferred_close = Some(last_motion_dts);
            return Ok(());
        }
        let peak_dts = self.peak_dts;
        h.status.write().in_event = None;
        if cam.require_object && self.objects.is_empty() {
            ctx.db.delete_event(id)?;
            info!(camera = cam.id, event = id, "event discarded: no object detected");
            *self = EventState::default();
            return Ok(());
        }
        let end = last_motion_dts + (cam.post_secs * ts as f64) as i64;
        ctx.db.close_event(id, end, None, &self.meta().to_string())?;
        info!(camera = cam.id, event = id, peak = self.peak, kind = %self.objects.kind(&[]), secs = (end - self.start_dts) / ts, "event end");
        ctx.bus.publish_event(&ctx.db, id, crate::notify::Notification::EventEnd);
        *self = EventState::default();
        // thumbnail in the background, from the open segment when the peak is still in it
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let (db, hub, ffmpeg, thumb_dir, bus, cam_id) = (ctx.db.clone(), ctx.hub.clone(), ctx.ffmpeg.clone(), ctx.thumb_dir.clone(), ctx.bus.clone(), cam.id);
            let classify = ctx.objects.clone().filter(|_| cam.objects).map(|d| (d, cam.clone()));
            rt.spawn(async move {
                if let Some(jpeg) = thumbnail_task(&db, Some(&hub), &ffmpeg, &thumb_dir, &bus, cam_id, id, peak_dts).await {
                    if let Some((det, cam)) = classify {
                        classify_task(&det, &db, &bus, &cam, id, jpeg).await;
                    }
                }
            });
        }
        Ok(())
    }

    fn step(&mut self, ctx: &DetectCtx, cam: &Camera, dts: i64, score: u8, h: &DetectorHandle, in_flight: bool) -> Result<()> {
        let ts = TIMESCALE as i64;
        if score > 0 {
            self.consecutive = self.consecutive.saturating_add(1);
            self.deferred_close = None; // motion resumed: the event simply continues
            for z in std::mem::take(&mut self.frame_zones) {
                if !self.zones.contains(&z) {
                    self.zones.push(z);
                }
            }
        } else {
            self.consecutive = 0;
            if self.event_id.is_none() {
                self.zones.clear(); // stray motion that never became an event
            }
        }
        if self.deferred_close.is_some() {
            return Ok(()); // waiting for the last detection before deciding
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
                    self.peak_cells = self.frame_cells.take();
                    self.last_motion_dts = dts;
                    self.last_db_update = dts;
                    self.motion_frames = 1;
                    self.frames = 1;
                    h.status.write().in_event = Some(id);
                    ctx.db.update_event_progress(id, score, dts)?;
                    // the zones that fired are known now: alert rules for a zone can act on the start
                    ctx.db.update_event_objects(id, "motion", &self.meta().to_string())?;
                    self.announced = !cam.require_object;
                    if self.announced {
                        ctx.bus.publish_event(&ctx.db, id, crate::notify::Notification::EventStart);
                    }
                    let pending = std::mem::take(&mut self.pending);
                    for (d, objs) in pending.into_iter().filter(|(d, _)| dts - *d < 10 * TIMESCALE as i64) {
                        self.on_objects(ctx, cam, d, None, objs)?;
                    }
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
                        self.peak_cells = self.frame_cells.take();
                    }
                    if dts - self.last_db_update > ts * 2 {
                        ctx.db.update_event_progress(id, self.peak, self.peak_dts)?;
                        self.last_db_update = dts;
                    }
                }
                let quiet = dts - self.last_motion_dts;
                let max_len = ts * 600; // hard cap 10 min from event start; a new event starts if motion continues
                if quiet > (cam.cooldown_secs * ts as f64) as i64 || dts - self.start_dts > max_len {
                    let last = self.last_motion_dts;
                    self.close(ctx, cam, last, h, in_flight)?;
                }
            }
        }
        Ok(())
    }
}

/// Make the thumbnail of a closed event, then tell everyone: the JPEG bytes
/// for MQTT, and an `event_update` whose `thumb` URL is now set (the
/// `event_end` went out before the picture existed).
/// Returns the JPEG so the caller can classify it.
#[allow(clippy::too_many_arguments)]
pub async fn thumbnail_task(db: &Db, hub: Option<&LiveHub>, ffmpeg: &str, thumb_dir: &std::path::Path, bus: &crate::notify::Bus, cam_id: i64, event_id: i64, peak_dts: i64) -> Option<Vec<u8>> {
    match make_thumbnail(db, hub, ffmpeg, thumb_dir, cam_id, event_id, peak_dts).await {
        Ok(jpeg) => {
            bus.publish(crate::notify::Notification::EventThumbnail { event_id, camera_id: cam_id, jpeg: bytes::Bytes::from(jpeg.clone()) });
            bus.publish_event(db, event_id, crate::notify::Notification::EventUpdate);
            Some(jpeg)
        }
        Err(e) => {
            error!(camera = cam_id, event = event_id, "thumbnail: {e:#}");
            None
        }
    }
}

/// Second opinion on a closed event from its full-resolution thumbnail
/// (see [`crate::objects::classify_event`]); announces changed objects.
pub async fn classify_task(det: &crate::objects::ObjectDetector, db: &Db, bus: &crate::notify::Bus, cam: &Camera, event_id: i64, jpeg: Vec<u8>) {
    match crate::objects::classify_event(det, db, cam, event_id, jpeg, &[]).await {
        Ok(true) => {
            info!(camera = cam.id, event = event_id, "objects updated from the thumbnail");
            bus.publish_event(db, event_id, crate::notify::Notification::EventUpdate);
        }
        Ok(false) => {}
        Err(e) => warn!(camera = cam.id, event = event_id, "classifying thumbnail: {e:#}"),
    }
}

/// Locate the fragment holding `dts`: an indexed segment first, else the
/// main recorder's open segment (already on disk, not yet indexed).
/// Returns (file, init_len, fragment, dts_offset).
fn locate_fragment(db: &Db, hub: Option<&LiveHub>, cam_id: i64, dts: i64) -> Result<Option<(PathBuf, u32, crate::mp4::FragEntry, i64)>> {
    if let Some(seg) = db.segments_in_range(cam_id, dts, dts + 1)?.first() {
        let frag = seg
            .index
            .iter()
            .find(|f| f.dts <= dts && dts < f.dts + f.duration as i64)
            .or_else(|| seg.index.first())
            .copied()
            .ok_or_else(|| anyhow::anyhow!("segment has no fragments"))?;
        let storage = db.storage(seg.storage_id)?.ok_or_else(|| anyhow::anyhow!("storage missing"))?;
        return Ok(Some((PathBuf::from(&storage.path).join(&seg.path), seg.init_len, frag, seg.dts_offset)));
    }
    if let Some(h) = hub.and_then(|h| h.get(cam_id)) {
        let open = h.open.read().clone();
        if let Some(o) = open {
            if let Some(f) = o.frags.iter().find(|f| f.dts <= dts && dts < f.dts + f.duration as i64) {
                return Ok(Some((o.abs_path, o.init_len, *f, 0)));
            }
        }
    }
    Ok(None)
}

/// Find the fragment containing `dts` (from the open segment when it is
/// still being written, retrying briefly until it has been flushed), decode
/// its keyframe and store a JPEG.
pub async fn make_thumbnail(db: &Db, hub: Option<&LiveHub>, ffmpeg: &str, thumb_dir: &std::path::Path, cam_id: i64, event_id: i64, dts: i64) -> Result<Vec<u8>> {
    let mut attempt = 0;
    loop {
        if let Some((path, init_len, frag, dts_offset)) = locate_fragment(db, hub, cam_id, dts)? {
            let init = crate::thumbs::read_range(&path, 0, init_len)?;
            let data = crate::mp4::rebase_fragment(&crate::thumbs::read_range(&path, frag.offset, frag.len)?, dts_offset);
            let jpeg = crate::thumbs::frag_to_jpeg(ffmpeg, &init, &data, 640, 5).await?;
            let rel = format!("{cam_id}/{event_id}.jpg");
            let abs = thumb_dir.join(&rel);
            std::fs::create_dir_all(abs.parent().unwrap())?;
            std::fs::write(&abs, &jpeg)?;
            db.set_event_thumb(event_id, &rel)?;
            debug!(camera = cam_id, event = event_id, bytes = jpeg.len(), "thumbnail written");
            return Ok(jpeg);
        }
        attempt += 1;
        if attempt > 40 {
            anyhow::bail!("no segment covers dts {dts} after waiting");
        }
        // the GOP holding the peak is flushed within a few seconds; the
        // segment close (up to 60 s) is no longer waited for
        tokio::time::sleep(Duration::from_secs(if attempt < 10 { 2 } else { 6 })).await;
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
    fn motion_cells_mark_what_moved_and_round_trip() {
        let (w, h) = (160usize, 90usize);
        let mut det = MotionDetector::new(w, h, 25, 0.5, 0.2, &[], &[]);
        let base = vec![100u8; w * h];
        for _ in 0..12 { det.feed(&base); }
        let mut f = base.clone();
        for y in 20..50 { for x in 40..70 { f[y * w + x] = 250; } } // x 0.25..0.44, y 0.22..0.56
        assert!(det.feed(&f) > 0);
        let c = det.cells();
        assert_eq!((c.bw, c.bh), (20, 12));
        assert!(c.overlaps([0.3, 0.3, 0.4, 0.5], 0), "the moving block");
        assert!(!c.overlaps([0.7, 0.1, 0.95, 0.4], 1), "a parked bus elsewhere");
        assert!(c.overlaps([0.47, 0.3, 0.6, 0.5], 1), "next to it, within the margin");
        let back = MotionCells::from_json(&c.to_json()).unwrap();
        assert_eq!(back, c);
        assert!(MotionCells::from_json(&serde_json::json!({"w": 1})).is_none());
    }

    #[test]
    fn showinfo_clock_reads_absolute_frame_times() {
        let mut c = ShowinfoClock::default();
        assert_eq!(c.line("[Parsed_showinfo_3 @ 0x1] n:   0 pts:5 pts_time:1"), None, "no time base yet");
        assert_eq!(c.line("[Parsed_showinfo_3 @ 0x55] config in time_base: 1/5, frame_rate: 5/1"), None);
        assert_eq!(c.line("[Parsed_showinfo_3 @ 0x55] n:   0 pts:8954710124 pts_time:1.79094e+09 duration:1"), Some(8_954_710_124 * 18_000));
        let mut c = ShowinfoClock::default();
        c.line("[Parsed_showinfo_3 @ 0x55] config in time_base: 1/90000, frame_rate: 10/1");
        assert_eq!(c.line("[Parsed_showinfo_3 @ 0x55] n:  12 pts:161184781964459 pts_time:1.79094e+09"), Some(161_184_781_964_459));
        assert_eq!(c.line("[mp4 @ 0x1] some other line pts:3"), None);
    }

    #[test]
    fn detect_key_covers_object_settings() {
        let a = Camera::example();
        let mut b = a.clone();
        assert_eq!(detect_key(&a), detect_key(&b));
        b.object_labels = "person".into();
        assert_ne!(detect_key(&a), detect_key(&b));
        let mut c = a.clone();
        c.require_object = true;
        assert_ne!(detect_key(&a), detect_key(&c));
        let mut d = a.clone();
        d.name = "renamed".into();
        assert_eq!(detect_key(&a), detect_key(&d), "a rename does not restart the detector");
    }

    /// Each zone has its own sensitivity: a small change in a sensitive
    /// doorway zone fires (and is named), the same change in a dull road
    /// zone does not; a mask over part of a zone hides that part.
    #[test]
    fn zones_fire_on_their_own_sensitivity() {
        use crate::zones::Zone;
        let (w, h) = (160usize, 90usize);
        let z = |name: &str, pts: Vec<(f64, f64)>, area: Option<f64>, blob: Option<f64>| Zone { name: name.into(), points: pts, min_area_pct: area, min_blob_pct: blob };
        let left = vec![(0.0, 0.0), (0.5, 0.0), (0.5, 1.0), (0.0, 1.0)];
        let right = vec![(0.5, 0.0), (1.0, 0.0), (1.0, 1.0), (0.5, 1.0)];
        // camera default: 5% area; "Door" (left) fires at 0.5%, "Road" (right) keeps the default
        let zones = [z("Door", left.clone(), Some(0.5), Some(0.2)), z("Road", right.clone(), None, None)];
        let mut det = MotionDetector::with_zones(w, h, 25, 5.0, 2.0, &zones, &[]);
        let base = vec![100u8; w * h];
        for _ in 0..12 { assert_eq!(det.feed(&base), 0); }
        // a 12x12 object (~2% of a half) in each half, one at a time
        let blob = |x0: usize| { let mut f = base.clone(); for y in 30..42 { for x in x0..x0 + 12 { f[y * w + x] = 250; } } f };
        let s = det.feed(&blob(20));
        assert!(s > 0, "door fires");
        assert_eq!(det.fired_zones(), vec!["Door".to_string()]);
        for _ in 0..40 { det.feed(&base); } // settle back
        assert_eq!(det.feed(&blob(110)), 0, "road needs 5%");
        assert!(det.fired_zones().is_empty());
        let rep = det.zone_report();
        assert_eq!(rep.len(), 2);
        assert!(rep[1].2 > 1.0 && rep[1].1 == 0, "{rep:?}");
        // both halves at once, big enough for the road too: both named, best first
        for _ in 0..40 { det.feed(&base); }
        let mut f = base.clone();
        for y in 10..80 { for x in 85..150 { f[y * w + x] = 250; } }
        for y in 30..42 { for x in 20..32 { f[y * w + x] = 250; } }
        assert!(det.feed(&f) > 0);
        assert_eq!(det.fired_zones(), vec!["Road".to_string(), "Door".to_string()]);
        // a mask over the door's top half hides motion there
        let mut masked = MotionDetector::with_zones(w, h, 25, 5.0, 2.0, &zones, &[z("", vec![(0.0, 0.0), (0.5, 0.0), (0.5, 0.5), (0.0, 0.5)], None, None)]);
        for _ in 0..12 { masked.feed(&base); }
        assert_eq!(masked.feed(&blob(20)), 0, "rows 30..42 of 90 are under the mask");
    }

    /// No zones: the whole frame is one unnamed zone; fired_zones stays empty.
    #[test]
    fn whole_frame_without_zones_has_no_zone_names() {
        let (w, h) = (160usize, 90usize);
        let mut det = MotionDetector::with_zones(w, h, 25, 0.5, 0.2, &[], &[]);
        let base = vec![100u8; w * h];
        for _ in 0..12 { det.feed(&base); }
        let mut f = base.clone();
        for y in 20..50 { for x in 40..70 { f[y * w + x] = 250; } }
        assert!(det.feed(&f) > 0);
        assert!(det.fired_zones().is_empty());
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
