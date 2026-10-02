//! Zone tuning: dry runs of other detection settings over recorded video,
//! and the live analysis stream.
mod common;
use common::*;
use serde_json::json;
use std::sync::Arc;
use zmng::mp4::TIMESCALE;

const T0: i64 = 1_800_000_000;
fn dts(secs: i64) -> i64 {
    secs * TIMESCALE as i64
}
fn ms(secs: i64) -> i64 {
    secs * 1000
}

/// 12 s of grey with a box that moves through the left half from 4 s to 8 s.
fn moving_box_clip() -> Encoded {
    let tmp = tempfile::NamedTempFile::with_suffix(".mp4").unwrap();
    let graph = "color=c=0x667788:s=320x180:r=10[bg];color=c=white:s=40x60:r=10[b];[bg][b]overlay=x='if(between(t\\,4\\,8)\\,10+25*(t-4)\\,-100)':y=60:shortest=1";
    let st = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", graph, "-t", "12", "-c:v", "libx264", "-preset", "ultrafast", "-g", "10", "-bf", "0", "-pix_fmt", "yuv420p",
               "-movflags", "frag_keyframe+empty_moov+default_base_moof", "-f", "mp4"])
        .arg(tmp.path())
        .status()
        .unwrap();
    assert!(st.success());
    parse_encoded(&std::fs::read(tmp.path()).unwrap())
}

