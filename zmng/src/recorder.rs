//! Per-camera passthrough recorder.
//!
//! One tokio task per camera pulls the main RTSP stream with `retina`, groups
//! access units into GOP-sized fMP4 fragments (`mp4::fragment`) and appends
//! them to a segment file.  A segment is closed at the first keyframe after
//! `segment_secs` and inserted into the SQLite index as ONE row with a compact
//! fragment index blob.  No decoding happens here, ever.
//!
//! The recorder also fans closed fragments out to live viewers through a
//! broadcast channel (`LiveHub`), so full-resolution live view costs nothing
//! extra on the server: the browser's MSE decoder does the work.

use anyhow::{Context, Result};
use bytes::Bytes;
use futures::StreamExt;
use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::db::{Camera, Db, Storage};
use crate::mp4::{self, AudioParams, Codec, FragEntry, Sample, VideoParams, TIMESCALE};

/// One closed fragment, published to live viewers.
#[derive(Clone)]
pub struct LiveFrag {
    pub params: Arc<VideoParams>,
    pub init: Bytes,
    pub dts: i64,
    pub duration: u32,
    pub data: Bytes,
}

/// Live status of a camera's recorder, for the UI/API.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct CamStatus {
    pub connected: bool,
    pub codec: Option<String>,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub kbps: f32,
    pub last_frame_dts: i64,
    pub last_error: Option<String>,
    pub reconnects: u32,
    pub current_segment: Option<String>,
    pub drift_ms: i64,
}

/// Per-camera motion log written by the detector and read by the recorder at
/// fragment close so the segment index carries a motion score per fragment.
#[derive(Default)]
pub struct MotionLog {
    ring: Mutex<VecDeque<(i64, u8)>>, // (dts, score)
}

impl MotionLog {
    pub fn push(&self, dts: i64, score: u8) {
        let mut r = self.ring.lock();
        r.push_back((dts, score));
        while r.len() > 4096 {
            r.pop_front();
        }
    }
    /// Max score in [start, end).
    pub fn max_in(&self, start: i64, end: i64) -> u8 {
        let r = self.ring.lock();
        r.iter().filter(|(d, _)| *d >= start && *d < end).map(|(_, s)| *s).max().unwrap_or(0)
    }
    pub fn latest(&self) -> Option<(i64, u8)> {
        self.ring.lock().back().copied()
    }
}

/// Which of a camera's two RTSP streams a recorder handles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamKind {
    Main,
    Sub,
}

impl StreamKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StreamKind::Main => "main",
            StreamKind::Sub => "sub",
        }
    }
    pub fn parse(s: &str) -> Option<StreamKind> {
        match s {
            "main" => Some(StreamKind::Main),
            "sub" => Some(StreamKind::Sub),
            _ => None,
        }
    }
}

/// Shared handles for one recorder (one per camera stream).
pub struct CamHandle {
    pub kind: StreamKind,
    pub camera: RwLock<Camera>,
    pub status: RwLock<CamStatus>,
    pub live: broadcast::Sender<LiveFrag>,
    /// Most recent init segment + last few fragments so a new live viewer can
    /// start immediately from a keyframe.
    pub recent: RwLock<Option<Recent>>,
    /// The segment being written: path and the fragments flushed so far, so
    /// a thumbnail or frame can be read before the segment closes.
    pub open: RwLock<Option<OpenSegmentInfo>>,
    /// Shared between the main and sub recorder of a camera.
    pub motion: Arc<MotionLog>,
    pub stop: tokio::sync::watch::Sender<bool>,
}

pub struct Recent {
    pub params: Arc<VideoParams>,
    pub init: Bytes,
    pub frags: VecDeque<(i64, u32, Bytes)>,
}

/// What is known about the segment currently being written.
#[derive(Clone, Debug)]
pub struct OpenSegmentInfo {
    pub abs_path: PathBuf,
    pub init_len: u32,
    pub frags: Vec<FragEntry>,
}

pub struct LiveHub {
    /// main-stream recorders by camera id
    pub cams: RwLock<HashMap<i64, Arc<CamHandle>>>,
    /// substream recorders by camera id (only when `record_sub` is on)
    pub subs: RwLock<HashMap<i64, Arc<CamHandle>>>,
    /// motion/object detectors by camera id
    pub detectors: RwLock<HashMap<i64, Arc<crate::detect::DetectorHandle>>>,
}

