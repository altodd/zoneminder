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
use crate::mp4::{self, Codec, FragEntry, Sample, VideoParams, TIMESCALE};

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

/// Shared handles for one camera.
pub struct CamHandle {
    pub camera: RwLock<Camera>,
    pub status: RwLock<CamStatus>,
    pub live: broadcast::Sender<LiveFrag>,
    /// Most recent init segment + last few fragments so a new live viewer can
    /// start immediately from a keyframe.
    pub recent: RwLock<Option<Recent>>,
    pub motion: MotionLog,
    pub stop: tokio::sync::watch::Sender<bool>,
}

pub struct Recent {
    pub params: Arc<VideoParams>,
    pub init: Bytes,
    pub frags: VecDeque<(i64, u32, Bytes)>,
}

pub struct LiveHub {
    pub cams: RwLock<HashMap<i64, Arc<CamHandle>>>,
}

impl LiveHub {
    pub fn new() -> Self {
        LiveHub { cams: RwLock::new(HashMap::new()) }
    }
    pub fn get(&self, id: i64) -> Option<Arc<CamHandle>> {
        self.cams.read().get(&id).cloned()
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

/// Spawn (or respawn) recorder tasks for every enabled camera.  Safe to call
/// repeatedly: cameras already running are left alone, removed/disabled ones
/// are stopped, changed URLs cause a restart.
pub async fn reconcile(ctx: Arc<RecorderCtx>) -> Result<()> {
    let cams = ctx.db.cameras()?;
    let mut hub = ctx.hub.cams.write();
    // stop removed or disabled
    let keep: Vec<i64> = cams.iter().filter(|c| c.enabled).map(|c| c.id).collect();
    let to_stop: Vec<i64> = hub.keys().filter(|id| !keep.contains(id)).copied().collect();
    for id in to_stop {
        if let Some(h) = hub.remove(&id) {
            let _ = h.stop.send(true);
            info!(camera = id, "stopping recorder");
        }
    }
    for cam in cams.into_iter().filter(|c| c.enabled) {
        if let Some(h) = hub.get(&cam.id) {
            let changed = {
                let cur = h.camera.read();
                cur.main_url != cam.main_url || cur.storage_id != cam.storage_id
            };
            if changed {
                let _ = h.stop.send(true);
                hub.remove(&cam.id);
                info!(camera = cam.id, "restarting recorder (config changed)");
            } else {
                *h.camera.write() = cam.clone();
                continue;
            }
        }
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let (live_tx, _) = broadcast::channel(64);
        let handle = Arc::new(CamHandle {
            camera: RwLock::new(cam.clone()),
            status: RwLock::new(CamStatus::default()),
            live: live_tx,
            recent: RwLock::new(None),
            motion: MotionLog::default(),
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
    Ok(())
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

    let url = url::Url::parse(&cam.main_url).context("bad main_url")?;
    let creds = if !url.username().is_empty() {
        Some(retina::client::Credentials {
            username: url.username().to_string(),
            password: url.password().unwrap_or("").to_string(),
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
    let playing = session
        .play(PlayOptions::default().enforce_timestamps_with_max_jump_secs(NonZeroU32::new(10).unwrap()))
        .await
        .context("PLAY")?;
    let mut demuxed = playing.demuxed().context("demux")?;
    info!(camera = cam.id, name = %cam.name, "connected");
    {
        let mut st = h.status.write();
        st.connected = true;
        st.last_error = None;
    }

    let cam_root = Path::new(&storage.path).join(cam.id.to_string());
    std::fs::create_dir_all(&cam_root)?;

    let mut params: Option<Arc<VideoParams>> = None;
    let mut init: Bytes = Bytes::new();
    let mut sample_entry_id: i64 = 0;
    let mut seg: Option<OpenSegment> = None;
    let mut gop: Vec<Sample> = Vec::new();
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
            let vp = Arc::new(VideoParams {
                codec,
                width: w,
                height: hgt,
                sample_entry: Bytes::from(entry),
                rfc6381: p.rfc6381_codec().to_string(),
            });
            if params.as_ref().map(|old| **old != *vp).unwrap_or(true) {
                info!(camera = cam.id, codec = %codec, width = w, height = hgt, rfc6381 = %vp.rfc6381, "video parameters");
                // a codec change forces a new segment: flush what we have under the OLD sample entry
                if seg.is_some() {
                    if let Some(prev) = pending.take() {
                        let dur = gop.last().map(|x| x.duration).unwrap_or(TIMESCALE / 18);
                        gop.push(Sample { dts: prev.dts, duration: dur, is_key: prev.is_key, data: prev.data });
                    }
                    flush_gop(&mut gop, &mut seg, h)?;
                    close_segment(ctx, h, cam, storage, sample_entry_id, seg.take()).await?;
                }
                init = mp4::init_segment(&vp);
                sample_entry_id = ctx.db.intern_sample_entry(codec, &vp.rfc6381, w, hgt, &vp.sample_entry)?;
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
                    seg = Some(open_segment(&cam_root, storage, cam, &init, gop[0].dts)?);
                }
                flush_gop(&mut gop, &mut seg, h)?;
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
            if let Ok(s) = open_segment(&cam_root, storage, cam, &init, gop[0].dts) {
                seg = Some(s);
            }
        }
        if flush_gop(&mut gop, &mut seg, h).is_ok() {
            if let Err(e) = close_segment(ctx, h, cam, storage, sample_entry_id, seg.take()).await {
                warn!(camera = cam.id, "closing segment at session end: {e:#}");
            }
        }
    }
    h.status.write().current_segment = None;
    result
}

fn open_segment(cam_root: &Path, storage: &Storage, cam: &Camera, init: &Bytes, start_dts: i64) -> Result<OpenSegment> {
    let secs = start_dts / TIMESCALE as i64;
    let t = chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0).unwrap_or_default();
    let day = t.format("%Y%m%d").to_string();
    let name = format!("{}.mp4", t.format("%H%M%S"));
    let dir = cam_root.join(&day);
    std::fs::create_dir_all(&dir)?;
    let mut abs = dir.join(&name);
    let mut rel = format!("{}/{}/{}", cam.id, day, name);
    let mut n = 0;
    let mut file = loop {
        match std::fs::OpenOptions::new().create_new(true).write(true).open(&abs) {
            Ok(f) => break f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 100 => {
                n += 1;
                let alt = format!("{}-{}.mp4", t.format("%H%M%S"), n);
                abs = dir.join(&alt);
                rel = format!("{}/{}/{}", cam.id, day, alt);
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
fn flush_gop(gop: &mut Vec<Sample>, seg: &mut Option<OpenSegment>, h: &CamHandle) -> Result<()> {
    let Some(s) = seg.as_mut() else { gop.clear(); return Ok(()) };
    if gop.is_empty() {
        return Ok(());
    }
    s.seq += 1;
    let frag = mp4::fragment(s.seq, gop);
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
    let (ctx, cam, storage) = (ctx.clone(), cam.clone(), storage.clone());
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
        let id = ctx.db.insert_segment(cam.id, storage.id, sample_entry_id, &s.rel_path, s.pos as i64, s.init_len, &s.frags)?;
        let dur = s.frags.last().map(|f| f.dts + f.duration as i64).unwrap_or(s.start_dts) - s.start_dts;
        debug!(camera = cam.id, segment = id, path = %s.rel_path, secs = dur / TIMESCALE as i64, bytes = s.pos, frags = s.frags.len(), "closed segment");
        Ok(())
    })
    .await?
}