async fn wait_done(r: &axum::Router, tok: &str, id: &str) -> serde_json::Value {
    for _ in 0..300 {
        let v = get(r, &format!("/api/dryrun/{id}"), tok).await.json();
        if v["state"] != "running" {
            return v;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("dry run did not finish");
}

#[tokio::test]
async fn dry_run_replays_recorded_video_with_other_zones() {
    let fx = Fixture::new();
    let cam = fx.add_camera("yard");
    let clip = moving_box_clip();
    fx.write_segment(cam, "sub", &clip, dts(T0), |_| 0);
    let admin = fx.token_for(fx.add_admin("admin", "password123"));
    let viewer = fx.token_for(fx.add_viewer("guest", "password123", &[cam]));
    let r = fx.router();
    let left = json!([{"name": "Left", "points": [[0, 0], [0.5, 0], [0.5, 1], [0, 1]], "min_area_pct": 0.5, "min_blob_pct": 0.2}]).to_string();
    let res = call(&r, "POST", &format!("/api/cameras/{cam}/dryrun"), Some(&admin), Some(json!({"start": ms(T0), "end": ms(T0 + 12), "zones_json": left}))).await;
    assert_eq!(res.status, 200, "{}", res.text());
    let job = res.json();
    assert_eq!(job["stream"], "sub");
    assert_eq!(job["recorded_events"], 0);
    let done = wait_done(&r, &admin, job["id"].as_str().unwrap()).await;
    assert_eq!(done["state"], "done", "{done}");
    // the first frame of every 1/detect_fps seconds of the 10 fps clip
    let fps = fx.db.camera(cam).unwrap().unwrap().detect_fps;
    assert_eq!(done["frames"].as_u64().unwrap(), (12.0 * fps.min(10.0)) as u64, "{done}");
    let evs = done["events"].as_array().unwrap();
    assert_eq!(evs.len(), 1, "{done}");
    assert_eq!(evs[0]["zones"], json!(["Left"]));
    let start = evs[0]["start_ms"].as_i64().unwrap();
    // motion from 4 s, opened on the second scored frame, minus 5 s pre-roll
    assert!((ms(T0 - 2)..=ms(T0)).contains(&start), "start {start}");
    assert!(evs[0]["end_ms"].as_i64().unwrap() > ms(T0 + 8));
    // a zone the box never enters: no event
    let far = json!([{"name": "Far", "points": [[0.85, 0], [1, 0], [1, 1], [0.85, 1]]}]).to_string();
    let job = call(&r, "POST", &format!("/api/cameras/{cam}/dryrun"), Some(&admin), Some(json!({"start": ms(T0), "end": ms(T0 + 12), "zones_json": far}))).await.json();
    let done = wait_done(&r, &admin, job["id"].as_str().unwrap()).await;
    assert_eq!(done["state"], "done");
    assert!(done["events"].as_array().unwrap().is_empty(), "{done}");
    // the camera itself is unchanged
    assert_eq!(fx.db.camera(cam).unwrap().unwrap().zones_json, "[]");
    // refused: viewers, bad zones, too long, nothing recorded
    assert_eq!(call(&r, "POST", &format!("/api/cameras/{cam}/dryrun"), Some(&viewer), Some(json!({"start": ms(T0), "end": ms(T0 + 12)}))).await.status, 403);
    assert_eq!(call(&r, "POST", &format!("/api/cameras/{cam}/dryrun"), Some(&admin), Some(json!({"start": ms(T0), "end": ms(T0 + 12), "zones_json": "[[[0,0]]]"}))).await.status, 400);
    assert_eq!(call(&r, "POST", &format!("/api/cameras/{cam}/dryrun"), Some(&admin), Some(json!({"start": ms(T0), "end": ms(T0 + 13 * 3600)}))).await.status, 400);
    let job = call(&r, "POST", &format!("/api/cameras/{cam}/dryrun"), Some(&admin), Some(json!({"start": ms(T0 + 100), "end": ms(T0 + 200)}))).await.json();
    let done = wait_done(&r, &admin, job["id"].as_str().unwrap()).await;
    assert_eq!(done["state"], "failed");
    assert!(done["error"].as_str().unwrap().contains("nothing was recorded"), "{done}");
    assert_eq!(get(&r, "/api/dryrun/nope", &admin).await.status, 404);
    // the recording is gone from disk: failed, with the reason
    let seg = fx.db.segments_in_range_stream(cam, "sub", dts(T0), dts(T0 + 12)).unwrap();
    std::fs::remove_file(fx.storage_path().join(&seg[0].path)).unwrap();
    let job = call(&r, "POST", &format!("/api/cameras/{cam}/dryrun"), Some(&admin), Some(json!({"start": ms(T0), "end": ms(T0 + 12)}))).await.json();
    let done = wait_done(&r, &admin, job["id"].as_str().unwrap()).await;
    assert_eq!(done["state"], "failed", "{done}");
    assert!(done["error"].as_str().unwrap().contains("reading the recording"), "{done}");
}

/// A second run for the same camera cancels the first; cancelling is final
/// (the run's end does not overwrite it).
#[tokio::test]
async fn a_new_dry_run_cancels_the_cameras_previous_one() {
    let fx = Fixture::new();
    let cam = fx.add_camera("yard");
    let clip = moving_box_clip();
    for k in 0..6 {
        fx.write_segment(cam, "sub", &clip, dts(T0 + 12 * k), |_| 0);
    }
    let admin = fx.token_for(fx.add_admin("admin", "password123"));
    let r = fx.router();
    let start = |r: axum::Router, admin: String| async move { call(&r, "POST", &format!("/api/cameras/{cam}/dryrun"), Some(&admin), Some(json!({"start": ms(T0), "end": ms(T0 + 72)}))).await.json() };
    let a = start(r.clone(), admin.clone()).await;
    let b = start(r.clone(), admin.clone()).await;
    assert_eq!(get(&r, &format!("/api/dryrun/{}", a["id"].as_str().unwrap()), &admin).await.json()["state"], "cancelled");
    assert_eq!(wait_done(&r, &admin, b["id"].as_str().unwrap()).await["state"], "done");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(get(&r, &format!("/api/dryrun/{}", a["id"].as_str().unwrap()), &admin).await.json()["state"], "cancelled");
    // cancel by hand
    let c = start(r.clone(), admin.clone()).await;
    let id = c["id"].as_str().unwrap();
    assert_eq!(call(&r, "DELETE", &format!("/api/dryrun/{id}"), Some(&admin), None).await.status, 200);
    assert_eq!(wait_done(&r, &admin, id).await["state"], "cancelled");
}

#[tokio::test]
async fn analysis_stream_sends_what_the_detector_sees() {
    use http_body_util::BodyExt;
    let fx = Fixture::new();
    let cam = fx.add_camera("yard");
    let admin = fx.token_for(fx.add_admin("admin", "password123"));
    let viewer = fx.token_for(fx.add_viewer("guest", "password123", &[cam]));
    let r = fx.router();
    assert_eq!(get(&r, &format!("/api/cameras/{cam}/analysis"), &admin).await.status, 404, "no detector running");
    let h = Arc::new(zmng::detect::DetectorHandle::new(fx.db.camera(cam).unwrap().unwrap()));
    fx.hub.detectors.write().insert(cam, h.clone());
    let req = axum::http::Request::builder().uri(format!("/api/cameras/{cam}/analysis")).header("authorization", format!("Bearer {viewer}")).body(axum::body::Body::empty()).unwrap();
    assert_eq!(tower::ServiceExt::oneshot(r.clone(), req).await.unwrap().status(), 403);
    let req = axum::http::Request::builder().uri(format!("/api/cameras/{cam}/analysis")).header("authorization", format!("Bearer {admin}")).body(axum::body::Body::empty()).unwrap();
    let res = tower::ServiceExt::oneshot(r.clone(), req).await.unwrap();
    assert_eq!(res.status(), 200);
    assert!(res.headers()["content-type"].to_str().unwrap().starts_with("text/event-stream"));
    assert_eq!(h.analysis.receiver_count(), 1, "subscribed while someone listens");
    // feed a detector a moving block and publish what it saw
    let mut det = zmng::detect::MotionDetector::new(160, 90, 25, 0.5, 0.2, &[], &[]);
    let base = vec![100u8; 160 * 90];
    for _ in 0..12 { det.feed(&base); }
    let mut f = base.clone();
    for y in 20..50 { for x in 40..70 { f[y * 160 + x] = 250; } }
    let score = det.feed(&f);
    h.analysis.send(Arc::new(det.analysis(1_800_000_000_000, score))).unwrap();
    let mut body = res.into_body();
    let mut text = String::new();
    while !text.contains("\n\n") {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), body.frame()).await.unwrap().unwrap().unwrap();
        if let Ok(d) = frame.into_data() {
            text.push_str(&String::from_utf8_lossy(&d));
        }
    }
    assert!(text.starts_with("event: analysis"), "{text}");
    let data: serde_json::Value = serde_json::from_str(text.lines().find(|l| l.starts_with("data:")).unwrap().trim_start_matches("data:").trim()).unwrap();
    assert_eq!(data["score"], score as i64);
    assert_eq!(data["bw"], 20);
    assert!(data["changed"].as_str().unwrap().chars().any(|c| c != '0'));
    assert_eq!(data["zones"][0]["fired"], true);
    assert!(data["zones"][0]["changed_pct"].as_f64().unwrap() > 5.0);
}