impl LiveHub {
    pub fn new() -> Self {
        LiveHub { cams: RwLock::new(HashMap::new()), subs: RwLock::new(HashMap::new()), detectors: RwLock::new(HashMap::new()) }
    }
    pub fn get(&self, id: i64) -> Option<Arc<CamHandle>> {
        self.cams.read().get(&id).cloned()
    }
    pub fn get_stream(&self, id: i64, kind: StreamKind) -> Option<Arc<CamHandle>> {
        match kind {
            StreamKind::Main => self.get(id),
            StreamKind::Sub => self.subs.read().get(&id).cloned(),
        }
    }
    pub fn all_handles(&self) -> Vec<Arc<CamHandle>> {
        let mut v: Vec<_> = self.cams.read().values().cloned().collect();
        v.extend(self.subs.read().values().cloned());
        v
    }
    /// Stop and forget every recorder and detector of a camera.
    pub fn remove_camera(&self, id: i64) {
        if let Some(h) = self.cams.write().remove(&id) {
            let _ = h.stop.send(true);
        }
        if let Some(h) = self.subs.write().remove(&id) {
            let _ = h.stop.send(true);
        }
        if let Some(d) = self.detectors.write().remove(&id) {
            let _ = d.stop.send(true);
        }
    }
}

impl Default for LiveHub {
    fn default() -> Self {
        Self::new()
    }
}

pub struct RecorderCtx {
    pub db: Db,
    pub hub: Arc<LiveHub>,
    pub segment_secs: u32,
    pub fsync: bool,
}

/// Spawn (or respawn) recorder tasks for every enabled camera and stream.
/// Safe to call repeatedly: running recorders are left alone, removed or
/// disabled ones are stopped, changed URLs cause a restart.
pub async fn reconcile(ctx: Arc<RecorderCtx>) -> Result<()> {
    let cams = ctx.db.cameras()?;
    let enabled: Vec<&Camera> = cams.iter().filter(|c| c.enabled).collect();
    // motion logs are shared per camera; collect them before taking write locks
    let mut motions: HashMap<i64, Arc<MotionLog>> = HashMap::new();
    for h in ctx.hub.all_handles() {
        motions.entry(h.camera.read().id).or_insert_with(|| h.motion.clone());
    }
    for kind in [StreamKind::Main, StreamKind::Sub] {
        let wanted: Vec<&Camera> = enabled
            .iter()
            .copied()
            .filter(|c| kind == StreamKind::Main || (c.record_sub && c.sub_url.as_deref().map(|u| u.starts_with("rtsp")).unwrap_or(false)))
            .collect();
        let map = match kind {
            StreamKind::Main => &ctx.hub.cams,
            StreamKind::Sub => &ctx.hub.subs,
        };
        let mut hub = map.write();
        let keep: Vec<i64> = wanted.iter().map(|c| c.id).collect();
        for id in hub.keys().filter(|id| !keep.contains(id)).copied().collect::<Vec<_>>() {
            if let Some(h) = hub.remove(&id) {
                let _ = h.stop.send(true);
                info!(camera = id, stream = kind.as_str(), "stopping recorder");
            }
        }
        for cam in wanted {
            if let Some(h) = hub.get(&cam.id) {
                let changed = {
                    let cur = h.camera.read();
                    stream_url(&cur, kind) != stream_url(cam, kind) || cur.storage_id != cam.storage_id
                        || (kind == StreamKind::Main && cur.record_audio != cam.record_audio)
                };
                if changed {
                    let _ = h.stop.send(true);
                    hub.remove(&cam.id);
                    info!(camera = cam.id, stream = kind.as_str(), "restarting recorder (config changed)");
                } else {
                    *h.camera.write() = cam.clone();
                    continue;
                }
            }
            // the motion log is per camera and shared by both streams
            let motion = motions.entry(cam.id).or_default().clone();
            let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
            let (live_tx, _) = broadcast::channel(64);
            let handle = Arc::new(CamHandle {
                kind,
                camera: RwLock::new(cam.clone()),
                status: RwLock::new(CamStatus::default()),
                live: live_tx,
                recent: RwLock::new(None),
                open: RwLock::new(None),
                motion,
                stop: stop_tx,
            });
            hub.insert(cam.id, handle.clone());
            let ctx2 = ctx.clone();
            tokio::spawn(async move {
                // supervisor: a panic inside the recorder must not silently stop recording
                loop {
                    let (c, h, s) = (ctx2.clone(), handle.clone(), stop_rx.clone());
                    let res = tokio::spawn(async move { run_camera(c, h, s).await }).await;
                    if *stop_rx.borrow() {
                        return;
                    }
                    if let Err(e) = res {
                        error!(camera = handle.camera.read().id, "recorder task panicked: {e}; restarting in 5 s");
                        handle.status.write().last_error = Some(format!("panic: {e}"));
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    } else {
                        return;
                    }
                }
            });
        }
    }
    Ok(())
}

