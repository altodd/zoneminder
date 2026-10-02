//! Shared test fixture: a temp database + storage with real segment files
//! written the way the recorder writes them (our fMP4 writer, absolute
//! timestamps), from video encoded by ffmpeg (`lavfi testsrc`). Every HTTP
//! test drives the real axum router in-process.
#![allow(dead_code)]
pub mod mqtt;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use bytes::Bytes;
use http_body_util::BodyExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower::ServiceExt;
use zmng::db::Db;
use zmng::mp4::{self, TIMESCALE};

pub const FPS: u32 = 10;
pub const GOP: u32 = 10;

pub struct Fixture {
    pub dir: tempfile::TempDir,
    pub db: Db,
    pub storage_id: i64,
    pub thumb_dir: PathBuf,
    pub cfg: zmng::config::Config,
    pub hub: Arc<zmng::recorder::LiveHub>,
    pub bus: Arc<zmng::notify::Bus>,
}

/// Encode `secs` seconds of synthetic video with ffmpeg as an fMP4 and return
/// the file bytes. `codec` is "libx264" or "libx265".
pub fn encode_fmp4(secs: u32, codec: &str, w: u32, h: u32) -> Vec<u8> {
    let tmp = tempfile::NamedTempFile::with_suffix(".mp4").unwrap();
    let st = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner", "-loglevel", "error", "-y",
            "-f", "lavfi", "-i", &format!("testsrc=size={w}x{h}:rate={FPS}"),
            "-t", &secs.to_string(), "-c:v", codec, "-preset", "ultrafast",
            "-g", &GOP.to_string(), "-bf", "0", "-pix_fmt", "yuv420p",
        ])
        .args(if codec == "libx265" { vec!["-tag:v", "hvc1", "-x265-params", "log-level=none"] } else { vec![] })
        .args(["-movflags", "frag_keyframe+empty_moov+default_base_moof", "-f", "mp4"])
        .arg(tmp.path())
        .status()
        .expect("ffmpeg present");
    assert!(st.success(), "ffmpeg failed");
    std::fs::read(tmp.path()).unwrap()
}

/// Parsed ffmpeg output: sample entry + samples (duration, key, payload).
pub struct Encoded {
    pub params: mp4::VideoParams,
    pub samples: Vec<(u32, bool, Bytes)>,
}

pub fn parse_encoded(file: &[u8]) -> Encoded {
    let scanned = mp4::scan_segment(file).unwrap();
    let (fourcc, mut entry, w, h) = mp4::extract_sample_entry(&file[..scanned.init_len as usize]).unwrap();
    let codec = mp4::Codec::parse(&fourcc).unwrap();
    mp4::prefer_hvc1(&mut entry);
    let rfc = mp4::rfc6381_from_sample_entry(codec, &entry).unwrap();
    let params = mp4::VideoParams { codec, width: w, height: h, sample_entry: Bytes::from(entry), rfc6381: rfc, audio: None };
    let mut samples = Vec::new();
    for f in &scanned.frags {
        let frag = &file[f.offset as usize..(f.offset + f.len as u64) as usize];
        for s in mp4::parse_fragment_samples(frag).unwrap() {
            samples.push((s.duration, s.is_key, Bytes::copy_from_slice(&frag[s.offset..s.offset + s.size as usize])));
        }
    }
    // ffmpeg's mp4 timescale for this clip is 10240 (rate 10 -> 1024/frame); renormalize to 90 kHz
    let per_frame = TIMESCALE / FPS;
    for s in samples.iter_mut() {
        s.0 = per_frame;
    }
    Encoded { params, samples }
}

