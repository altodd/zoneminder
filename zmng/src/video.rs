//! Serving recorded and live video.
//!
//! * `range_mp4` streams a virtual fMP4 for any [start, end) time range of a
//!   camera: init segment(s) + the fragments that overlap the range, read
//!   straight from the segment files.  Because `tfdt` values are absolute, no
//!   box is rewritten.  The browser plays it with MSE (or a plain `<video>`
//!   element for H.264).
//! * `hls_playlist` produces an HLS (fMP4) VOD playlist whose entries are
//!   byte ranges into the segment files, for Safari/iOS native playback and
//!   hls.js everywhere else.
//! * `live_mp4` streams init + fragments as they are closed (latency ≈ one
//!   GOP), for full-resolution live view with zero server-side decode.

use anyhow::Result;
use bytes::Bytes;
use futures::Stream;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::db::{Db, Segment};
use crate::mp4::{FragEntry, TIMESCALE};
use crate::recorder::{CamHandle, LiveFrag};

/// Which fragments of a segment overlap [start,end).
pub fn overlapping<'a>(seg: &'a Segment, start: i64, end: i64) -> impl Iterator<Item = &'a FragEntry> {
    seg.index.iter().filter(move |f| f.dts + f.duration as i64 > start && f.dts < end)
}

pub struct RangePlan {
    pub items: Vec<RangeItem>,
    /// total bytes (for Content-Length); only exact when `exact_len` is true
    pub bytes: u64,
    /// false when legacy fragments are rewritten while streaming
    pub exact_len: bool,
    /// added to every fragment's tfdt when served (0 = absolute time; set to
    /// -first_dts to produce a self-contained file whose timeline starts at 0)
    pub tfdt_shift: i64,
    pub first_dts: Option<i64>,
    pub last_end_dts: Option<i64>,
    pub mime: Option<String>,
}

pub enum RangeItem {
    Init(Bytes),
    Frag { path: PathBuf, offset: u64, len: u32, dts: i64, duration: u32, dts_offset: i64 },
}

/// Build the plan for a time range: which init segments and fragments to send.
pub fn plan_range(db: &Db, camera_id: i64, start: i64, end: i64) -> Result<RangePlan> {
    plan_range_stream(db, camera_id, "main", start, end)
}

pub fn plan_range_stream(db: &Db, camera_id: i64, stream: &str, start: i64, end: i64) -> Result<RangePlan> {
    let segs = db.segments_in_range_stream(camera_id, stream, start, end)?;
    let mut items = Vec::new();
    let mut bytes = 0u64;
    let mut cur_se: Option<i64> = None;
    let mut first = None;
    let mut last = None;
    let mut mime = None;
    let mut exact_len = true;
    for seg in &segs {
        if seg.dts_offset != 0 {
            exact_len = false;
        }
        let storage = match db.storage(seg.storage_id)? {
            Some(s) => s,
            None => continue,
        };
        let path = PathBuf::from(&storage.path).join(&seg.path);
        if cur_se != Some(seg.sample_entry_id) {
            if let Some(se) = db.sample_entry(seg.sample_entry_id)? {
                let vp = se.video_params();
                let init = crate::mp4::init_segment(&vp);
                bytes += init.len() as u64;
                if mime.is_none() {
                    mime = Some(vp.mime());
                }
                items.push(RangeItem::Init(init));
                cur_se = Some(seg.sample_entry_id);
            }
        }
        for f in overlapping(seg, start, end) {
            if first.is_none() {
                first = Some(f.dts);
            }
            last = Some(f.dts + f.duration as i64);
            bytes += f.len as u64;
            items.push(RangeItem::Frag { path: path.clone(), offset: f.offset, len: f.len, dts: f.dts, duration: f.duration, dts_offset: seg.dts_offset });
        }
    }
    Ok(RangePlan { items, bytes, exact_len, tfdt_shift: 0, first_dts: first, last_end_dts: last, mime })
}

/// Turn a plan into a byte stream, reading files on a blocking thread.
pub fn stream_plan(plan: RangePlan) -> impl Stream<Item = std::result::Result<Bytes, std::io::Error>> {
    let (tx, rx) = mpsc::channel::<std::result::Result<Bytes, std::io::Error>>(8);
    tokio::task::spawn_blocking(move || {
        let shift = plan.tfdt_shift;
        let mut open: Option<(PathBuf, std::fs::File)> = None;
        for item in plan.items {
            let res = match item {
                RangeItem::Init(b) => Ok(b),
                RangeItem::Frag { path, offset, len, dts_offset, .. } => {
                    let need_open = open.as_ref().map(|(p, _)| *p != path).unwrap_or(true);
                    if need_open {
                        match std::fs::File::open(&path) {
                            Ok(f) => open = Some((path.clone(), f)),
                            Err(e) => {
                                let _ = tx.blocking_send(Err(e));
                                return;
                            }
                        }
                    }
                    let f = &mut open.as_mut().unwrap().1;
                    let mut buf = vec![0u8; len as usize];
                    f.seek(SeekFrom::Start(offset))
                        .and_then(|_| f.read_exact(&mut buf))
                        .map(|_| Bytes::from(crate::mp4::rebase_fragment(&buf, dts_offset + shift)))
                }
            };
            let stop = res.is_err();
            if tx.blocking_send(res).is_err() || stop {
                return;
            }
        }
    });
    tokio_stream_from_rx(rx)
}

