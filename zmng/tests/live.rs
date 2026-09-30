//! Live paths that need a recorder handle: snapshots come from the
//! substream, and event thumbnails from the segment still being written.
mod common;
use bytes::Bytes;
use common::*;
use std::sync::Arc;
use zmng::mp4::TIMESCALE;
use zmng::notify::Notification;
use zmng::recorder::{CamHandle, CamStatus, MotionLog, OpenSegmentInfo, Recent, StreamKind};

/// A running-looking recorder handle whose recent frames come from a
/// written segment file.
fn handle(fx: &Fixture, cam: i64, kind: StreamKind, enc: &Encoded, seg_id: i64) -> Arc<CamHandle> {
    let seg = fx.db.segment(seg_id).unwrap().unwrap();
    let path = fx.storage_path().join(&seg.path);
    let init = Bytes::from(zmng::thumbs::read_range(&path, 0, seg.init_len).unwrap());
    let frags = seg.index.iter().map(|f| (f.dts, f.duration, Bytes::from(zmng::thumbs::read_range(&path, f.offset, f.len).unwrap()))).collect();
    let (live, _) = tokio::sync::broadcast::channel(8);
    let (stop, _) = tokio::sync::watch::channel(false);
    Arc::new(CamHandle {
        kind,
        camera: parking_lot::RwLock::new(fx.db.camera(cam).unwrap().unwrap()),
        status: parking_lot::RwLock::new(CamStatus { connected: true, codec: Some(enc.params.rfc6381.clone()), fps: 10.0, ..Default::default() }),
        live,
        recent: parking_lot::RwLock::new(Some(Recent { params: Arc::new(enc.params.clone()), init, frags })),
        open: parking_lot::RwLock::new(None),
        motion: Arc::new(MotionLog::default()),
        stop,
    })
}

#[tokio::test]
async fn snapshot_decodes_the_substream_unless_main_is_asked_for() {
    let fx = Fixture::new();
    let cam = fx.add_camera("c");
    let main = parse_encoded(&encode_fmp4(2, "libx264", 640, 360));
    let sub = parse_encoded(&encode_fmp4(2, "libx264", 320, 180));
    let t = 1_000 * TIMESCALE as i64;
    let sm = fx.write_segment(cam, "main", &main, t, |_| 0);
    let ss = fx.write_segment(cam, "sub", &sub, t, |_| 0);
    fx.hub.cams.write().insert(cam, handle(&fx, cam, StreamKind::Main, &main, sm));
    fx.hub.subs.write().insert(cam, handle(&fx, cam, StreamKind::Sub, &sub, ss));
    let admin = fx.add_admin("admin", "password123");
    let tok = fx.token_for(admin);
    let r = fx.router();
    let dims = |jpeg: &[u8]| -> (u32, u32) {
        // SOF0 marker: height, width
        let i = jpeg.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
        (u16::from_be_bytes([jpeg[i + 7], jpeg[i + 8]]) as u32, u16::from_be_bytes([jpeg[i + 5], jpeg[i + 6]]) as u32)
    };
    // the wall's default request: substream, no main-stream decode
    let res = get(&r, &format!("/api/cameras/{cam}/snapshot.jpg"), &tok).await;
    assert_eq!(res.status, 200, "{}", res.text());
    assert_eq!(res.header("x-stream").as_deref(), Some("sub"));
    assert_eq!(dims(&res.body), (320, 180));
    // a width the substream cannot give: main
    let res = get(&r, &format!("/api/cameras/{cam}/snapshot.jpg?width=640"), &tok).await;
    assert_eq!(res.header("x-stream").as_deref(), Some("main"));
    assert_eq!(dims(&res.body), (640, 360));
    // a width the substream has, scaled down from it
    let res = get(&r, &format!("/api/cameras/{cam}/snapshot.jpg?width=160"), &tok).await;
    assert_eq!(res.header("x-stream").as_deref(), Some("sub"));
    assert_eq!(dims(&res.body).0, 160);
    // explicit main
    let res = get(&r, &format!("/api/cameras/{cam}/snapshot.jpg?stream=main&width=320"), &tok).await;
    assert_eq!(res.header("x-stream").as_deref(), Some("main"));
    // no substream recorder: main serves the default too
    fx.hub.subs.write().remove(&cam);
    let res = get(&r, &format!("/api/cameras/{cam}/snapshot.jpg"), &tok).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.header("x-stream").as_deref(), Some("main"));
}

