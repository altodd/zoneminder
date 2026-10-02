//! Dry run: replay recorded video through the motion detector with other
//! settings (edited zones, sensitivities, threshold) and report the events
//! they would have produced, without writing anything. This is how zones are
//! tuned against last night instead of waiting for the next one.
//!
//! The substream recording (or the main stream for cameras that do not
//! record one) is read from disk and decoded by ffmpeg as fast as it goes,
//! the first frame of every `1/detect_fps` seconds of recording (no frames
//! invented across gaps). Events follow the same rules as the live detector
//! (two scored frames in a row open one; quiet for `cooldown_secs` or 10
//! minutes long closes it; pre/post-roll added). Object detection is not
//! replayed.

use anyhow::{Context, Result};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::db::Camera;
use crate::detect::MotionDetector;
use crate::mp4::TIMESCALE;

/// Longest range one dry run covers.
pub const MAX_RANGE_MS: i64 = 12 * 3600 * 1000;
/// Dry runs at once, all cameras together (each holds an ffmpeg decode).
pub const MAX_JOBS: usize = 2;
/// Longest a dry run may take, whatever its range.
pub const MAX_RUN: Duration = Duration::from_secs(2 * 3600);
/// No frame from ffmpeg for this long: the run is stuck.
const STALL: Duration = Duration::from_secs(120);

/// Settings to try; absent fields keep the camera's.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Settings {
    pub zones_json: Option<String>,
    pub masks_json: Option<String>,
    pub pixel_threshold: Option<u8>,
    pub min_area_pct: Option<f64>,
    pub min_blob_pct: Option<f64>,
    pub cooldown_secs: Option<f64>,
}

impl Settings {
    /// The camera with these settings applied (validated like a camera patch).
    pub fn apply(&self, cam: &Camera) -> Result<Camera> {
        let mut c = cam.clone();
        if let Some(z) = &self.zones_json {
            crate::zones::validate(z, "zones")?;
            c.zones_json = z.clone();
        }
        if let Some(m) = &self.masks_json {
            crate::zones::validate(m, "masks")?;
            c.masks_json = m.clone();
        }
        if let Some(t) = self.pixel_threshold {
            anyhow::ensure!(t >= 1, "pixel_threshold must be 1..255");
            c.pixel_threshold = t;
        }
        for (name, v, slot) in [("min_area_pct", self.min_area_pct, &mut c.min_area_pct), ("min_blob_pct", self.min_blob_pct, &mut c.min_blob_pct)] {
            if let Some(v) = v {
                anyhow::ensure!((0.0..=100.0).contains(&v), "{name} must be 0..100");
                *slot = v;
            }
        }
        if let Some(v) = self.cooldown_secs {
            anyhow::ensure!((0.0..=600.0).contains(&v), "cooldown_secs must be 0..600");
            c.cooldown_secs = v;
        }
        Ok(c)
    }
}

/// An event the settings would have produced.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct SimEvent {
    pub start_ms: i64,
    pub end_ms: i64,
    pub peak_ms: i64,
    pub score: u8,
    pub zones: Vec<String>,
    /// seconds with motion scored
    pub motion_secs: f64,
}

/// The live detector's event rules, without the database.
pub struct Sim {
    det: MotionDetector,
    pre: i64,
    post: i64,
    cooldown: i64,
    /// one detector interval, 90 kHz
    frame_ticks: i64,
    consecutive: u8,
    pending_zones: Vec<String>,
    open: Option<(i64, SimEvent, i64, i64)>, // (first motion dts, event, last motion dts, motion ticks)
    prev_dts: Option<i64>,
    pub events: Vec<SimEvent>,
    pub frames: u64,
}

