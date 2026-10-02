//! What zmNinjaNg sees through the ZoneMinder-compatible API beyond the
//! basics in api.rs: the live alarm state, the zones, the detection picture,
//! and honest refusals for what zmng does not do (forced alarms, ZoneMinder
//! functions it has no equivalent for).
mod common;
use common::*;
use std::sync::Arc;
use zmng::mp4::TIMESCALE;

const T0: i64 = 1_800_000_000;
fn dts(secs: i64) -> i64 {
    secs * TIMESCALE as i64
}

struct World {
    fx: Fixture,
    front: i64,
    back: i64,
    admin: String,
    viewer: String,
}

fn world() -> World {
    let fx = Fixture::new();
    let front = fx.add_camera("front");
    let back = fx.add_camera("back");
    let a = fx.add_admin("admin", "password123");
    let v = fx.add_viewer("guest", "password123", &[front]);
    World { admin: fx.token_for(a), viewer: fx.token_for(v), fx, front, back }
}

/// Pretend the detector for `cam` is running and (maybe) inside an event.
fn set_detector(fx: &Fixture, cam: i64, in_event: Option<i64>) {
    let camera = fx.db.camera(cam).unwrap().unwrap();
    let h = zmng::detect::DetectorHandle::new(camera);
    *h.status.write() = zmng::detect::DetectStatus { running: true, fps: 5.0, in_event, ..Default::default() };
    fx.hub.detectors.write().insert(cam, Arc::new(h));
}

async fn form(r: &axum::Router, path: &str, body: &str) -> Resp {
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    let res = tower::ServiceExt::oneshot(r.clone(), req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = http_body_util::BodyExt::collect(res.into_body()).await.unwrap().to_bytes();
    Resp { status, headers, body }
}

fn jpeg(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_pixel(w, h, image::Rgb([100, 100, 100]));
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90).encode(img.as_raw(), w, h, image::ExtendedColorType::Rgb8).unwrap();
    out
}

/// The app's Live Activity page and alarm ring poll `command:status`: it must
/// say "alarm" (2) while the camera is in an event, "idle" (0) otherwise,
/// and only for cameras the user may see.
#[tokio::test]
async fn alarm_status_follows_the_detector_and_forcing_is_refused() {
    let w = world();
    let r = w.fx.router();
    let st = |id: i64, tok: &str| format!("/zm/api/monitors/alarm/id:{id}/command:status.json?token={tok}");
    set_detector(&w.fx, w.front, None);
    assert_eq!(get(&r, &st(w.front, &w.viewer), &w.viewer).await.json()["status"], "0");
    set_detector(&w.fx, w.front, Some(42));
    let v = get(&r, &st(w.front, &w.viewer), &w.viewer).await.json();
    assert_eq!(v["status"], "2");
    // a camera without a detector is idle, one the viewer cannot see is not found
    assert_eq!(get(&r, &st(w.back, &w.admin), &w.admin).await.json()["status"], "0");
    assert_eq!(get(&r, &st(w.back, &w.viewer), &w.viewer).await.status, 404);
    assert_eq!(call(&r, "GET", &format!("/zm/api/monitors/alarm/id:{}/command:status.json", w.front), None, None).await.status, 401);
    // forcing an alarm on/off is answered with ZoneMinder's error shape, so the app shows a failure
    for cmd in ["on", "off"] {
        let v = get(&r, &format!("/zm/api/monitors/alarm/id:{}/command:{cmd}.json?token={}", w.front, w.admin), &w.admin).await.json();
        assert_eq!(v["status"], "false", "{cmd}");
        assert!(v["error"].as_str().unwrap().contains("zmng"), "{v}");
    }
}

/// Monitor settings from the app: Enabled / Capturing / Function None|Mocord
/// map onto the camera's enabled flag; anything zmng cannot do is refused
/// with a message instead of a fake "Saved".
#[tokio::test]
async fn monitor_settings_apply_what_maps_and_refuse_the_rest() {
    let w = world();
    let r = w.fx.router();
    let url = |tok: &str| format!("/zm/api/monitors/{}.json?token={tok}", w.front);
    let enabled = || w.fx.db.camera(w.front).unwrap().unwrap().enabled;
    assert_eq!(form(&r, &url(&w.viewer), "Monitor%5BEnabled%5D=0").await.status, 403);
    assert!(enabled());
    let res = form(&r, &url(&w.admin), "Monitor%5BEnabled%5D=0").await;
    assert_eq!(res.status, 200, "{}", res.text());
    assert_eq!(res.json()["message"], "Saved");
    assert!(!enabled());
    assert_eq!(form(&r, &url(&w.admin), "Monitor%5BCapturing%5D=Always").await.status, 200);
    assert!(enabled());
    assert_eq!(form(&r, &url(&w.admin), "Monitor%5BFunction%5D=None").await.status, 200);
    assert!(!enabled());
    assert_eq!(form(&r, &url(&w.admin), "Monitor%5BFunction%5D=Mocord").await.status, 200);
    assert!(enabled());
    // values that are already true are fine (the app may send them along)
    assert_eq!(form(&r, &url(&w.admin), "Monitor%5BAnalysing%5D=Always&Monitor%5BRecording%5D=Always").await.status, 200);
    // and None for each part when the camera is switched off with it
    assert_eq!(form(&r, &url(&w.admin), "Monitor%5BCapturing%5D=None&Monitor%5BAnalysing%5D=None&Monitor%5BRecording%5D=None").await.status, 200);
    assert!(!enabled());
    assert_eq!(form(&r, &url(&w.admin), "Monitor%5BEnabled%5D=1").await.status, 200);
    assert!(enabled());
    // no equivalent: refused, nothing changed
    for body in ["Monitor%5BFunction%5D=Modect", "Monitor%5BRecording%5D=OnMotion", "Monitor%5BAnalysing%5D=None", "Monitor%5BMaxFPS%5D=5", "Monitor%5BEnabled%5D=0&Monitor%5BPath%5D=rtsp%3A%2F%2Fx"] {
        let res = form(&r, &url(&w.admin), body).await;
        assert_eq!(res.status, 400, "{body}: {}", res.text());
        assert!(res.text().contains("zmng"), "{}", res.text());
        assert!(enabled(), "{body} must not change anything");
    }
    // unknown camera
    assert_eq!(form(&r, &format!("/zm/api/monitors/999.json?token={}", w.admin), "Monitor%5BEnabled%5D=0").await.status, 404);
    // the Server page's Start/Stop/Restart: no run states here, and the app is told so
    let res = form(&r, &format!("/zm/api/states/change/restart.json?token={}", w.admin), "").await;
    assert_eq!(res.status, 501);
    assert!(res.text().contains("systemctl"), "{}", res.text());
}