fn stream_url(cam: &Camera, kind: StreamKind) -> String {
    match kind {
        StreamKind::Main => cam.main_url.clone(),
        StreamKind::Sub => cam.sub_url.clone().unwrap_or_default(),
    }
}

async fn run_camera(ctx: Arc<RecorderCtx>, h: Arc<CamHandle>, mut stop: tokio::sync::watch::Receiver<bool>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if *stop.borrow() {
            return;
        }
        let cam = h.camera.read().clone();
        let storage = match ctx.db.storage(cam.storage_id) {
            Ok(Some(s)) => s,
            _ => {
                error!(camera = cam.id, "storage {} missing", cam.storage_id);
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }
        };
        let started = Instant::now();
        let stop2 = stop.clone();
        let res = tokio::select! {
            r = record_session(&ctx, &h, &cam, &storage, &stop2) => r,
            _ = stop.changed() => return,
        };
        {
            let mut st = h.status.write();
            st.connected = false;
            st.reconnects += 1;
            st.fps = 0.0;
            st.kbps = 0.0;
        }
        match res {
            Ok(()) => info!(camera = cam.id, "session ended cleanly"),
            Err(e) => {
                warn!(camera = cam.id, name = %cam.name, error = %format!("{e:#}"), "recorder session failed");
                h.status.write().last_error = Some(format!("{e:#}"));
            }
        }
        // reset backoff after a healthy long session
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {},
            _ = stop.changed() => return,
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

fn dts_from_wall(t: SystemTime) -> i64 {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    (d.as_nanos() as i128 * TIMESCALE as i128 / 1_000_000_000) as i64
}

/// Open state of the segment file being written.
struct OpenSegment {
    file: std::fs::File,
    rel_path: String,
    abs_path: PathBuf,
    start_dts: i64,
    pos: u64,
    init_len: u32,
    frags: Vec<FragEntry>,
    seq: u32,
}

struct Pending {
    dts: i64,
    is_key: bool,
    data: Bytes,
}

async fn record_session(ctx: &Arc<RecorderCtx>, h: &Arc<CamHandle>, cam: &Camera, storage: &Storage, stop: &tokio::sync::watch::Receiver<bool>) -> Result<()> {
    use retina::client::{PlayOptions, SessionOptions, SetupOptions, Transport};
    use retina::codec::CodecItem;

    let kind = h.kind;
    let url = url::Url::parse(&stream_url(cam, kind)).context("bad stream url")?;
    let creds = if !url.username().is_empty() {
        Some(retina::client::Credentials {
            username: percent_encoding::percent_decode_str(url.username()).decode_utf8_lossy().to_string(),
            password: percent_encoding::percent_decode_str(url.password().unwrap_or("")).decode_utf8_lossy().to_string(),
        })
    } else {
        None
    };
    let mut clean = url.clone();
    let _ = clean.set_username("");
    let _ = clean.set_password(None);

    let opts = SessionOptions::default()
        .creds(creds)
        .user_agent("zmng/0.1".to_string())
        .teardown(retina::client::TeardownPolicy::Auto);
    let mut session = retina::client::Session::describe(clean, opts).await.context("DESCRIBE")?;
    let video_i = session
        .streams()
        .iter()
        .position(|s| s.media() == "video" && matches!(s.encoding_name(), "h264" | "h265"))
        .ok_or_else(|| anyhow::anyhow!("no h264/h265 video stream in SDP"))?;
    session
        .setup(video_i, SetupOptions::default().transport(Transport::Tcp(Default::default())))
        .await
        .context("SETUP")?;
    // opt-in AAC audio on the main stream (substream recordings stay video-only)
    let mut audio_i = if cam.record_audio && kind == StreamKind::Main {
        session.streams().iter().position(|s| s.media() == "audio" && s.encoding_name() == "aac")
    } else {
        None
    };
    if let Some(ai) = audio_i {
        if let Err(e) = session.setup(ai, SetupOptions::default().transport(Transport::Tcp(Default::default()))).await {
            warn!(camera = cam.id, "audio SETUP failed, recording video only: {e}");
            audio_i = None;
        }
    } else if cam.record_audio && kind == StreamKind::Main {
        warn!(camera = cam.id, "record_audio is on but the camera offers no AAC track");
    }
    let playing = session
        .play(PlayOptions::default().enforce_timestamps_with_max_jump_secs(NonZeroU32::new(10).unwrap()))
        .await
        .context("PLAY")?;
    let mut demuxed = playing.demuxed().context("demux")?;
    info!(camera = cam.id, name = %cam.name, stream = kind.as_str(), "connected");
    {
        let mut st = h.status.write();
        st.connected = true;
        st.last_error = None;
    }

    if !crate::retention::storage_mounted(storage) {
        // a bare mount point would silently record onto the root filesystem
        anyhow::bail!("storage {} is not mounted (no {} marker at {})", storage.id, crate::retention::MARKER, storage.path);
    }
    let cam_root = match kind {
        StreamKind::Main => Path::new(&storage.path).join(cam.id.to_string()),
        StreamKind::Sub => Path::new(&storage.path).join(cam.id.to_string()).join("sub"),
    };
    std::fs::create_dir_all(&cam_root)?;

    let mut params: Option<Arc<VideoParams>> = None;
    let mut init: Bytes = Bytes::new();
    let mut sample_entry_id: i64 = 0;
    let mut seg: Option<OpenSegment> = None;
    let mut gop: Vec<Sample> = Vec::new();
    let mut agop: Vec<Sample> = Vec::new(); // audio frames of the current GOP (audio timescale)
    let mut audio_rate: u32 = 0;
    // the audio stream has its own RTP origin: anchor it on its own first packet's wall clock
    let mut audio_anchor: Option<(i64, i64)> = None;
    let mut pending: Option<Pending> = None;
    // wallclock anchoring: (rtp elapsed at anchor, dts at anchor)
    let mut anchor: Option<(i64, i64)> = None;
    let mut last_dts: i64 = 0;
    let mut drift_bad_since: Option<Instant> = None;
    let mut force_new_segment = false;
    // stats
    let mut stat_t = Instant::now();
    let mut stat_frames = 0u32;
    let mut stat_bytes = 0u64;
    let seg_len_dts = ctx.segment_secs as i64 * TIMESCALE as i64;

    let result: Result<()> = async {
    loop {
        if *stop.borrow() {
            return Ok(());
        }
        let item = tokio::time::timeout(Duration::from_secs(20), demuxed.next())
            .await
            .map_err(|_| anyhow::anyhow!("no packets for 20 s"))?;
        let item = match item {
            Some(Ok(i)) => i,
            Some(Err(e)) => return Err(anyhow::Error::new(e).context("stream error")),
            None => anyhow::bail!("stream ended"),
        };
        let frame = match item {
            CodecItem::VideoFrame(f) if f.stream_id() == video_i => f,
            CodecItem::AudioFrame(a) if Some(a.stream_id()) == audio_i => {
                // only once video is flowing; the audio clock is mapped from its own first packet
                if audio_rate > 0 && anchor.is_some() && (seg.is_some() || !gop.is_empty() || pending.is_some()) {
                    let ts = a.timestamp();
                    let elapsed_90k = (ts.elapsed() as i128 * TIMESCALE as i128 / ts.clock_rate().get() as i128) as i64;
                    let (a_rtp, a_wall) = *audio_anchor.get_or_insert_with(|| (elapsed_90k, dts_from_wall(a.ctx().received_wall().into())));
                    let dts90k = a_wall + (elapsed_90k - a_rtp);
                    let dts = (dts90k as i128 * audio_rate as i128 / TIMESCALE as i128) as i64;
                    agop.push(Sample { dts, duration: a.frame_length().get(), is_key: true, data: Bytes::copy_from_slice(a.data()) });
                }
                continue;
            }
            _ => continue,
        };
        let is_key = frame.is_random_access_point();
        if frame.loss() > 0 {
            warn!(camera = cam.id, lost = frame.loss(), "RTP packet loss");
        }

        // --- parameters (first frame or mid-stream change) -------------------
        if params.is_none() || frame.has_new_parameters() {
            let stream = &demuxed.streams()[video_i];
            let p = match stream.parameters() {
                Some(retina::codec::ParametersRef::Video(v)) => v.clone(),
                _ => anyhow::bail!("video parameters unavailable"),
            };
            let codec = Codec::parse(stream.encoding_name()).unwrap_or(Codec::H264);
            let (w, hgt) = p.pixel_dimensions();
            let entry = p.mp4_sample_entry().build().map_err(|e| anyhow::anyhow!("sample entry: {e}"))?;
            let audio: Option<AudioParams> = audio_i.and_then(|ai| match demuxed.streams()[ai].parameters() {
                Some(retina::codec::ParametersRef::Audio(a)) => match a.mp4_sample_entry().build() {
                    Ok(e) => Some(AudioParams {
                        sample_entry: Bytes::from(e),
                        sample_rate: a.clock_rate(),
                        channels: a.channels().get(),
                        rfc6381: a.rfc6381_codec().unwrap_or("mp4a.40.2").to_string(),
                    }),
                    Err(e) => {
                        warn!(camera = cam.id, "audio sample entry: {e}; recording video only");
                        None
                    }
                },
                _ => None,
            });
            audio_rate = audio.as_ref().map(|a| a.sample_rate).unwrap_or(0);
            let vp = Arc::new(VideoParams {
                codec,
                width: w,
                height: hgt,
                sample_entry: Bytes::from(entry),
                rfc6381: p.rfc6381_codec().to_string(),
                audio,
            });
            if params.as_ref().map(|old| **old != *vp).unwrap_or(true) {
                info!(camera = cam.id, stream = kind.as_str(), codec = %codec, width = w, height = hgt, rfc6381 = %vp.rfc6381, audio = vp.audio.as_ref().map(|a| a.rfc6381.as_str()).unwrap_or("none"), "video parameters");
                // a codec change forces a new segment: flush what we have under the OLD sample entry
                if seg.is_some() {
                    if let Some(prev) = pending.take() {
                        let dur = gop.last().map(|x| x.duration).unwrap_or(TIMESCALE / 18);
                        gop.push(Sample { dts: prev.dts, duration: dur, is_key: prev.is_key, data: prev.data });
                    }
                    flush_gop(&mut gop, &mut agop, &mut seg, h)?;
                    close_segment(ctx, h, cam, storage, sample_entry_id, seg.take()).await?;
                }
                init = mp4::init_segment(&vp);
                sample_entry_id = ctx.db.intern_sample_entry(codec, &vp.rfc6381, w, hgt, &vp.sample_entry, vp.audio.as_ref())?;
                *h.recent.write() = Some(Recent { params: vp.clone(), init: init.clone(), frags: VecDeque::new() });
                {
                    let mut st = h.status.write();
                    st.codec = Some(vp.rfc6381.clone());
                    st.width = w;
                    st.height = hgt;
                }
                params = Some(vp);
            }
        }

        // --- timestamps: map RTP clock to 90 kHz since epoch ------------------
        let ts = frame.timestamp();
        let elapsed_90k = (ts.elapsed() as i128 * TIMESCALE as i128 / ts.clock_rate().get() as i128) as i64;
        let wall_now = dts_from_wall(frame.start_ctx().received_wall().into());
        let mut dts = match anchor {
            None => {
                anchor = Some((elapsed_90k, wall_now));
                wall_now
            }
            Some((a_rtp, a_wall)) => a_wall + (elapsed_90k - a_rtp),
        };
        // drift check: re-anchor only at a keyframe when sustained > 1 s for 30 s
        let drift = wall_now - dts;
        if drift.abs() > TIMESCALE as i64 {
            drift_bad_since.get_or_insert_with(Instant::now);
        } else {
            drift_bad_since = None;
        }
        if is_key {
            if let Some(since) = drift_bad_since {
                if since.elapsed() > Duration::from_secs(30) {
                    warn!(camera = cam.id, drift_ms = drift / 90, "re-anchoring clock to wallclock");
                    anchor = Some((elapsed_90k, wall_now));
                    dts = wall_now;
                    drift_bad_since = None;
                    if dts <= last_dts {
                        // wallclock stepped backwards: start a new segment so the
                        // timeline can have a discontinuity instead of freezing
                        force_new_segment = true;
                        last_dts = dts - 1;
                    }
                }
            }
        }
        if dts <= last_dts {
            dts = last_dts + 1; // keep strictly monotonic within a segment
        }
        last_dts = dts;

        // --- finalize the previous sample now that we know its duration ------
        if let Some(prev) = pending.take() {
            let dur = (dts - prev.dts).clamp(1, TIMESCALE as i64 * 2) as u32;
            gop.push(Sample { dts: prev.dts, duration: dur, is_key: prev.is_key, data: prev.data });
        }

        if is_key {
            // The GOP that just ended becomes a fragment.
            if !gop.is_empty() {
                if seg.is_none() {
                    seg = Some(open_segment(&cam_root, storage, cam, kind, &init, gop[0].dts)?);
                }
                flush_gop(&mut gop, &mut agop, &mut seg, h)?;
                let must_close = seg.as_ref().map(|s| dts - s.start_dts >= seg_len_dts).unwrap_or(false) || force_new_segment;
                if must_close {
                    close_segment(ctx, h, cam, storage, sample_entry_id, seg.take()).await?;
                    force_new_segment = false;
                }
            }
        } else if gop.is_empty() && seg.is_none() && pending.is_none() {
            debug!(camera = cam.id, "dropping non-key frame before first keyframe");
            continue;
        }

        let data = frame.into_data();
        stat_frames += 1;
        stat_bytes += data.len() as u64;
        pending = Some(Pending { dts, is_key, data: Bytes::from(data) });

        // --- stats -----------------------------------------------------------
        if stat_t.elapsed() >= Duration::from_secs(5) {
            let secs = stat_t.elapsed().as_secs_f32();
            let mut st = h.status.write();
            st.fps = stat_frames as f32 / secs;
            st.kbps = stat_bytes as f32 * 8.0 / 1000.0 / secs;
            st.last_frame_dts = dts;
            st.drift_ms = drift / 90;
            st.current_segment = seg.as_ref().map(|s| s.rel_path.clone());
            stat_t = Instant::now();
            stat_frames = 0;
            stat_bytes = 0;
        }
    }
    }
    .await;

    // Whatever ended the session, index what was recorded: the pending sample
    // (duration estimated from the previous one) and the open segment.
    if seg.is_some() || !gop.is_empty() {
        if let Some(prev) = pending.take() {
            let dur = gop.last().map(|x| x.duration).unwrap_or(TIMESCALE / 18);
            gop.push(Sample { dts: prev.dts, duration: dur, is_key: prev.is_key, data: prev.data });
        }
        if seg.is_none() && !gop.is_empty() {
            if let Ok(s) = open_segment(&cam_root, storage, cam, kind, &init, gop[0].dts) {
                seg = Some(s);
            }
        }
        if flush_gop(&mut gop, &mut agop, &mut seg, h).is_ok() {
            if let Err(e) = close_segment(ctx, h, cam, storage, sample_entry_id, seg.take()).await {
                warn!(camera = cam.id, "closing segment at session end: {e:#}");
            }
        }
    }
    h.status.write().current_segment = None;
    result
}

fn open_segment(cam_root: &Path, storage: &Storage, cam: &Camera, kind: StreamKind, init: &Bytes, start_dts: i64) -> Result<OpenSegment> {
    let prefix = match kind {
        StreamKind::Main => cam.id.to_string(),
        StreamKind::Sub => format!("{}/sub", cam.id),
    };
    let secs = start_dts / TIMESCALE as i64;
    let t = chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0).unwrap_or_default();
    let day = t.format("%Y%m%d").to_string();
    let name = format!("{}.mp4", t.format("%H%M%S"));
    let dir = cam_root.join(&day);
    std::fs::create_dir_all(&dir)?;
    let mut abs = dir.join(&name);
    let mut rel = format!("{}/{}/{}", prefix, day, name);
    let mut n = 0;
    let mut file = loop {
        match std::fs::OpenOptions::new().create_new(true).write(true).open(&abs) {
            Ok(f) => break f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 100 => {
                n += 1;
                let alt = format!("{}-{}.mp4", t.format("%H%M%S"), n);
                abs = dir.join(&alt);
                rel = format!("{}/{}/{}", prefix, day, alt);
            }
            Err(e) => return Err(e.into()),
        }
    };
    file.write_all(init)?;
    debug!(camera = cam.id, path = %rel, storage = storage.id, "opened segment");
    Ok(OpenSegment {
        file,
        rel_path: rel,
        abs_path: abs,
        start_dts,
        pos: init.len() as u64,
        init_len: init.len() as u32,
        frags: Vec::new(),
        seq: 0,
    })
}

