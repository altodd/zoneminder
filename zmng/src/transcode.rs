//! On-demand H.264 transcode for clients that cannot decode the camera's
//! HEVC (phase 3).
//!
//! Firefox and Windows machines without an HEVC decoder can play the H.264
//! substream, but not the full-resolution recording. When `[transcode]` is
//! configured, `video.mp4?codec=h264` and `live.mp4?codec=h264` pipe the
//! fMP4 through ffmpeg (`h264_nvenc` on the GPU, or `libx264`) and stream the
//! result as an fMP4 with the same absolute 90 kHz timeline: ffmpeg always
//! restarts `tfdt` at zero, so every `moof` is re-stamped on the way out with
//! [`crate::mp4::shift_tfdt`]. The browser code is unchanged.
//!
//! Sessions are capped (`max_sessions`): a transcode costs an encoder slot,
//! and the P2200 has one NVENC engine.

use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, warn};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct TranscodeConfig {
    /// ffmpeg video encoder: `h264_nvenc`, `h264_vaapi`, `h264_qsv`, `libx264`.
    pub encoder: String,
    /// Encoder preset (`p4` for nvenc, `veryfast` for libx264).
    pub preset: String,
    /// Output height (width follows the aspect); 0 = source size.
    pub max_height: u32,
    pub bitrate_kbps: u32,
    /// Keyframe interval in seconds (= fragment length = live latency).
    pub gop_secs: f64,
    /// Concurrent transcode sessions.
    pub max_sessions: usize,
    /// Extra ffmpeg output arguments (e.g. `["-rc", "vbr"]`).
    pub extra_args: Vec<String>,
    /// Hardware decoder for the input (`cuda`, `vaapi`, `qsv`); decoded
    /// frames come back to system memory for the scale filter. Unset =
    /// software decode.
    pub hwaccel: Option<String>,
}

impl Default for TranscodeConfig {
    fn default() -> Self {
        TranscodeConfig {
            encoder: "libx264".into(),
            preset: "veryfast".into(),
            max_height: 1080,
            bitrate_kbps: 4000,
            gop_secs: 1.0,
            max_sessions: 3,
            extra_args: Vec::new(),
            hwaccel: None,
        }
    }
}

pub struct Transcoder {
    pub cfg: TranscodeConfig,
    pub ffmpeg: String,
    sem: Arc<tokio::sync::Semaphore>,
}

/// A running transcode: its init segment, MIME type and fragment stream.
pub struct Transcoded {
    pub init: Bytes,
    pub mime: String,
    pub frags: std::pin::Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send>>,
}

/// Parse the ffmpeg argument list for one session (pure, unit-tested).
pub fn ffmpeg_args(cfg: &TranscodeConfig, fps_hint: f64) -> Vec<String> {
    // no `-fflags nobuffer`: with it the mp4 demuxer drops every frame of a
    // short piped input (a 2 s range encodes to nothing); low_delay alone
    // keeps live latency at one fragment
    let mut a: Vec<String> = ["-nostdin", "-loglevel", "error", "-nostats", "-flags", "low_delay",
        "-probesize", "1000000", "-analyzeduration", "0"]
        .iter().map(|s| s.to_string()).collect();
    // decode on the GPU (NVDEC) when configured; an input option, so before -i
    if let Some(hw) = cfg.hwaccel.as_deref().map(str::trim).filter(|h| !h.is_empty()) {
        a.extend(["-hwaccel".into(), hw.to_string()]);
    }
    a.extend(["-f", "mp4", "-i", "pipe:0", "-an", "-sn", "-dn",
        // keep the camera's variable frame rate: without it ffmpeg pads an
        // 18 fps VFR source to 30 fps CFR (1829 frames out for 1098 in)
        "-fps_mode", "passthrough"].iter().map(|s| s.to_string()));
    if cfg.max_height > 0 {
        a.extend(["-vf".into(), format!("scale=-2:'min({},ih)'", cfg.max_height)]);
    }
    a.extend(["-c:v".into(), cfg.encoder.clone()]);
    if !cfg.preset.is_empty() {
        a.extend(["-preset".into(), cfg.preset.clone()]);
    }
    if cfg.encoder == "libx264" {
        a.extend(["-tune".into(), "zerolatency".into()]);
    }
    let gop = (cfg.gop_secs.max(0.2) * fps_hint.max(1.0)).round() as u32;
    a.extend([
        "-b:v".into(), format!("{}k", cfg.bitrate_kbps.max(100)),
        "-maxrate".into(), format!("{}k", cfg.bitrate_kbps.max(100) * 3 / 2),
        "-bufsize".into(), format!("{}k", cfg.bitrate_kbps.max(100)),
        "-g".into(), gop.max(1).to_string(), "-bf".into(), "0".into(),
        "-force_key_frames".into(), format!("expr:gte(t,n_forced*{})", cfg.gop_secs.max(0.2)),
        "-pix_fmt".into(), "yuv420p".into(),
    ]);
    a.extend(cfg.extra_args.iter().cloned());
    a.extend([
        "-f".into(), "mp4".into(),
        "-movflags".into(), "frag_keyframe+empty_moov+default_base_moof".into(),
        "-video_track_timescale".into(), crate::mp4::TIMESCALE.to_string(),
        "pipe:1".into(),
    ]);
    a
}

