//! Object detection against a mock DeepStack-style server: the detector
//! client, the motion-gated flow inside the detector, event kinds and the
//! `require_object` policy, and the events API `kind` filter.
mod common;
use common::*;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use zmng::notify::Notification;
use zmng::objects::{ObjectDetector, ObjectsConfig};

/// Mock detection server. `reply` is what every request gets; requests are
/// counted and the last body checked for a multipart JPEG.
async fn mock_detector(reply: Value) -> (u16, Arc<Mutex<usize>>) {
    let calls = Arc::new(Mutex::new(0usize));
    let c2 = calls.clone();
    let app = axum::Router::new().route(
        "/v1/vision/detection",
        axum::routing::post(move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
            let c = c2.clone();
            let reply = reply.clone();
            async move {
                let ct = headers.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                assert!(ct.starts_with("multipart/form-data"), "{ct}");
                let s = String::from_utf8_lossy(&body);
                assert!(s.contains("name=\"image\""), "no image field");
                assert!(body.windows(2).any(|w| w == [0xFF, 0xD8]), "no JPEG in body");
                assert_eq!(headers.get("x-api-key").and_then(|v| v.to_str().ok()), Some("k1"));
                *c.lock().unwrap() += 1;
                axum::Json(reply)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (port, calls)
}

fn cfg(port: u16) -> ObjectsConfig {
    ObjectsConfig { url: format!("http://127.0.0.1:{port}/v1/vision/detection"), api_key: Some("k1".into()), interval_secs: 0.3, min_confidence: 0.5, ..Default::default() }
}

fn person_reply() -> Value {
    // a person whose feet are in the lower middle of a 320x180 frame, plus a low-confidence car and a bird
    json!({"success": true, "predictions": [
        {"label": "person", "confidence": 0.88, "x_min": 120, "y_min": 40, "x_max": 200, "y_max": 170},
        {"label": "car", "confidence": 0.2, "x_min": 0, "y_min": 0, "x_max": 50, "y_max": 50},
        {"label": "bird", "confidence": 0.99, "x_min": 0, "y_min": 0, "x_max": 50, "y_max": 50}
    ]})
}

#[tokio::test]
async fn detector_client_posts_multipart_and_normalizes() {
    let (port, calls) = mock_detector(person_reply()).await;
    let det = ObjectDetector::new(cfg(port)).unwrap();
    let jpeg = zmng::preview::encode_jpeg(&vec![10u8; 320 * 180 * 3], 320, 180, 80).unwrap();
    let objs = det.detect(jpeg, 320, 180).await.unwrap();
    assert_eq!(objs.len(), 3);
    assert_eq!(objs[0].label, "person");
    assert!((objs[0].bbox[0] - 0.375).abs() < 1e-9);
    assert_eq!(*calls.lock().unwrap(), 1);
    // an unreachable server is an error, not a panic
    let dead = ObjectDetector::new(ObjectsConfig { url: "http://127.0.0.1:1/x".into(), timeout_ms: 300, ..Default::default() }).unwrap();
    assert!(dead.detect(vec![0xFF, 0xD8], 1, 1).await.is_err());
    // a server-side failure is reported
    let (port2, _) = mock_detector(json!({"success": false, "error": "no model"})).await;
    let bad = ObjectDetector::new(cfg(port2)).unwrap();
    let e = bad.detect(vec![0xFF, 0xD8], 1, 1).await.unwrap_err().to_string();
    assert!(e.contains("no model"), "{e}");
}

fn ctx(fx: &Fixture, det: Option<Arc<ObjectDetector>>) -> Arc<zmng::detect::DetectCtx> {
    Arc::new(zmng::detect::DetectCtx { db: fx.db.clone(), hub: fx.hub.clone(), ffmpeg: "ffmpeg".into(), thumb_dir: fx.thumb_dir.clone(), bus: fx.bus.clone(), preview_secs: 0, objects: det })
}

/// A moving synthetic source triggers motion, the mock detector sees a
/// person, the event's kind and objects follow, and the API filters by kind.
#[tokio::test]
async fn motion_gated_detection_labels_the_event() {
    let (port, calls) = mock_detector(person_reply()).await;
    let fx = Fixture::new();
    let cam = fx.db.add_camera("synthetic", "rtsp://none/", Some("lavfi:testsrc=size=320x180:rate=10"), fx.storage_id).unwrap();
    fx.db.update_camera(cam, json!({"record_sub": false, "detect_fps": 10, "object_labels": "person, car"}).as_object().unwrap()).unwrap();
    let admin = fx.add_admin("admin", "password123");
    let tok = fx.token_for(admin);
    let ctx = ctx(&fx, Some(Arc::new(ObjectDetector::new(cfg(port)).unwrap())));
    let mut rx = fx.bus.subscribe();
    zmng::detect::reconcile(ctx.clone()).await.unwrap();
    let (mut started, mut updated) = (None, None);
    while updated.is_none() {
        let n = tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv()).await.expect("notifications").unwrap();
        match n {
            Notification::EventStart(e) => started = Some(e),
            Notification::EventUpdate(e) => updated = Some(e),
            _ => {}
        }
    }
    let (started, updated) = (started.unwrap(), updated.unwrap());
    assert_eq!(started.kind, "motion", "motion event announced first");
    assert_eq!(updated.id, started.id);
    assert_eq!(updated.kind, "person");
    assert_eq!(updated.objects.len(), 1, "car below confidence and bird not wanted: {:?}", updated.objects);
    assert_eq!(updated.objects[0].label, "person");
    assert!(*calls.lock().unwrap() >= 1);
    let e = fx.db.event(started.id).unwrap().unwrap();
    assert_eq!(e.kind, "person");
    let meta: Value = serde_json::from_str(&e.meta_json).unwrap();
    assert_eq!(meta["objects"][0]["label"], "person");
    // the detector is rate limited: a couple of seconds of continuous motion => a few calls, not 20+
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let n = *calls.lock().unwrap();
    assert!((2..=12).contains(&n), "calls={n}");
    // disabling the camera ends the session, which closes the open event
    fx.db.update_camera(cam, json!({"enabled": false}).as_object().unwrap()).unwrap();
    zmng::detect::reconcile(ctx).await.unwrap();
    let mut closed = false;
    for _ in 0..50 {
        if fx.db.event(started.id).unwrap().unwrap().end_dts.is_some() { closed = true; break; }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(closed, "event not closed when the detector session ended");
    // API: kind filter and objects in the payload (the event is closed now)
    let r = fx.router();
    let people = get(&r, "/api/events?kind=person", &tok).await.json();
    assert_eq!(people.as_array().unwrap().len(), 1);
    assert_eq!(people[0]["objects"][0]["label"], "person");
    assert!(get(&r, "/api/events?kind=motion", &tok).await.json().as_array().unwrap().is_empty());
    assert_eq!(get(&r, "/api/events?kind=person,car", &tok).await.json().as_array().unwrap().len(), 1);
}

/// With `require_object`, an event whose detections never return an object
/// is discarded when it closes, and EventStart is never announced.
#[tokio::test]
async fn require_object_discards_eventless_motion() {
    let (port, calls) = mock_detector(json!({"success": true, "predictions": []})).await;
    let fx = Fixture::new();
    // a finite source: ffmpeg exits after 3 s, the session ends and the event closes
    let cam = fx.db.add_camera("synthetic", "rtsp://none/", Some("lavfi:testsrc=size=320x180:rate=10:duration=3"), fx.storage_id).unwrap();
    fx.db.update_camera(cam, json!({"record_sub": false, "detect_fps": 10, "require_object": true}).as_object().unwrap()).unwrap();
    let ctx = ctx(&fx, Some(Arc::new(ObjectDetector::new(cfg(port)).unwrap())));
    let mut rx = fx.bus.subscribe();
    zmng::detect::reconcile(ctx.clone()).await.unwrap();
    // the event opens after the warm-up and stays open until ffmpeg exits (~2 s)
    let mut id = None;
    for _ in 0..400 {
        if let Some(e) = zmng::detect::status(cam).and_then(|s| s.in_event) { id = Some(e); break; }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let id = id.expect("an event row was opened while motion ran");
    for _ in 0..100 {
        if fx.db.event(id).unwrap().is_none() { break; }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(fx.db.event(id).unwrap().is_none(), "event without objects must be discarded");
    assert!(*calls.lock().unwrap() >= 1, "the detector was asked");
    // nothing was announced for it
    let mut announced = false;
    while let Ok(n) = rx.try_recv() {
        if matches!(n, Notification::EventStart(_) | Notification::EventEnd(_)) { announced = true; }
    }
    assert!(!announced);
    fx.db.update_camera(cam, json!({"enabled": false}).as_object().unwrap()).unwrap();
    zmng::detect::reconcile(ctx).await.unwrap();
}

#[tokio::test]
async fn camera_object_fields_validate() {
    let fx = Fixture::new();
    let cam = fx.add_camera("c");
    let admin = fx.add_admin("admin", "password123");
    let tok = fx.token_for(admin);
    let r = fx.router();
    assert_eq!(call(&r, "PATCH", &format!("/api/cameras/{cam}"), Some(&tok), Some(json!({"object_labels": 5}))).await.status, 400);
    assert_eq!(call(&r, "PATCH", &format!("/api/cameras/{cam}"), Some(&tok), Some(json!({"require_object": "yes"}))).await.status, 400);
    assert_eq!(call(&r, "PATCH", &format!("/api/cameras/{cam}"), Some(&tok), Some(json!({"objects": false, "require_object": true, "object_labels": "person"}))).await.status, 200);
    let c = get(&r, &format!("/api/cameras/{cam}"), &tok).await.json();
    assert_eq!(c["objects"], false);
    assert_eq!(c["require_object"], true);
    assert_eq!(c["object_labels"], "person");
}