impl Fixture {
    pub fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("zmng.db")).unwrap();
        let storage = dir.path().join("storage");
        std::fs::create_dir_all(&storage).unwrap();
        let storage_id = db.add_storage(storage.to_str().unwrap(), None, 0).unwrap();
        let thumb_dir = dir.path().join("thumbs");
        std::fs::create_dir_all(&thumb_dir).unwrap();
        let web_dir = dir.path().join("web");
        std::fs::create_dir_all(&web_dir).unwrap();
        std::fs::write(web_dir.join("index.html"), "<html>test</html>").unwrap();
        let cfg = zmng::config::Config {
            db_path: dir.path().join("zmng.db"),
            web_dir,
            thumb_dir: thumb_dir.clone(),
            ..Default::default()
        };
        Fixture { dir, db, storage_id, thumb_dir, cfg, hub: Arc::new(zmng::recorder::LiveHub::new()), bus: Arc::new(zmng::notify::Bus::new()) }
    }

    pub fn storage_path(&self) -> PathBuf {
        self.dir.path().join("storage")
    }

    pub fn add_camera(&self, name: &str) -> i64 {
        self.db.add_camera(name, &format!("rtsp://u:p@10.0.0.1/{name}"), Some(&format!("rtsp://u:p@10.0.0.1/{name}/sub")), self.storage_id).unwrap()
    }

    /// Write one segment file for `cam` starting at `start_dts` from encoded
    /// samples, exactly as the recorder does (init + one fragment per GOP),
    /// and index it. `motion(frag_index)` supplies per-fragment scores.
    pub fn write_segment(&self, cam: i64, stream: &str, enc: &Encoded, start_dts: i64, motion: impl Fn(usize) -> u8) -> i64 {
        self.write_segment_ext(cam, stream, enc, start_dts, motion, false)
    }

    /// A segment as imported from ZoneMinder: the file's own timeline starts
    /// at 0 and the index carries the offset to wall-clock time.
    pub fn write_legacy_segment(&self, cam: i64, stream: &str, enc: &Encoded, start_dts: i64) -> i64 {
        self.write_segment_ext(cam, stream, enc, start_dts, |_| 0, true)
    }

    fn write_segment_ext(&self, cam: i64, stream: &str, enc: &Encoded, start_dts: i64, motion: impl Fn(usize) -> u8, legacy: bool) -> i64 {
        let offset = if legacy { start_dts } else { 0 };
        let init = mp4::init_segment(&enc.params);
        let se = self.db.intern_sample_entry(enc.params.codec, &enc.params.rfc6381, enc.params.width, enc.params.height, &enc.params.sample_entry, enc.params.audio.as_ref()).unwrap();
        let secs = start_dts / TIMESCALE as i64;
        let t = chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0).unwrap();
        let prefix = if stream == "sub" { format!("{cam}/sub") } else { cam.to_string() };
        let rel = format!("{}/{}/{}.mp4", prefix, t.format("%Y%m%d"), t.format("%H%M%S"));
        let abs = self.storage_path().join(&rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        let mut file = init.to_vec();
        let mut frags = Vec::new();
        let mut gop: Vec<mp4::Sample> = Vec::new();
        let mut dts = start_dts;
        let mut seq = 0u32;
        let mut flush = |gop: &mut Vec<mp4::Sample>, file: &mut Vec<u8>, frags: &mut Vec<mp4::FragEntry>| {
            if gop.is_empty() { return; }
            seq += 1;
            let dts = gop[0].dts;
            for s in gop.iter_mut() {
                s.dts -= offset;
            }
            let frag = mp4::fragment(seq, gop);
            let dur: u32 = gop.iter().map(|s| s.duration).sum();
            frags.push(mp4::FragEntry { dts, duration: dur, offset: file.len() as u64, len: frag.len() as u32, samples: gop.len() as u32, motion: motion(frags.len()) });
            file.extend_from_slice(&frag);
            gop.clear();
        };
        for (dur, key, data) in &enc.samples {
            if *key {
                flush(&mut gop, &mut file, &mut frags);
            }
            gop.push(mp4::Sample { dts, duration: *dur, is_key: *key, data: data.clone() });
            dts += *dur as i64;
        }
        flush(&mut gop, &mut file, &mut frags);
        std::fs::write(&abs, &file).unwrap();
        self.db.insert_segment_ext(cam, self.storage_id, se, &rel, file.len() as i64, init.len() as u32, &frags, offset, None, stream).unwrap()
    }

    pub fn add_admin(&self, name: &str, pw: &str) -> i64 {
        self.db.add_user(name, &zmng::api::hash_password(pw).unwrap(), "admin").unwrap()
    }
    pub fn add_viewer(&self, name: &str, pw: &str, cams: &[i64]) -> i64 {
        let id = self.db.add_user(name, &zmng::api::hash_password(pw).unwrap(), "viewer").unwrap();
        self.db.set_user_cameras(id, cams).unwrap();
        id
    }
    /// A bearer token for `user_id` (bypasses the login endpoint).
    pub fn token_for(&self, user_id: i64) -> String {
        let t = zmng::api::new_token();
        self.db.create_session(user_id, &t, 3600).unwrap();
        t
    }

    pub fn app(&self) -> zmng::api::App {
        zmng::api::App::new(self.cfg.clone(), self.db.clone(), self.hub.clone(), self.bus.clone())
    }
    pub fn router(&self) -> Router {
        zmng::api::router(self.app())
    }
}

pub struct Resp {
    pub status: StatusCode,
    pub headers: axum::http::HeaderMap,
    pub body: Bytes,
}
impl Resp {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("not json ({e}): {}", String::from_utf8_lossy(&self.body)))
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
    pub fn header(&self, k: &str) -> Option<String> {
        self.headers.get(k).and_then(|v| v.to_str().ok()).map(|s| s.to_string())
    }
}

pub async fn call(router: &Router, method: &str, path: &str, token: Option<&str>, body: Option<serde_json::Value>) -> Resp {
    let mut req = Request::builder().method(method).uri(path);
    if let Some(t) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let req = match body {
        Some(b) => req.header(header::CONTENT_TYPE, "application/json").body(Body::from(b.to_string())).unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let res = router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    Resp { status, headers, body }
}

pub async fn get(router: &Router, path: &str, token: &str) -> Resp {
    call(router, "GET", path, Some(token), None).await
}

/// Decode an fMP4 byte stream with ffmpeg and return the number of frames.
pub fn count_frames(mp4: &[u8]) -> usize {
    use std::io::Write;
    let mut child = std::process::Command::new("ffprobe")
        .args(["-hide_banner", "-loglevel", "error", "-f", "mp4", "-i", "pipe:0", "-select_streams", "v:0", "-count_frames", "-show_entries", "stream=nb_read_frames", "-of", "csv=p=0"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(mp4).unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or_else(|_| panic!("ffprobe: {}", String::from_utf8_lossy(&out.stderr)))
}

pub fn ffprobe_json(path: &Path) -> serde_json::Value {
    let out = std::process::Command::new("ffprobe")
        .args(["-hide_banner", "-loglevel", "error", "-show_streams", "-show_format", "-of", "json"])
        .arg(path)
        .output()
        .unwrap();
    serde_json::from_slice(&out.stdout).unwrap()
}