/// The event's peak frame is in the segment still being written: the
/// thumbnail is read from that file at once instead of waiting for the
/// segment to close and be indexed.
#[tokio::test]
async fn thumbnail_comes_from_the_open_segment() {
    let fx = Fixture::new();
    let cam = fx.add_camera("c");
    let enc = parse_encoded(&encode_fmp4(3, "libx264", 320, 180));
    let t = 1_000 * TIMESCALE as i64;
    // write a segment file the way the recorder does, but do not index it
    let seg = fx.write_segment(cam, "main", &enc, t, |_| 0);
    let row = fx.db.segment(seg).unwrap().unwrap();
    let h = handle(&fx, cam, StreamKind::Main, &enc, seg);
    fx.db.delete_segment(seg).unwrap(); // the file stays; only the index row is gone
    assert!(fx.db.segments_in_range(cam, t, t + 1).unwrap().is_empty());
    *h.open.write() = Some(OpenSegmentInfo { abs_path: fx.storage_path().join(&row.path), init_len: row.init_len, frags: row.index.clone() });
    fx.hub.cams.write().insert(cam, h);
    let ev = fx.db.insert_event(cam, t, "motion").unwrap();
    let mut rx = fx.bus.subscribe();
    let started = std::time::Instant::now();
    zmng::detect::thumbnail_task(&fx.db, Some(&fx.hub), "ffmpeg", &fx.thumb_dir, &fx.bus, cam, ev, t + TIMESCALE as i64).await;
    assert!(started.elapsed().as_secs() < 5, "took {:?}: waited for the segment to close", started.elapsed());
    let e = fx.db.event(ev).unwrap().unwrap();
    let thumb = e.thumb_path.expect("thumbnail path set");
    assert!(fx.thumb_dir.join(&thumb).is_file());
    let mut kinds = Vec::new();
    while let Ok(n) = rx.try_recv() {
        kinds.push(match n { Notification::EventThumbnail { .. } => "thumb", Notification::EventUpdate(_) => "update", _ => "other" });
    }
    assert_eq!(kinds, ["thumb", "update"]);
    // without a recorder handle the old behaviour holds: nothing to read, a bounded wait, an error
    fx.hub.cams.write().clear();
    let ev2 = fx.db.insert_event(cam, t, "motion").unwrap();
    let r = tokio::time::timeout(std::time::Duration::from_secs(1), zmng::detect::make_thumbnail(&fx.db, Some(&fx.hub), "ffmpeg", &fx.thumb_dir, cam, ev2, t + TIMESCALE as i64)).await;
    assert!(r.is_err(), "still waiting (no segment) rather than returning something");
}