impl Transcoder {
    pub fn new(cfg: TranscodeConfig, ffmpeg: String) -> Transcoder {
        Transcoder { sem: Arc::new(tokio::sync::Semaphore::new(cfg.max_sessions.max(1))), cfg, ffmpeg }
    }

    /// Sessions currently running.
    pub fn busy(&self) -> usize {
        self.cfg.max_sessions.max(1) - self.sem.available_permits()
    }

    /// Start a transcode of `input` (an fMP4 byte stream with absolute
    /// timestamps whose first fragment starts at `first_dts`). Returns once
    /// the init segment is available or fails fast when no slot is free.
    pub async fn start(&self, input: impl Stream<Item = std::io::Result<Bytes>> + Send + 'static, first_dts: i64, fps_hint: f64) -> Result<Transcoded> {
        let permit = self.sem.clone().try_acquire_owned().map_err(|_| anyhow::anyhow!("all {} transcode sessions are in use", self.cfg.max_sessions))?;
        let mut child = crate::thumbs::ffmpeg_command(&self.ffmpeg)
            .args(ffmpeg_args(&self.cfg, fps_hint))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("spawning ffmpeg for transcode")?;
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        tokio::spawn(async move {
            let mut r = tokio::io::BufReader::new(stderr);
            let mut line = String::new();
            use tokio::io::AsyncBufReadExt;
            while let Ok(n) = r.read_line(&mut line).await {
                if n == 0 { break; }
                warn!(ffmpeg = %line.trim(), "transcode");
                line.clear();
            }
        });
        // feed ffmpeg; a closed stdin (ffmpeg gone) ends the feed quietly
        tokio::spawn(async move {
            let mut input = std::pin::pin!(input);
            while let Some(chunk) = input.next().await {
                match chunk {
                    Ok(b) => {
                        if stdin.write_all(&b).await.is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        debug!("transcode input ended: {e}");
                        break;
                    }
                }
            }
            let _ = stdin.shutdown().await;
        });
        let mut reader = BoxReader { io: stdout, buf: BytesMut::with_capacity(1 << 20) };
        // init: ftyp + moov
        let mut init = BytesMut::new();
        let mut mime = String::from("video/mp4");
        loop {
            let (typ, boxed) = tokio::time::timeout(std::time::Duration::from_secs(20), reader.next_box())
                .await
                .map_err(|_| anyhow::anyhow!("ffmpeg produced no init segment in 20 s"))??
                .ok_or_else(|| anyhow::anyhow!("ffmpeg ended before producing an init segment"))?;
            init.extend_from_slice(&boxed);
            if &typ == b"moov" {
                if let Some((fourcc, entry, _, _)) = crate::mp4::extract_sample_entry(&init) {
                    if let Some(codec) = crate::mp4::Codec::parse(&fourcc) {
                        if let Some(rfc) = crate::mp4::rfc6381_from_sample_entry(codec, &entry) {
                            mime = format!("video/mp4; codecs=\"{rfc}\"");
                        }
                    }
                }
                break;
            }
        }
        // fragments: re-stamp every moof to the absolute timeline
        let frags = futures::stream::unfold((reader, child, permit, first_dts), |(mut reader, child, permit, base)| async move {
            match reader.next_box().await {
                Ok(Some((typ, mut boxed))) => {
                    if &typ == b"moof" {
                        let mut v = boxed.to_vec();
                        if !crate::mp4::shift_tfdt(&mut v, base) {
                            return Some((Err(std::io::Error::other("transcoded moof without tfdt")), (reader, child, permit, base)));
                        }
                        boxed = Bytes::from(v);
                    }
                    Some((Ok(boxed), (reader, child, permit, base)))
                }
                Ok(None) => None,
                Err(e) => Some((Err(std::io::Error::other(e.to_string())), (reader, child, permit, base))),
            }
        });
        Ok(Transcoded { init: init.freeze(), mime, frags: Box::pin(frags) })
    }
}

