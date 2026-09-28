//! Notification delivery: SSE, the zmNinja event-server websocket, MQTT (with
//! Home Assistant discovery) and the webhook, plus the detector end to end.
mod common;
use common::*;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::Arc;
use zmng::notify::{EventInfo, Notification};

async fn recv<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>) -> Value
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let m = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next()).await.expect("ws message").unwrap().unwrap();
    serde_json::from_str::<Value>(m.to_text().unwrap()).unwrap()
}

fn info(fx: &Fixture, cam: i64, id: i64) -> EventInfo {
    let c = fx.db.camera(cam).unwrap().unwrap();
    EventInfo { id, camera_id: cam, camera_name: c.name, start_ms: 1_800_000_000_000, end_ms: None, score: 120, kind: "motion".into(), thumb: None, objects: vec![] }
}

/// Serve the router on a random loopback port (needed for websockets/SSE).
async fn serve(fx: &Fixture) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let router = fx.router();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    port
}

#[tokio::test]
async fn sse_delivers_only_visible_cameras() {
    let fx = Fixture::new();
    let cam = fx.add_camera("front");
    let cam2 = fx.add_camera("back");
    let viewer = fx.add_viewer("v", "password123", &[cam]);
    let tok = fx.token_for(viewer);
    let port = serve(&fx).await;
    let client = reqwest::Client::new();
    let res = client.get(format!("http://127.0.0.1:{port}/api/events/stream")).bearer_auth(&tok).send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert!(res.headers()["content-type"].to_str().unwrap().starts_with("text/event-stream"));
    let mut body = res.bytes_stream();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    fx.bus.publish(Notification::EventStart(info(&fx, cam2, 1))); // invisible: filtered out
    fx.bus.publish(Notification::CameraDown { camera_id: cam, name: "front".into() });
    fx.bus.publish(Notification::EventStart(info(&fx, cam, 2)));
    let mut text = String::new();
    while !text.contains("event_start") {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), body.next()).await.expect("sse chunk").unwrap().unwrap();
        text.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert!(text.contains("event: camera_down\ndata: {\"event\":\"camera_down\""), "{text}");
    assert!(text.contains("\"id\":2"));
    assert!(!text.contains("\"id\":1"));
    assert_eq!(client.get(format!("http://127.0.0.1:{port}/api/events/stream")).send().await.unwrap().status(), 401);
}