impl Sim {
    pub fn new(cam: &Camera) -> Sim {
        let zones = crate::zones::parse(&cam.zones_json, "Zone");
        let masks = crate::zones::parse(&cam.masks_json, "Mask");
        let ts = TIMESCALE as f64;
        Sim {
            det: MotionDetector::with_zones(cam.detect_width as usize, cam.detect_height as usize, cam.pixel_threshold, cam.min_area_pct, cam.min_blob_pct, &zones, &masks),
            pre: (cam.pre_secs * ts) as i64,
            post: (cam.post_secs * ts) as i64,
            cooldown: (cam.cooldown_secs * ts) as i64,
            frame_ticks: (ts / cam.detect_fps.max(0.5)) as i64,
            consecutive: 0,
            pending_zones: Vec::new(),
            open: None,
            prev_dts: None,
            events: Vec::new(),
            frames: 0,
        }
    }

    /// Close the open event; like the live detector, start over (a new
    /// event needs two scored frames in a row again).
    fn close(&mut self) {
        if let Some((_, mut e, last, motion_ticks)) = self.open.take() {
            e.end_ms = (last + self.post) / 90;
            e.motion_secs = (motion_ticks as f64 / TIMESCALE as f64 * 10.0).round() / 10.0;
            self.events.push(e);
        }
        self.consecutive = 0;
        self.pending_zones.clear();
    }

    /// One grayscale frame (the Y plane) at `dts` (90 kHz).
    pub fn feed(&mut self, dts: i64, y: &[u8]) {
        self.frames += 1;
        // seconds of motion from the real frame spacing (a gap in the
        // recording counts as at most two intervals)
        let gap = self.prev_dts.map_or(self.frame_ticks, |p| (dts - p).clamp(0, 2 * self.frame_ticks));
        self.prev_dts = Some(dts);
        let score = self.det.feed(y);
        if score > 0 {
            self.consecutive = self.consecutive.saturating_add(1);
            for z in self.det.fired_zones() {
                if !self.pending_zones.contains(&z) {
                    self.pending_zones.push(z);
                }
            }
        } else {
            self.consecutive = 0;
            if self.open.is_none() {
                self.pending_zones.clear();
            }
        }
        match &mut self.open {
            None => {
                if self.consecutive >= 2 {
                    let zones = std::mem::take(&mut self.pending_zones);
                    self.open = Some((dts, SimEvent { start_ms: (dts - self.pre) / 90, end_ms: 0, peak_ms: dts / 90, score, zones, motion_secs: 0.0 }, dts, gap));
                }
            }
            Some((first, e, last, motion_ticks)) => {
                if score > 0 {
                    *last = dts;
                    *motion_ticks += gap;
                    if score > e.score {
                        e.score = score;
                        e.peak_ms = dts / 90;
                    }
                    for z in std::mem::take(&mut self.pending_zones) {
                        if !e.zones.contains(&z) {
                            e.zones.push(z);
                        }
                    }
                }
                let (first, last) = (*first, *last);
                if dts - last > self.cooldown || dts - first > 600 * TIMESCALE as i64 {
                    self.close();
                }
            }
        }
    }