/// Zones in ZoneMinder's shape (percent coordinates), masks as Inactive
/// zones, the whole frame as "All" when a camera has no zones; viewers only
/// get the zones of their cameras. Admins have the Control permission the
/// app needs to show the PTZ pad.
#[tokio::test]
async fn zones_and_permissions_in_zoneminder_shape() {
    let w = world();
    let r = w.fx.router();
    let patch = serde_json::json!({"zones_json": "[[[0.1,0.2],[0.5,0.2],[0.5,0.9]]]", "masks_json": "[[[0,0],[0.25,0],[0.25,0.125]]]"});
    w.fx.db.update_camera(w.front, patch.as_object().unwrap()).unwrap();
    let z = get(&r, &format!("/zm/api/zones.json?MonitorId={}", w.front), &w.viewer).await.json();
    let zones = z["zones"].as_array().unwrap();
    assert_eq!(zones.len(), 2, "{z}");
    assert_eq!(zones[0]["Zone"]["Type"], "Active");
    assert_eq!(zones[0]["Zone"]["MonitorId"], w.front.to_string());
    assert_eq!(zones[0]["Zone"]["Coords"], "10.00,20.00 50.00,20.00 50.00,90.00");
    assert_eq!(zones[0]["Zone"]["NumCoords"], "3");
    assert_eq!(zones[0]["Zone"]["Units"], "Percent");
    assert_eq!(zones[1]["Zone"]["Type"], "Inactive");
    assert_eq!(zones[1]["Zone"]["Coords"], "0.00,0.00 25.00,0.00 25.00,12.50");
    assert_ne!(zones[0]["Zone"]["Id"], zones[1]["Zone"]["Id"]);
    let all = get(&r, &format!("/zm/api/zones.json?MonitorId={}", w.back), &w.admin).await.json();
    assert_eq!(all["zones"][0]["Zone"]["Name"], "All");
    assert_eq!(all["zones"][0]["Zone"]["Coords"], "0.00,0.00 100.00,0.00 100.00,100.00 0.00,100.00");
    assert!(get(&r, &format!("/zm/api/zones.json?MonitorId={}", w.back), &w.viewer).await.json()["zones"].as_array().unwrap().is_empty());
    // no MonitorId: every visible camera's zones
    assert_eq!(get(&r, "/zm/api/zones.json", &w.viewer).await.json()["zones"].as_array().unwrap().len(), 2);
    assert_eq!(get(&r, "/zm/api/users.json", &w.admin).await.json()["users"][0]["User"]["Control"], "Edit");
    assert_eq!(get(&r, "/zm/api/users.json", &w.viewer).await.json()["users"][0]["User"]["Control"], "None");
}

/// `fid=objdetect` is the event picture with the detected boxes drawn on it;
/// an event without objects has none (404, so the app skips that frame).
#[tokio::test]
async fn objdetect_picture_draws_the_boxes() {
    let w = world();
    let r = w.fx.router();
    std::fs::create_dir_all(w.fx.thumb_dir.join(w.front.to_string())).unwrap();
    std::fs::write(w.fx.thumb_dir.join(format!("{}/thumb.jpg", w.front)), jpeg(640, 360)).unwrap();
    let rel = format!("{}/thumb.jpg", w.front);
    let with = w.fx.db.insert_event(w.front, dts(T0), "person").unwrap();
    w.fx.db.close_event(with, dts(T0 + 5), Some(&rel), r#"{"objects":[{"label":"person","confidence":0.9,"bbox":[0.2,0.2,0.6,0.9]}]}"#).unwrap();
    let without = w.fx.db.insert_event(w.front, dts(T0 + 10), "motion").unwrap();
    w.fx.db.close_event(without, dts(T0 + 15), Some(&rel), "{}").unwrap();
    let img = |eid: i64, fid: &str| format!("/zm/index.php?view=image&eid={eid}&fid={fid}&width=640&token={}", w.viewer);
    let plain = get(&r, &img(with, "snapshot"), &w.viewer).await;
    assert_eq!(plain.status, 200);
    let boxed = get(&r, &img(with, "objdetect"), &w.viewer).await;
    assert_eq!(boxed.status, 200);
    assert_eq!(boxed.header("content-type").unwrap(), "image/jpeg");
    assert_ne!(boxed.body, plain.body);
    let pic = image::load_from_memory(&boxed.body).unwrap().to_rgb8();
    let edge = pic.get_pixel(256, 73); // top edge of the box (0.2 * 360 = 72)
    assert!(edge[1] > 160 && edge[0] < 120, "{edge:?}");
    assert_eq!(get(&r, &img(without, "objdetect"), &w.viewer).await.status, 404);
    // snapshot still works for the event without objects
    assert_eq!(get(&r, &img(without, "snapshot"), &w.viewer).await.status, 200);
}