/// Write the accumulated GOP as one fragment and publish it to live viewers.
/// A write failure (disk full, volume gone) is fatal for the session: the
/// fragment is not indexed and the caller tears the session down so the
/// supervisor reconnects with backoff instead of indexing garbage.
fn flush_gop(gop: &mut Vec<Sample>, agop: &mut Vec<Sample>, seg: &mut Option<OpenSegment>, h: &CamHandle) -> Result<()> {
    let Some(s) = seg.as_mut() else { gop.clear(); agop.clear(); return Ok(()) };
    if gop.is_empty() {
        return Ok(());
    }
    s.seq += 1;
    let frag = mp4::fragment_av(s.seq, gop, agop);
    agop.clear();
    let dts = gop[0].dts;
    let duration: u32 = gop.iter().map(|x| x.duration).sum();
    let samples = gop.len() as u32;
    if let Err(e) = s.file.write_all(&frag) {
        error!(path = %s.rel_path, "write failed: {e}");
        gop.clear();
        return Err(anyhow::anyhow!("write to {}: {e}", s.rel_path));
    }
    let entry = FragEntry {
        dts,
        duration,
        offset: s.pos,
        len: frag.len() as u32,
        samples,
        motion: h.motion.max_in(dts, dts + duration as i64),
    };
    s.pos += frag.len() as u64;
    s.frags.push(entry);
    gop.clear();
    *h.open.write() = Some(OpenSegmentInfo { abs_path: s.abs_path.clone(), init_len: s.init_len, frags: s.frags.clone() });

    // live fan-out
    let (params, init) = {
        let mut guard = h.recent.write();
        let Some(recent) = guard.as_mut() else { return Ok(()) };
        recent.frags.push_back((dts, duration, frag.clone()));
        while recent.frags.len() > 3 {
            recent.frags.pop_front();
        }
        (recent.params.clone(), recent.init.clone())
    };
    let _ = h.live.send(LiveFrag { params, init, dts, duration, data: frag });
    Ok(())
}

