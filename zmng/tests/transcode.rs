//! On-demand H.264 transcode of HEVC ranges and live streams.
mod common;
use common::*;
use bytes::Bytes;
use std::sync::Arc;
use zmng::mp4::TIMESCALE;

const T0: i64 = 1_800_000_000;
fn dts(secs: i64) -> i64 { secs * TIMESCALE as i64 }
fn ms(secs: i64) -> i64 { secs * 1000 }

fn with_transcode(fx: &mut Fixture) {
    fx.cfg.transcode = Some(zmng::transcode::TranscodeConfig { encoder: "libx264".into(), preset: "ultrafast".into(), max_height: 0, max_sessions: 2, ..Default::default() });
}

/// First tfdt of an fMP4 buffer (init + fragments), in 90 kHz ticks.
fn first_tfdt(mp4: &[u8]) -> i64 {
    zmng::mp4::scan_segment(mp4).unwrap().frags[0].dts
}

#[tokio::test]
async fn hevc_range_is_transcoded_to_h264_with_absolute_timestamps() {
    let mut fx = Fixture::new();
    with_transcode(&mut fx);
    let cam = fx.add_camera("hevc");
    let enc = parse_encoded(&encode_fmp4(4, "libx265", 320, 180));
    fx.write_segment(cam, "main", &enc, dts(T0), |_| 0);
    let admin = fx.add_admin("admin", "password123");
    let tok = fx.token_for(admin);
    let r = fx.router();
    assert_eq!(get(&r, "/api/me", &tok).await.json()["transcode"], true);
    let res = get(&r, &format!("/api/cameras/{cam}/video.mp4?start={}&end={}&codec=h264", ms(T0 + 1), ms(T0 + 3)), &tok).await;
    assert_eq!(res.status, 200, "{}", res.text());
    let ct = res.header("content-type").unwrap();
    assert!(ct.starts_with("video/mp4; codecs=\"avc1."), "{ct}");
    assert_eq!(res.header("x-first-dts-ms").unwrap(), ms(T0 + 1).to_string());
    assert!(res.header("content-length").is_none(), "transcoded length is unknown up front");
    // decodes, has the right number of frames, and its timeline is the recording's
    assert_eq!(count_frames(&res.body), 20);
    let t = first_tfdt(&res.body);
    assert!((t - dts(T0 + 1)).abs() < TIMESCALE as i64 / 2, "tfdt {t} vs {}", dts(T0 + 1));
    // native request still serves HEVC
    let native = get(&r, &format!("/api/cameras/{cam}/video.mp4?start={}&end={}", ms(T0 + 1), ms(T0 + 3)), &tok).await;
    assert!(native.header("content-type").unwrap().starts_with("video/mp4; codecs=\"hvc1."));
    // unknown codec
    assert_eq!(get(&r, &format!("/api/cameras/{cam}/video.mp4?start={}&end={}&codec=vp9", ms(T0 + 1), ms(T0 + 3)), &tok).await.status, 400);
}

#[tokio::test]
async fn h264_source_passes_through_and_missing_config_is_501() {
    let fx = Fixture::new();
    let cam = fx.add_camera("avc");
    let enc = parse_encoded(&encode_fmp4(2, "libx264", 160, 90));
    fx.write_segment(cam, "main", &enc, dts(T0), |_| 0);
    let admin = fx.add_admin("admin", "password123");
    let tok = fx.token_for(admin);
    let r = fx.router();
    assert_eq!(get(&r, "/api/me", &tok).await.json()["transcode"], false);
    let a = get(&r, &format!("/api/cameras/{cam}/video.mp4?start={}&end={}", ms(T0), ms(T0 + 2)), &tok).await;
    let b = get(&r, &format!("/api/cameras/{cam}/video.mp4?start={}&end={}&codec=h264", ms(T0), ms(T0 + 2)), &tok).await;
    assert_eq!(b.status, 200);
    assert_eq!(a.body, b.body, "an H.264 source needs no transcode");
    // an HEVC camera without [transcode] configured
    let cam2 = fx.add_camera("hevc");
    let enc = parse_encoded(&encode_fmp4(2, "libx265", 160, 90));
    fx.write_segment(cam2, "main", &enc, dts(T0), |_| 0);
    let c = get(&r, &format!("/api/cameras/{cam2}/video.mp4?start={}&end={}&codec=h264", ms(T0), ms(T0 + 2)), &tok).await;
    assert_eq!(c.status, 501);
}