/// zmNinjaNg: `nph-zms?mode=single` never decodes the main stream for a
/// size the substream can give, and the go2rtc-compatible websocket serves
/// MSE (mime, init, fragments rebased to t=0) and declines WebRTC.
#[tokio::test]
async fn zmninja_single_frames_and_mse_websocket_use_the_substream() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let fx = Fixture::new();
    let cam = fx.add_camera("c");
    let main = parse_encoded(&encode_fmp4(2, "libx264", 640, 360));
    let sub = parse_encoded(&encode_fmp4(2, "libx264", 320, 180));
    let t = 1_000 * TIMESCALE as i64;
    let sm = fx.write_segment(cam, "main", &main, t, |_| 0);
    let ss = fx.write_segment(cam, "sub", &sub, t, |_| 0);
    fx.hub.cams.write().insert(cam, handle(&fx, cam, StreamKind::Main, &main, sm));
    let subh = handle(&fx, cam, StreamKind::Sub, &sub, ss);
    fx.hub.subs.write().insert(cam, subh.clone());
    let admin = fx.add_admin("admin", "password123");
    let tok = fx.token_for(admin);
    let r = fx.router();
    // scale=100 of a 640 px camera asks for 640; the 320 px substream answers
    let res = get(&r, &format!("/zm/cgi-bin/nph-zms?monitor={cam}&mode=single&scale=100&token={tok}"), &tok).await;
    assert_eq!(res.status, 200, "{}", res.text());
    let i = res.body.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
    assert_eq!(u16::from_be_bytes([res.body[i + 7], res.body[i + 8]]), 320);
    // the advertised go2rtc path is this server, as the client reached it
    let mon = get(&r, "/zm/api/monitors.json", &tok).await.json();
    assert_eq!(mon["monitors"][0]["Monitor"]["Go2RTCEnabled"], true);
    let req = axum::http::Request::builder().uri("/zm/api/configs/viewByName/ZM_GO2RTC_PATH.json").header("host", "nvr.local:8080").header("authorization", format!("Bearer {tok}")).body(axum::body::Body::empty()).unwrap();
    let res = tower::ServiceExt::oneshot(r.clone(), req).await.unwrap();
    let body = http_body_util::BodyExt::collect(res.into_body()).await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["config"]["Value"], "http://nvr.local:8080/zm/go2rtc");

    // the websocket needs a real listener
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = r.clone();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let url = format!("ws://127.0.0.1:{port}/zm/go2rtc/ws?src={cam}_CameraDirectSecondary&token={tok}");
    // no token: refused before the upgrade
    assert!(tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/zm/go2rtc/ws?src={cam}")).await.is_err());
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let text = |m: Message| -> serde_json::Value { serde_json::from_str(m.to_text().unwrap()).unwrap() };
    // WebRTC is declined (the player then keeps MSE)
    ws.send(Message::text(r#"{"type":"webrtc/offer","value":"v=0"}"#)).await.unwrap();
    let m = text(ws.next().await.unwrap().unwrap());
    assert_eq!(m["type"], "error");
    assert!(m["value"].as_str().unwrap().starts_with("webrtc/offer"));
    // a client that only plays HEVC gets nothing (both streams are H.264 here)
    ws.send(Message::text(r#"{"type":"mse","value":"hvc1.1.6.L153.B0"}"#)).await.unwrap();
    assert_eq!(text(ws.next().await.unwrap().unwrap())["type"], "error");
    // an H.264 client gets the substream: mime, init, newest fragment at t=0
    ws.send(Message::text(r#"{"type":"mse","value":"avc1.640029,avc1.64002A,hvc1.1.6.L153.B0,mp4a.40.2"}"#)).await.unwrap();
    let m = text(ws.next().await.unwrap().unwrap());
    assert_eq!(m["type"], "mse");
    assert_eq!(m["value"], sub.params.mime());
    let init = ws.next().await.unwrap().unwrap().into_data();
    assert_eq!(&init[4..8], b"ftyp");
    let tfdt = |d: &[u8]| -> u64 { let i = d.windows(4).position(|w| w == b"tfdt").unwrap(); assert_eq!(d[i + 4], 1); u64::from_be_bytes(d[i + 8..i + 16].try_into().unwrap()) };
    let first = ws.next().await.unwrap().unwrap().into_data();
    assert_eq!(&first[4..8], b"moof");
    assert_eq!(tfdt(&first), 0, "the session starts at t=0");
    // a live fragment follows, on the same rebased clock
    let (last_dts, last_dur, last_data, params) = {
        let rr = subh.recent.read();
        let rr = rr.as_ref().unwrap();
        let f = rr.frags.back().unwrap().clone();
        (f.0, f.1, f.2, rr.params.clone())
    };
    let init_bytes = subh.recent.read().as_ref().unwrap().init.clone();
    let mut next = last_data.to_vec();
    zmng::mp4::shift_tfdt(&mut next, last_dur as i64);
    subh.live.send(zmng::recorder::LiveFrag { params, init: init_bytes, dts: last_dts + last_dur as i64, duration: last_dur, data: Bytes::from(next) }).ok().expect("a subscriber");
    let live = ws.next().await.unwrap().unwrap().into_data();
    assert_eq!(tfdt(&live), last_dur as u64);
    ws.close(None).await.unwrap();
}