/// Reads whole ISO BMFF boxes from a byte stream.
struct BoxReader<R> {
    io: R,
    buf: BytesMut,
}

impl<R: tokio::io::AsyncRead + Unpin> BoxReader<R> {
    async fn fill(&mut self, need: usize) -> Result<bool> {
        while self.buf.len() < need {
            let n = self.io.read_buf(&mut self.buf).await?;
            if n == 0 {
                return Ok(false);
            }
        }
        Ok(true)
    }
    /// Next top-level box as (type, bytes incl. header); None at EOF.
    async fn next_box(&mut self) -> Result<Option<([u8; 4], Bytes)>> {
        if !self.fill(8).await? {
            return Ok(None);
        }
        let size = u32::from_be_bytes(self.buf[0..4].try_into().unwrap()) as usize;
        let typ: [u8; 4] = self.buf[4..8].try_into().unwrap();
        if !(8..=64 << 20).contains(&size) {
            anyhow::bail!("bad box size {size}");
        }
        if !self.fill(size).await? {
            anyhow::bail!("truncated {} box", String::from_utf8_lossy(&typ));
        }
        Ok(Some((typ, self.buf.split_to(size).freeze())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_follow_config() {
        let a = ffmpeg_args(&TranscodeConfig::default(), 18.0);
        let s = a.join(" ");
        assert!(s.contains("-c:v libx264 -preset veryfast -tune zerolatency"));
        assert!(s.contains("-g 18 -bf 0"));
        assert!(s.contains("scale=-2:'min(1080,ih)'"));
        assert!(s.ends_with("-movflags frag_keyframe+empty_moov+default_base_moof -video_track_timescale 90000 pipe:1"));
        let nv = TranscodeConfig { encoder: "h264_nvenc".into(), preset: "p4".into(), max_height: 0, extra_args: vec!["-rc".into(), "vbr".into()], ..Default::default() };
        let s = ffmpeg_args(&nv, 20.0).join(" ");
        assert!(s.contains("-c:v h264_nvenc -preset p4 -b:v"));
        assert!(!s.contains("zerolatency") && !s.contains("scale="));
        assert!(s.contains("-rc vbr -f mp4"));
        assert!(!s.contains("-hwaccel"));
        let gpu = TranscodeConfig { hwaccel: Some("cuda".into()), ..Default::default() };
        let s = ffmpeg_args(&gpu, 20.0).join(" ");
        assert!(s.contains("-analyzeduration 0 -hwaccel cuda -f mp4 -i pipe:0"));
    }

    #[tokio::test]
    async fn box_reader_splits_boxes_and_detects_truncation() {
        let mut data = Vec::new();
        for (t, n) in [(b"ftyp", 4usize), (b"moov", 100), (b"moof", 30)] {
            data.extend_from_slice(&((8 + n) as u32).to_be_bytes());
            data.extend_from_slice(t);
            data.extend(std::iter::repeat_n(7u8, n));
        }
        data.extend_from_slice(&40u32.to_be_bytes());
        data.extend_from_slice(b"mdat");
        data.extend_from_slice(&[1, 2, 3]); // truncated
        let mut r = BoxReader { io: std::io::Cursor::new(data), buf: BytesMut::new() };
        let (t, b) = r.next_box().await.unwrap().unwrap();
        assert_eq!(&t, b"ftyp");
        assert_eq!(b.len(), 12);
        assert_eq!(&r.next_box().await.unwrap().unwrap().0, b"moov");
        assert_eq!(&r.next_box().await.unwrap().unwrap().0, b"moof");
        assert!(r.next_box().await.is_err());
    }
}