/// Close a segment: recompute per-fragment motion (the detector may have
/// delivered scores after the fragment was flushed), fsync and index it.
/// fsync + SQLite are blocking, so this runs on the blocking pool.
async fn close_segment(
    ctx: &Arc<RecorderCtx>,
    h: &Arc<CamHandle>,
    cam: &Camera,
    storage: &Storage,
    sample_entry_id: i64,
    seg: Option<OpenSegment>,
) -> Result<()> {
    let Some(mut s) = seg else { return Ok(()) };
    for f in s.frags.iter_mut() {
        f.motion = f.motion.max(h.motion.max_in(f.dts, f.dts + f.duration as i64));
    }
    // indexed rows win from here; the open-segment view is cleared once the row exists
    let open_after = if s.frags.is_empty() { None } else { Some(OpenSegmentInfo { abs_path: s.abs_path.clone(), init_len: s.init_len, frags: s.frags.clone() }) };
    let (ctx, cam, storage, kind) = (ctx.clone(), cam.clone(), storage.clone(), h.kind);
    tokio::task::spawn_blocking(move || -> Result<()> {
        if ctx.fsync {
            if let Err(e) = s.file.sync_data() {
                warn!(path = %s.rel_path, "fsync failed: {e}");
            }
        }
        drop(s.file);
        if s.frags.is_empty() {
            let _ = std::fs::remove_file(&s.abs_path);
            return Ok(());
        }
        let id = ctx.db.insert_segment_ext(cam.id, storage.id, sample_entry_id, &s.rel_path, s.pos as i64, s.init_len, &s.frags, 0, None, kind.as_str())?;
        let dur = s.frags.last().map(|f| f.dts + f.duration as i64).unwrap_or(s.start_dts) - s.start_dts;
        debug!(camera = cam.id, segment = id, path = %s.rel_path, secs = dur / TIMESCALE as i64, bytes = s.pos, frags = s.frags.len(), "closed segment");
        Ok(())
    })
    .await??;
    let mut o = h.open.write();
    if o.as_ref().map(|x| Some(&x.abs_path) == open_after.as_ref().map(|y| &y.abs_path)).unwrap_or(false) {
        *o = None;
    }
    Ok(())
}
