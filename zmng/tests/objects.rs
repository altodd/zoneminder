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
    mock_detector_delayed(reply, std::time::Duration::ZERO).await
}

/// [`mock_detector`] that answers after `delay` (a slow GPU box).
async fn mock_detector_delayed(reply: Value, delay: std::time::Duration) -> (u16, Arc<Mutex<usize>>) {
    let calls = Arc::new(Mutex::new(0usize));
    let c2 = calls.clone();
    let app = axum::Router::new().route(
        "/v1/vision/detection",
        axum::routing::post(move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
            let c = c2.clone();
            let reply = reply.clone();
            async move {
                tokio::time::sleep(delay).await;
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
        if let Some(e) = zmng::detect::status(&fx.hub, cam).and_then(|s| s.in_event) { id = Some(e); break; }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let id = id.expect("an event row was opened while motion ran");
    for _ in 0..100 {
        if fx.db.event(id).unwrap().is_none() { break; }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(fx.db.event(id).unwrap().is_none(), "event without objects must be discarded");
    for _ in 0..50 {
        if *calls.lock().unwrap() >= 1 { break; }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
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

/// A `require_object` event whose only detection answers after motion has
/// stopped and the cooldown passed: the close waits for the answer, the
/// person is attributed to the event, and it is kept and announced.
#[tokio::test]
async fn late_detection_keeps_a_require_object_event() {
    let (port, calls) = mock_detector_delayed(person_reply(), std::time::Duration::from_millis(2500)).await;
    let fx = Fixture::new();
    // 1.5 s of motion, then 3 s of a frozen frame (no motion), then the source ends
    let cam = fx.db.add_camera("synthetic", "rtsp://none/", Some("lavfi:testsrc=size=320x180:rate=10:duration=1.5,tpad=stop_mode=clone:stop_duration=3"), fx.storage_id).unwrap();
    fx.db.update_camera(cam, json!({"record_sub": false, "detect_fps": 10, "require_object": true, "cooldown_secs": 0.5, "post_secs": 0.5, "object_labels": "person"}).as_object().unwrap()).unwrap();
    let ctx = ctx(&fx, Some(Arc::new(ObjectDetector::new(ObjectsConfig { timeout_ms: 5000, ..cfg(port) }).unwrap())));
    let mut rx = fx.bus.subscribe();
    zmng::detect::reconcile(ctx.clone()).await.unwrap();
    let mut id = None;
    for _ in 0..400 {
        if let Some(e) = zmng::detect::status(&fx.hub, cam).and_then(|s| s.in_event) { id = Some(e); break; }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let id = id.expect("an event row was opened while motion ran");
    // the cooldown has passed long before the detector answers: the event must survive it
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert!(fx.db.event(id).unwrap().is_some(), "closed before the in-flight detection answered");
    let mut done = None;
    for _ in 0..120 {
        if let Some(e) = fx.db.event(id).unwrap() {
            if e.end_dts.is_some() { done = Some(e); break; }
        } else {
            panic!("event discarded although the late detection found a person");
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let e = done.expect("event closed after the answer arrived");
    assert_eq!(e.kind, "person");
    assert_eq!(*calls.lock().unwrap(), 1, "one detection during 1.5 s of motion");
    let mut kinds = Vec::new();
    while let Ok(n) = rx.try_recv() {
        match n {
            Notification::EventStart(e) => kinds.push(format!("start:{}", e.kind)),
            Notification::EventEnd(e) => kinds.push(format!("end:{}", e.kind)),
            _ => {}
        }
    }
    assert_eq!(kinds, ["start:person", "end:person"], "{kinds:?}");
    fx.db.update_camera(cam, json!({"enabled": false}).as_object().unwrap()).unwrap();
    zmng::detect::reconcile(ctx).await.unwrap();
}

/// Changing object settings restarts the detector session; a rename does not.
#[tokio::test]
async fn object_setting_changes_restart_the_detector() {
    let (port, _calls) = mock_detector(person_reply()).await;
    let fx = Fixture::new();
    let cam = fx.db.add_camera("synthetic", "rtsp://none/", Some("lavfi:testsrc=size=320x180:rate=5"), fx.storage_id).unwrap();
    fx.db.update_camera(cam, json!({"record_sub": false, "detect_fps": 5}).as_object().unwrap()).unwrap();
    let ctx = ctx(&fx, Some(Arc::new(ObjectDetector::new(cfg(port)).unwrap())));
    zmng::detect::reconcile(ctx.clone()).await.unwrap();
    let first = zmng::detect::status(&fx.hub, cam).expect("session").started_ms;
    fx.db.update_camera(cam, json!({"name": "renamed"}).as_object().unwrap()).unwrap();
    zmng::detect::reconcile(ctx.clone()).await.unwrap();
    assert_eq!(zmng::detect::status(&fx.hub, cam).unwrap().started_ms, first, "a rename must not restart");
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    fx.db.update_camera(cam, json!({"require_object": true}).as_object().unwrap()).unwrap();
    zmng::detect::reconcile(ctx.clone()).await.unwrap();
    let mut restarted = false;
    for _ in 0..100 {
        if zmng::detect::status(&fx.hub, cam).map(|s| s.started_ms != first).unwrap_or(false) { restarted = true; break; }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(restarted, "require_object change must restart the detector session");
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

/// A finished event is classified from its thumbnail: the labels are merged
/// into the event, its kind follows, and the events filter finds it by any
/// label it contains (not only the top one). The admin endpoint re-runs the
/// pass over past events in the background.
#[tokio::test]
async fn events_are_classified_from_their_thumbnail() {
    // a person and a car, both confident, feet/wheels inside the frame
    let (port, calls) = mock_detector(json!({"success": true, "predictions": [
        {"label": "person", "confidence": 0.91, "x_min": 100, "y_min": 60, "x_max": 180, "y_max": 300},
        {"label": "car", "confidence": 0.8, "x_min": 300, "y_min": 150, "x_max": 600, "y_max": 340}
    ]})).await;
    let fx = Fixture::new();
    let cam_id = fx.add_camera("door");
    let cam = fx.db.camera(cam_id).unwrap().unwrap();
    assert!(cam.objects, "cameras opt in to object detection by default");
    let jpeg = zmng::preview::encode_jpeg(&vec![90u8; 640 * 360 * 3], 640, 360, 80).unwrap();
    assert_eq!(zmng::objects::jpeg_size(&jpeg).unwrap(), (640, 360));
    let ev = fx.db.insert_event(cam_id, 1_000, "motion").unwrap();
    fx.db.close_event(ev, 2_000, None, r#"{"frames":10}"#).unwrap();
    let det = ObjectDetector::new(cfg(port)).unwrap();
    assert!(zmng::objects::classify_event(&det, &fx.db, &cam, ev, jpeg.clone(), &[]).await.unwrap());
    let e = fx.db.event(ev).unwrap().unwrap();
    assert_eq!(e.kind, "person", "person outranks car in the default label order");
    let meta: Value = serde_json::from_str(&e.meta_json).unwrap();
    assert_eq!(meta["frames"], 10, "other metadata is kept");
    assert_eq!(meta["classified"], true);
    assert_eq!(meta["objects"].as_array().unwrap().len(), 2);
    // the same answer again changes nothing
    assert!(!zmng::objects::classify_event(&det, &fx.db, &cam, ev, jpeg.clone(), &[]).await.unwrap());
    assert_eq!(*calls.lock().unwrap(), 2);

    // an event that knows where it moved: only the person (left) moved, the car (right) is parked
    let moved = fx.db.insert_event(cam_id, 5_000, "motion").unwrap();
    let mut bits = vec![false; 80 * 45];
    for y in 10..40 { for x in 15..25 { bits[y * 80 + x] = true; } } // x 0.19..0.31 of a 640 px frame
    let cells = zmng::detect::MotionCells { w: 640, h: 360, bw: 80, bh: 45, bits };
    fx.db.close_event(moved, 6_000, None, &json!({"motion_frames": 20, "peak_cells": cells.to_json()}).to_string()).unwrap();
    assert!(zmng::objects::classify_event(&det, &fx.db, &cam, moved, jpeg.clone(), &[]).await.unwrap());
    let labels: Vec<String> = zmng::notify::objects_from_meta(&fx.db.event(moved).unwrap().unwrap()).into_iter().map(|o| o.label).collect();
    assert_eq!(labels, vec!["person"], "the parked car did not move");
    // an old event without cells: a background box (seen in many events) is dropped
    let old = fx.db.insert_event(cam_id, 7_000, "car").unwrap();
    fx.db.close_event(old, 8_000, None, &json!({"objects": [{"label": "car", "confidence": 0.8, "bbox": [0.47, 0.42, 0.94, 0.94]}]}).to_string()).unwrap();
    let statics = vec![(cam_id, "car".to_string(), [0.47, 0.42, 0.94, 0.94])];
    assert!(zmng::objects::classify_event(&det, &fx.db, &cam, old, jpeg.clone(), &statics).await.unwrap());
    let e = fx.db.event(old).unwrap().unwrap();
    assert_eq!(e.kind, "person");
    assert!(zmng::notify::objects_from_meta(&e).iter().all(|o| o.label != "car"));
    assert!(det.failing(i64::MAX / 2, i64::MAX).is_none());

    let admin = fx.add_admin("admin", "password123");
    let tok = fx.token_for(admin);
    let r = fx.router();
    // kind "person", but the car inside it counts for a vehicles search
    assert_eq!(get(&r, "/api/events?kind=car,truck,bus", &tok).await.json().as_array().unwrap().len(), 1);
    assert_eq!(get(&r, "/api/events?kind=person", &tok).await.json().as_array().unwrap().len(), 3);
    // seconds of motion: 20 frames at the default 5 fps = 4 s; events without the counter always pass
    let all = get(&r, "/api/events", &tok).await.json();
    assert!(all.as_array().unwrap().iter().any(|e| e["motion_secs"] == 4.0));
    let long = get(&r, "/api/events?min_motion=3", &tok).await.json();
    let short_hidden = get(&r, "/api/events?min_motion=5", &tok).await.json();
    assert_eq!(long.as_array().unwrap().len(), short_hidden.as_array().unwrap().len() + 1);
    assert!(get(&r, "/api/events?kind=dog,cat", &tok).await.json().as_array().unwrap().is_empty());
    // a malformed meta_json must not break the filters for everyone else
    let bad = fx.db.insert_event(cam_id, 3_000, "motion").unwrap();
    fx.db.close_event(bad, 4_000, None, "not json").unwrap();
    assert_eq!(get(&r, "/api/events?kind=car", &tok).await.json().as_array().unwrap().len(), 1);
    assert!(get(&r, "/api/events?min_motion=1", &tok).await.status.is_success());
    // background boxes: the same parked bus in five events, people never
    let evs: Vec<zmng::db::Event> = (0..5).map(|i| {
        let id = fx.db.insert_event(cam_id, 10_000 + i, "bus").unwrap();
        fx.db.close_event(id, 10_500 + i, None, &json!({"objects": [{"label": "bus", "confidence": 0.9, "bbox": [0.78, 0.33, 0.98 - i as f64 * 0.002, 0.46]}, {"label": "person", "confidence": 0.9, "bbox": [0.2, 0.3, 0.3, 0.8]}]}).to_string()).unwrap();
        fx.db.event(id).unwrap().unwrap()
    }).collect();
    let st = zmng::objects::static_boxes(&evs, 5);
    assert_eq!(st.len(), 1);
    assert_eq!(st[0].1, "bus");
    assert!(zmng::objects::static_boxes(&evs[..4], 5).is_empty());

    // the re-classification endpoint needs a configured detector
    let res = call(&r, "POST", "/api/events/classify", Some(&tok), Some(json!({"days": 1}))).await;
    assert_eq!(res.status, 400, "{}", res.text());
    *fx.hub.objects.write() = Some(Arc::new(ObjectDetector::new(cfg(port)).unwrap()));
    assert_eq!(get(&r, "/api/me", &tok).await.json()["objects"], true);
    // events without a thumbnail are skipped; already classified ones too unless `again`
    let res = call(&r, "POST", "/api/events/classify", Some(&tok), Some(json!({"days": 36500, "again": true}))).await;
    assert_eq!(res.status, 202, "{}", res.text());
    assert_eq!(res.json()["queued"], 0);
}