#[tokio::test]
async fn session_cap_returns_503_for_the_extra_viewer() {
    let mut fx = Fixture::new();
    with_transcode(&mut fx);
    fx.cfg.transcode.as_mut().unwrap().max_sessions = 1;
    let cam = fx.add_camera("hevc");
    let enc = parse_encoded(&encode_fmp4(6, "libx265", 320, 180));
    fx.write_segment(cam, "main", &enc, dts(T0), |_| 0);
    let admin = fx.add_admin("admin", "password123");
    let tok = fx.token_for(admin);
    // a real listener so the first response can stay open while the second is made
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let router = fx.router();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let url = format!("http://127.0.0.1:{port}/api/cameras/{cam}/video.mp4?start={}&end={}&codec=h264", ms(T0), ms(T0 + 6));
    let client = reqwest::Client::new();
    let first = client.get(&url).bearer_auth(&tok).send().await.unwrap();
    assert_eq!(first.status(), 200);
    let second = client.get(&url).bearer_auth(&tok).send().await.unwrap();
    assert_eq!(second.status(), 503);
    // the slot is released once the first stream is consumed
    let body = first.bytes().await.unwrap();
    assert!(count_frames(&body) >= 50);
    for _ in 0..50 {
        if client.get(&url).bearer_auth(&tok).send().await.unwrap().status() == 200 { return; }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("slot never released");
}

/// Live transcode from a hand-built recorder handle: init + fragments are
/// broadcast like the recorder does, and the client gets H.264 with the
/// live timeline.
#[tokio::test]
async fn live_stream_is_transcoded() {
    let mut fx = Fixture::new();
    with_transcode(&mut fx);
    let cam = fx.add_camera("hevc");
    let enc = parse_encoded(&encode_fmp4(6, "libx265", 320, 180));
    let seg = fx.write_segment(cam, "main", &enc, dts(T0), |_| 0);
    let seg = fx.db.segment(seg).unwrap().unwrap();
    let path = fx.storage_path().join(&seg.path);
    let init = Bytes::from(zmng::thumbs::read_range(&path, 0, seg.init_len).unwrap());
    let frags: Vec<(i64, u32, Bytes)> = seg.index.iter().map(|f| (f.dts, f.duration, Bytes::from(zmng::thumbs::read_range(&path, f.offset, f.len).unwrap()))).collect();
    let params = Arc::new(enc.params.clone());
    let (live_tx, _) = tokio::sync::broadcast::channel(64);
    let (stop_tx, _) = tokio::sync::watch::channel(false);
    let camera = fx.db.camera(cam).unwrap().unwrap();
    let handle = Arc::new(zmng::recorder::CamHandle {
        kind: zmng::recorder::StreamKind::Main,
        camera: parking_lot::RwLock::new(camera),
        status: parking_lot::RwLock::new(zmng::recorder::CamStatus { connected: true, codec: Some(enc.params.rfc6381.clone()), fps: 10.0, ..Default::default() }),
        live: live_tx.clone(),
        recent: parking_lot::RwLock::new(Some(zmng::recorder::Recent { params: params.clone(), init: init.clone(), frags: frags[..1].iter().cloned().collect() })),
        motion: Arc::new(zmng::recorder::MotionLog::default()),
        stop: stop_tx,
    });
    fx.hub.cams.write().insert(cam, handle);
    let admin = fx.add_admin("admin", "password123");
    let tok = fx.token_for(admin);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let router = fx.router();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    // publish the remaining fragments at their real pace
    let feeder = tokio::spawn(async move {
        for (dts, duration, data) in frags.into_iter().skip(1) {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            let _ = live_tx.send(zmng::recorder::LiveFrag { params: params.clone(), init: init.clone(), dts, duration, data });
        }
    });
    let res = reqwest::Client::new().get(format!("http://127.0.0.1:{port}/api/cameras/{cam}/live.mp4?codec=h264")).bearer_auth(&tok).send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert!(res.headers()["content-type"].to_str().unwrap().starts_with("video/mp4; codecs=\"avc1."));
    assert_eq!(res.headers()["x-first-dts-ms"].to_str().unwrap(), ms(T0).to_string());
    let mut body = Vec::new();
    let mut stream = res.bytes_stream();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline && count_moofs(&body) < 3 {
        match tokio::time::timeout_at(deadline, futures::StreamExt::next(&mut stream)).await {
            Ok(Some(Ok(chunk))) => body.extend_from_slice(&chunk),
            _ => break,
        }
    }
    feeder.abort();
    assert!(count_moofs(&body) >= 3, "got {} fragments", count_moofs(&body));
    assert!((first_tfdt(&body) - dts(T0)).abs() < TIMESCALE as i64 / 2);
    assert!(count_frames(&body) >= 20);
}

fn count_moofs(b: &[u8]) -> usize {
    zmng::mp4::scan_segment(b).map(|s| s.frags.len()).unwrap_or(0)
}