    /// End of the replay: close an event still open.
    pub fn finish(&mut self) {
        self.close();
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct JobView {
    pub id: String,
    pub camera_id: i64,
    pub start_ms: i64,
    pub end_ms: i64,
    pub stream: String,
    /// "running", "done", "failed", "cancelled"
    pub state: String,
    pub progress: f64,
    pub frames: u64,
    pub error: Option<String>,
    pub events: Vec<SimEvent>,
    /// events actually recorded in the range, for comparison
    pub recorded_events: usize,
    pub settings: Settings,
}

pub struct Job {
    pub view: RwLock<JobView>,
    pub abort: RwLock<Option<tokio::task::AbortHandle>>,
    pub finished_ms: RwLock<Option<i64>>,
}

impl Job {
    /// Stop a running job: its ffmpeg is killed and the state is
    /// "cancelled". No effect once it ended.
    pub fn cancel(&self) {
        let abort = self.abort.write().take();
        if let Some(a) = abort {
            a.abort();
        }
        let mut v = self.view.write();
        if v.state == "running" {
            v.state = "cancelled".into();
            *self.finished_ms.write() = Some(crate::db::now_dts() / 90);
        }
    }

    /// Record how the run ended, unless it was cancelled meanwhile.
    fn finish(&self, res: Result<()>) {
        let mut v = self.view.write();
        if v.state != "running" {
            return;
        }
        match res {
            Ok(()) => {
                v.state = "done".into();
                v.progress = 1.0;
            }
            Err(e) => {
                v.state = "failed".into();
                v.error = Some(format!("{e:#}"));
            }
        }
        *self.finished_ms.write() = Some(crate::db::now_dts() / 90);
        drop(v);
        self.abort.write().take();
    }
}

pub type Jobs = Arc<parking_lot::Mutex<std::collections::HashMap<String, Arc<Job>>>>;

/// Start a dry run of `cam` (with `settings`) over [start_ms, end_ms).
/// Refused when [`MAX_JOBS`] are running; a camera's earlier run is
/// cancelled.
pub fn start(jobs: &Jobs, db: crate::db::Db, ffmpeg: String, cam: &Camera, settings: Settings, start_ms: i64, end_ms: i64) -> Result<Arc<Job>> {
    anyhow::ensure!(end_ms > start_ms && end_ms - start_ms <= MAX_RANGE_MS, "range must be 0 < end-start <= 12 h");
    let tried = settings.apply(cam)?;
    let (start_dts, end_dts) = (start_ms * 90, end_ms * 90);
    let stream = if !db.segments_in_range_stream(cam.id, "sub", start_dts, end_dts)?.is_empty() { "sub" } else { "main" };
    let recorded_events = db.events_in_range(cam.id, start_dts, end_dts)?.len();
    let now = crate::db::now_dts() / 90;
    let mut map = jobs.lock();
    // forget finished runs after an hour; cancel this camera's previous run
    map.retain(|_, j| j.finished_ms.read().is_none_or(|t| now - t < 3_600_000));
    for j in map.values() {
        if j.view.read().camera_id == cam.id {
            j.cancel();
        }
    }
    anyhow::ensure!(map.values().filter(|j| j.view.read().state == "running").count() < MAX_JOBS, "{MAX_JOBS} dry runs are already running; try again when one finishes");
    let id = crate::api::new_token()[..16].to_string();
    let job = Arc::new(Job {
        view: RwLock::new(JobView {
            id: id.clone(), camera_id: cam.id, start_ms, end_ms, stream: stream.into(), state: "running".into(), progress: 0.0, frames: 0,
            error: None, events: Vec::new(), recorded_events, settings,
        }),
        abort: RwLock::new(None),
        finished_ms: RwLock::new(None),
    });
    // the run, and a watcher that records how it ended: an error, the time
    // limit, a panic or a cancel never leaves a job "running"
    let j2 = job.clone();
    let work = tokio::spawn(async move { tokio::time::timeout(MAX_RUN, run(&db, &ffmpeg, &tried, stream, start_dts, end_dts, &j2)).await });
    *job.abort.write() = Some(work.abort_handle());
    let j3 = job.clone();
    tokio::spawn(async move {
        let res = match work.await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(anyhow::anyhow!("stopped after {} minutes", MAX_RUN.as_secs() / 60)),
            Err(e) if e.is_cancelled() => Err(anyhow::anyhow!("cancelled")),
            Err(e) => Err(anyhow::anyhow!("the dry run stopped unexpectedly: {e}")),
        };
        j3.finish(res);
    });
    // visible to other requests only with its abort handle in place
    map.insert(id, job.clone());
    drop(map);
    Ok(job)
}

async fn run(db: &crate::db::Db, ffmpeg: &str, cam: &Camera, stream: &str, start: i64, end: i64, job: &Job) -> Result<()> {
    let plan = crate::video::plan_range_stream(db, cam.id, stream, start, end)?;
    anyhow::ensure!(!plan.items.is_empty(), "nothing was recorded in this range");
    let (w, h) = (cam.detect_width as usize, cam.detect_height as usize);
    let fps = cam.detect_fps.max(0.5);
    let mut child = crate::thumbs::ffmpeg_command(ffmpeg)
        .args([
            "-nostdin", "-loglevel", "info", "-nostats", "-threads", "1", "-copyts",
            "-f", "mp4", "-i", "pipe:0", "-an", "-sn", "-dn", "-threads", "1", "-filter_threads", "1",
            // the first frame of every 1/fps seconds of recording (the live
            // detector's rate); nothing is invented across gaps
            "-vf", &format!("select=isnan(prev_selected_t)+gt(floor(t*{fps})\\,floor(prev_selected_t*{fps})),scale={w}:{h}:flags=fast_bilinear,format=yuv420p,showinfo"),
            "-fps_mode", "passthrough", "-f", "rawvideo", "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawning ffmpeg for the dry run")?;
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let feeder = tokio::spawn(async move {
        use futures::StreamExt;
        let mut s = Box::pin(crate::video::stream_plan(plan));
        while let Some(chunk) = s.next().await {
            let chunk = chunk.context("reading the recording")?;
            if stdin.write_all(&chunk).await.is_err() {
                break; // ffmpeg stopped reading; its exit status says why
            }
        }
        anyhow::Ok(())
    });
    // the last lines ffmpeg printed, for the error when it fails
    let tail = Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::<String>::new()));
    let t2 = tail.clone();
    let ts = TIMESCALE as i64;
    let mut times = crate::detect::FrameTimes::spawn(stderr, Duration::from_secs(5), start - 60 * ts..end + 60 * ts, move |l| {
        let l = l.trim();
        if !l.is_empty() {
            let mut t = t2.lock();
            if t.len() == 4 {
                t.pop_front();
            }
            t.push_back(l.to_string());
        }
    });
    let step = (ts as f64 / fps) as i64;
    let mut sim = Sim::new(cam);
    let mut frame = vec![0u8; w * h * 3 / 2];
    loop {
        match tokio::time::timeout(STALL, stdout.read_exact(&mut frame)).await {
            Err(_) => anyhow::bail!("ffmpeg produced no frame for {} s", STALL.as_secs()),
            Ok(Ok(_)) => {}
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Ok(Err(e)) => return Err(e.into()),
        }
        let dts = times.next(|prev| prev.map_or(start, |p| p + step)).await;
        sim.feed(dts, &frame[..w * h]);
        if sim.frames.is_multiple_of(25) {
            let mut v = job.view.write();
            v.frames = sim.frames;
            v.progress = ((dts - start) as f64 / (end - start) as f64).clamp(0.0, 0.99);
            v.events = sim.events.clone();
        }
    }
    let fed = feeder.await;
    let status = child.wait().await.context("waiting for ffmpeg")?;
    sim.finish();
    {
        let mut v = job.view.write();
        v.frames = sim.frames;
        v.events = sim.events.clone();
    }
    // what was decoded stays visible; the run still counts as failed
    fed.map_err(|e| anyhow::anyhow!("feeding ffmpeg: {e}"))??;
    if !status.success() {
        let tail: Vec<String> = tail.lock().iter().cloned().collect();
        anyhow::bail!("ffmpeg failed ({status}): {}", tail.join(" / "));
    }
    anyhow::ensure!(sim.frames > 0, "ffmpeg decoded no frames from the recording");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The simulated events follow the live rules: two scored frames open
    /// one (with pre-roll), quiet longer than the cooldown closes it (with
    /// post-roll), zones are named, and the end of the replay closes the last.
    #[test]
    fn sim_opens_and_closes_events_like_the_live_detector() {
        let mut cam = Camera::example();
        cam.detect_width = 160;
        cam.detect_height = 90;
        cam.detect_fps = 5.0;
        cam.pre_secs = 2.0;
        cam.post_secs = 3.0;
        cam.cooldown_secs = 4.0;
        cam.zones_json = r#"[{"name":"Left","points":[[0,0],[0.5,0],[0.5,1],[0,1]]}]"#.into();
        let mut sim = Sim::new(&cam);
        let (w, h) = (160usize, 90usize);
        let base = vec![100u8; w * h];
        let ts = TIMESCALE as i64;
        let mut t = 1_800_000_000 * ts;
        let tick = ts / 5;
        let feed = |sim: &mut Sim, f: &[u8], t: &mut i64| { sim.feed(*t, f); *t += tick; };
        for _ in 0..20 { feed(&mut sim, &base, &mut t); }
        // a moving object for 2 s in the left half
        let t_motion = t;
        for k in 0..10 {
            let mut f = base.clone();
            for y in 20..50 { for x in (10 + k * 3)..(40 + k * 3) { f[y * w + x] = 250; } }
            feed(&mut sim, &f, &mut t);
        }
        for _ in 0..40 { feed(&mut sim, &base, &mut t); } // 8 s quiet: closes
        // something in the right half only: outside the zone
        for k in 0..10 {
            let mut f = base.clone();
            for y in 20..50 { for x in (100 + k)..(130 + k) { f[y * w + x] = 250; } }
            feed(&mut sim, &f, &mut t);
        }
        sim.finish();
        assert_eq!(sim.events.len(), 1, "{:?}", sim.events);
        let e = &sim.events[0];
        assert_eq!(e.zones, vec!["Left".to_string()]);
        // opened on the second scored frame, minus 2 s pre-roll
        assert_eq!(e.start_ms, (t_motion + tick - 2 * ts) / 90);
        assert!(e.end_ms > e.start_ms && e.score > 0);
        assert!(e.motion_secs >= 1.0, "{}", e.motion_secs);
    }