#[tokio::test]
async fn event_server_websocket_speaks_the_zmninja_protocol() {
    let fx = Fixture::new();
    let cam = fx.add_camera("front");
    let cam2 = fx.add_camera("back");
    fx.add_viewer("v", "password123", &[cam]);
    let port = serve(&fx).await;
    let url = format!("ws://127.0.0.1:{port}/zm/ws");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    ws.send(json!({"event": "auth", "data": {"user": "v", "password": "password123", "appversion": "2.6.0"}}).to_string().into()).await.unwrap();
    let a = recv(&mut ws).await;
    assert_eq!(a["event"], "auth");
    assert_eq!(a["status"], "Success");
    ws.send(json!({"event": "control", "data": {"type": "version"}}).to_string().into()).await.unwrap();
    assert_eq!(recv(&mut ws).await["type"], "version");
    ws.send(json!({"event": "push", "data": {"type": "token", "token": "fcm-abc", "platform": "ios", "monlist": "1", "state": "enabled", "profile": "home"}}).to_string().into()).await.unwrap();
    assert_eq!(recv(&mut ws).await["status"], "Success");
    let tokens = fx.db.push_tokens(fx.db.user_by_name("v").unwrap().unwrap().id).unwrap();
    assert_eq!(tokens[0]["Token"], "fcm-abc");
    // filter: this camera at most every 60 s
    ws.send(json!({"event": "control", "data": {"type": "filter", "monlist": cam.to_string(), "intlist": "60"}}).to_string().into()).await.unwrap();
    assert_eq!(recv(&mut ws).await["type"], "filter");
    fx.bus.publish(Notification::EventStart(info(&fx, cam2, 5))); // not visible
    fx.bus.publish(Notification::EventStart(info(&fx, cam, 6)));
    fx.bus.publish(Notification::EventStart(info(&fx, cam, 7))); // rate limited
    let alarm = recv(&mut ws).await;
    assert_eq!(alarm["event"], "alarm");
    assert_eq!(alarm["events"][0]["EventId"], 6);
    assert_eq!(alarm["events"][0]["MonitorName"], "front");
    assert_eq!(alarm["events"][0]["Cause"], "Motion");
    // nothing else arrives (id 7 is rate limited, id 5 invisible)
    assert!(tokio::time::timeout(std::time::Duration::from_millis(500), ws.next()).await.is_err());
    // bad credentials
    let (mut ws2, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    ws2.send(json!({"event": "auth", "data": {"user": "v", "password": "nope"}}).to_string().into()).await.unwrap();
    let a = recv(&mut ws2).await;
    assert_eq!(a["status"], "Fail");
    assert_eq!(a["reason"], "BADAUTH");
}

#[tokio::test]
async fn mqtt_sink_publishes_discovery_and_motion_state() {
    let broker = common::mqtt::FakeBroker::start().await;
    let fx = Fixture::new();
    let cam = fx.add_camera("front");
    let cfg = zmng::notify::MqttConfig { host: "127.0.0.1".into(), port: broker.port, ..Default::default() };
    tokio::spawn(zmng::notify::mqtt_sink(fx.bus.clone(), fx.db.clone(), cfg, fx.hub.clone()));
    let st = broker.wait_for("zmng/status", 5).await;
    assert_eq!(st.payload, b"online");
    assert!(st.retain);
    let disc = broker.wait_for(&format!("homeassistant/binary_sensor/zmng_{cam}_motion/config"), 5).await;
    let v: Value = serde_json::from_slice(&disc.payload).unwrap();
    assert_eq!(v["state_topic"], format!("zmng/camera/{cam}/motion"));
    assert_eq!(v["device"]["name"], "front");
    assert_eq!(broker.wait_for(&format!("zmng/camera/{cam}/recording"), 5).await.payload, b"OFF");
    fx.bus.publish(Notification::EventStart(info(&fx, cam, 1)));
    let m = broker.wait_for(&format!("zmng/camera/{cam}/event"), 5).await;
    assert_eq!(broker.last(&format!("zmng/camera/{cam}/motion")).unwrap().payload, b"ON");
    let v: Value = serde_json::from_slice(&m.payload).unwrap();
    assert_eq!(v["event"], "event_start");
    let mut done = info(&fx, cam, 1);
    done.end_ms = Some(1_800_000_009_000);
    fx.bus.publish(Notification::EventEnd(done));
    fx.bus.publish(Notification::EventThumbnail { event_id: 1, camera_id: cam, jpeg: bytes::Bytes::from_static(b"\xff\xd8jpeg") });
    let t = broker.wait_for(&format!("zmng/camera/{cam}/thumbnail"), 5).await;
    assert_eq!(t.payload, b"\xff\xd8jpeg");
    assert_eq!(broker.last(&format!("zmng/camera/{cam}/motion")).unwrap().payload, b"OFF");
    fx.bus.publish(Notification::CameraUp { camera_id: cam, name: "front".into() });
    assert_eq!(broker.wait_for(&format!("zmng/camera/{cam}/recording"), 5).await.payload, b"OFF");
    for _ in 0..50 {
        if broker.last(&format!("zmng/camera/{cam}/recording")).unwrap().payload == b"ON" { break; }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(broker.last(&format!("zmng/camera/{cam}/recording")).unwrap().payload, b"ON");
}

#[tokio::test]
async fn webhook_posts_json_for_external_notifications() {
    let received: Arc<std::sync::Mutex<Vec<Value>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let r2 = received.clone();
    let app = axum::Router::new().route("/hook", axum::routing::post(move |axum::Json(v): axum::Json<Value>| { let r = r2.clone(); async move { r.lock().unwrap().push(v); "ok" } }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let bus = Arc::new(zmng::notify::Bus::new());
    tokio::spawn(zmng::notify::webhook_sink(bus.clone(), format!("http://127.0.0.1:{port}/hook")));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    bus.publish(Notification::EventThumbnail { event_id: 1, camera_id: 1, jpeg: bytes::Bytes::new() }); // not external
    bus.publish(Notification::CameraDown { camera_id: 4, name: "Side".into() });
    bus.publish(Notification::StorageLow { storage_id: 1, path: "/vol".into(), free_bytes: 5, reserve_bytes: 10 });
    for _ in 0..100 {
        if received.lock().unwrap().len() >= 2 { break; }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let got = received.lock().unwrap().clone();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0], json!({"event": "camera_down", "camera_id": 4, "name": "Side"}));
    assert_eq!(got[1]["event"], "storage_low");
}

/// The whole detector path on a synthetic source: ffmpeg decodes lavfi
/// testsrc (a moving bar), the motion detector scores it, the state machine
/// opens an event and the bus announces it.
#[tokio::test]
async fn detector_end_to_end_on_synthetic_source() {
    let fx = Fixture::new();
    let cam = fx.db.add_camera("synthetic", "rtsp://none/", Some("lavfi:testsrc=size=320x180:rate=10"), fx.storage_id).unwrap();
    fx.db.update_camera(cam, json!({"record_sub": false, "detect_fps": 10, "min_area_pct": 0.2, "min_blob_pct": 0.1, "cooldown_secs": 1}).as_object().unwrap()).unwrap();
    let ctx = Arc::new(zmng::detect::DetectCtx { db: fx.db.clone(), hub: fx.hub.clone(), ffmpeg: "ffmpeg".into(), thumb_dir: fx.thumb_dir.clone(), bus: fx.bus.clone(), preview_secs: 1, objects: None });
    let mut rx = fx.bus.subscribe();
    zmng::detect::reconcile(ctx.clone()).await.unwrap();
    let start = loop {
        let n = tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv()).await.expect("event within 30 s").unwrap();
        if let Notification::EventStart(e) = n { break e; }
    };
    assert_eq!(start.camera_id, cam);
    assert_eq!(start.kind, "motion");
    assert!(start.score > 0);
    let e = fx.db.event(start.id).unwrap().unwrap();
    assert!(e.end_dts.is_none());
    let st = zmng::detect::status(cam).unwrap();
    assert!(st.running);
    assert_eq!(st.in_event, Some(start.id));
    // disabling the camera stops the detector
    fx.db.update_camera(cam, json!({"enabled": false}).as_object().unwrap()).unwrap();
    zmng::detect::reconcile(ctx).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(zmng::detect::status(cam).is_none());
}