fn tokio_stream_from_rx<T>(mut rx: mpsc::Receiver<T>) -> impl Stream<Item = T> {
    futures::stream::poll_fn(move |cx| rx.poll_recv(cx))
}

/// HLS VOD playlist with byte ranges into the segment files.
/// `seg_url(seg_id)` returns the URL for the raw segment file endpoint.
pub fn hls_playlist(
    db: &Db,
    camera_id: i64,
    stream: &str,
    start: i64,
    end: i64,
    seg_url: impl Fn(i64) -> String,
    frag_url: impl Fn(i64, usize) -> String,
    init_url: impl Fn(i64) -> String,
) -> Result<String> {
    let segs = db.segments_in_range_stream(camera_id, stream, start, end)?;
    let mut out = String::from("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n");
    let mut target = 1.0f64;
    let mut body = String::new();
    let mut cur_se: Option<i64> = None;
    let mut prev_end: Option<i64> = None;
    for seg in &segs {
        let url = seg_url(seg.id);
        let legacy = seg.dts_offset != 0;
        let mut first_in_seg = true;
        for (fi, f) in seg.index.iter().enumerate().filter(|(_, f)| f.dts + f.duration as i64 > start && f.dts < end) {
            if let Some(pe) = prev_end {
                // gap larger than half a second => discontinuity
                if (f.dts - pe).abs() > TIMESCALE as i64 / 2 {
                    body.push_str("#EXT-X-DISCONTINUITY\n");
                }
            }
            if first_in_seg {
                if cur_se.is_some() && cur_se != Some(seg.sample_entry_id) && !body.ends_with("#EXT-X-DISCONTINUITY\n") {
                    body.push_str("#EXT-X-DISCONTINUITY\n");
                }
                if legacy {
                    body.push_str(&format!("#EXT-X-MAP:URI=\"{}\"\n", init_url(seg.id)));
                } else {
                    body.push_str(&format!("#EXT-X-MAP:URI=\"{}\",BYTERANGE=\"{}@0\"\n", url, seg.init_len));
                }
                cur_se = Some(seg.sample_entry_id);
            }
            first_in_seg = false;
            let secs = f.duration as f64 / TIMESCALE as f64;
            target = target.max(secs);
            let wall = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(f.dts / 90).unwrap_or_default();
            if legacy {
                body.push_str(&format!(
                    "#EXT-X-PROGRAM-DATE-TIME:{}\n#EXTINF:{:.3},\n{}\n",
                    wall.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    secs,
                    frag_url(seg.id, fi)
                ));
            } else {
                body.push_str(&format!(
                    "#EXT-X-PROGRAM-DATE-TIME:{}\n#EXTINF:{:.3},\n#EXT-X-BYTERANGE:{}@{}\n{}\n",
                    wall.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    secs,
                    f.len,
                    f.offset,
                    url
                ));
            }
            prev_end = Some(f.dts + f.duration as i64);
        }
    }
    out.push_str(&format!("#EXT-X-TARGETDURATION:{}\n#EXT-X-MEDIA-SEQUENCE:0\n", target.ceil() as u64));
    out.push_str(&body);
    out.push_str("#EXT-X-ENDLIST\n");
    Ok(out)
}

/// Live fMP4: init + a couple of recent fragments, then everything new.
pub fn live_mp4(h: Arc<CamHandle>) -> impl Stream<Item = std::result::Result<Bytes, std::io::Error>> {
    let (tx, rx) = mpsc::channel::<std::result::Result<Bytes, std::io::Error>>(16);
    let mut sub = h.live.subscribe();
    // prime with recent fragments so the player starts on a keyframe now
    let mut current_init: Option<Bytes> = None;
    if let Some(r) = h.recent.read().as_ref() {
        current_init = Some(r.init.clone());
        let _ = tx.try_send(Ok(r.init.clone()));
        // send only the last fragment to keep latency low
        if let Some((_, _, b)) = r.frags.back() {
            let _ = tx.try_send(Ok(b.clone()));
        }
    }
    tokio::spawn(async move {
        loop {
            match sub.recv().await {
                Ok(LiveFrag { init, data, .. }) => {
                    if current_init.as_ref() != Some(&init) {
                        if tx.send(Ok(init.clone())).await.is_err() {
                            return;
                        }
                        current_init = Some(init);
                    }
                    if tx.send(Ok(data)).await.is_err() {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            }
        }
    });
    tokio_stream_from_rx(rx)
}

/// Convert milliseconds since epoch to 90 kHz ticks.
pub fn ms_to_dts(ms: i64) -> i64 {
    ms * 90
}
pub fn dts_to_ms(dts: i64) -> i64 {
    dts / 90
}