    /// An event reaching the 10-minute cap closes; the next one needs two
    /// scored frames in a row again, as live. Motion seconds follow the real
    /// frame spacing.
    #[test]
    fn sim_caps_events_at_ten_minutes_like_the_live_detector() {
        let mut cam = Camera::example();
        cam.detect_width = 160;
        cam.detect_height = 90;
        cam.detect_fps = 1.0;
        cam.pre_secs = 0.0;
        cam.post_secs = 0.0;
        cam.min_area_pct = 0.5;
        cam.min_blob_pct = 0.2;
        let mut sim = Sim::new(&cam);
        let (w, h) = (160usize, 90usize);
        let base = vec![100u8; w * h];
        let ts = TIMESCALE as i64;
        let t0 = 1_800_000_000 * ts;
        for i in 0..12 {
            sim.feed(t0 + i * ts, &base);
        }
        // a box on alternate sides every second: motion on every frame
        let mut frames = [base.clone(), base.clone()];
        for (k, f) in frames.iter_mut().enumerate() {
            for y in 20..60 { for x in (10 + k * 100)..(50 + k * 100) { f[y * w + x] = 250; } }
        }
        let first = 12;
        for i in first..first + 700 {
            sim.feed(t0 + i * ts, &frames[(i % 2) as usize]);
        }
        sim.finish();
        assert_eq!(sim.events.len(), 2, "{:?}", sim.events);
        let open1 = first + 1; // the second scored frame
        assert_eq!(sim.events[0].start_ms, (t0 + open1 * ts) / 90);
        // closed on the first frame more than 600 s after it opened, then two frames to open again
        let closed = open1 + 601;
        assert_eq!(sim.events[1].start_ms, (t0 + (closed + 2) * ts) / 90);
        assert!((sim.events[0].motion_secs - 601.0).abs() <= 1.0, "{}", sim.events[0].motion_secs);
    }

    #[test]
    fn settings_override_the_camera_and_are_validated() {
        let cam = Camera::example();
        let s = Settings { pixel_threshold: Some(40), min_area_pct: Some(2.0), zones_json: Some(r#"[{"name":"A","points":[[0,0],[1,0],[1,1]]}]"#.into()), ..Default::default() };
        let c = s.apply(&cam).unwrap();
        assert_eq!((c.pixel_threshold, c.min_area_pct), (40, 2.0));
        assert!(c.zones_json.contains("\"A\""));
        assert!(Settings { zones_json: Some("[[[0,0]]]".into()), ..Default::default() }.apply(&cam).is_err());
        assert!(Settings { min_blob_pct: Some(200.0), ..Default::default() }.apply(&cam).is_err());
    }
}
