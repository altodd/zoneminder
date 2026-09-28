//! Scrub previews: API, detector integration, retention.
mod common;
use common::*;
use serde_json::json;
use std::sync::Arc;
use zmng::mp4::TIMESCALE;
use zmng::preview::{encode_jpeg, hour_key, Store};

fn dts(secs: i64) -> i64 { secs * TIMESCALE as i64 }

#[tokio::test]
async fn preview_endpoints_serve_tiles_and_sprites_with_acl() {
    let fx = Fixture::new();
    let cam = fx.add_camera("front");
    let cam2 = fx.add_camera("back");
    let viewer = fx.add_viewer("v", "password123", &[cam]);
    let tok = fx.token_for(viewer);
    let store = Store::new(&fx.thumb_dir);
    let base = dts(1_700_000_000);
    let tile = encode_jpeg(&vec![90u8; 160 * 90 * 3], 160, 90, 70).unwrap();
    for i in 0..4 {
        store.append(cam, base + dts(i * 5), &tile).unwrap();
        store.append(cam2, base + dts(i * 5), &tile).unwrap();
    }
    let r = fx.router();
    let t_ms = (base + dts(6)) / 90;
    let res = get(&r, &format!("/api/cameras/{cam}/preview.jpg?t={t_ms}"), &tok).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.header("content-type").unwrap(), "image/jpeg");
    assert_eq!(res.body.to_vec(), tile);
    // too far from any tile
    assert_eq!(get(&r, &format!("/api/cameras/{cam}/preview.jpg?t={}", (base + dts(60)) / 90), &tok).await.status, 404);
    // invisible camera
    assert_eq!(get(&r, &format!("/api/cameras/{cam2}/preview.jpg?t={t_ms}"), &tok).await.status, 404);
    let hours = get(&r, &format!("/api/cameras/{cam}/previews"), &tok).await.json();
    assert_eq!(hours["hours"][0], hour_key(base));
    assert_eq!(hours["interval_secs"], 5);
    let key = hour_key(base);
    let m = get(&r, &format!("/api/cameras/{cam}/previews/{key}.json"), &tok).await;
    assert_eq!(m.status, 200);
    let m = m.json();
    assert_eq!(m["times"].as_array().unwrap().len(), 4);
    assert_eq!(m["tile_w"], 160);
    let s = get(&r, &format!("/api/cameras/{cam}/previews/{key}.jpg"), &tok).await;
    assert_eq!(s.status, 200);
    let img = image::load_from_memory(&s.body).unwrap();
    assert_eq!((img.width(), img.height()), (160 * 4, 90));
    assert_eq!(get(&r, &format!("/api/cameras/{cam}/previews/2020010100.jpg"), &tok).await.status, 404);
    assert!(matches!(get(&r, &format!("/api/cameras/{cam}/previews/..%2Fetc.jpg"), &tok).await.status.as_u16(), 400 | 404));
    assert_eq!(get(&r, &format!("/api/cameras/{cam}/previews/{key}.gif"), &tok).await.status, 400);
    assert_eq!(get(&r, &format!("/api/cameras/{cam2}/previews/{key}.jpg"), &tok).await.status, 404);
}

#[tokio::test]
async fn detector_writes_preview_tiles_from_its_frames() {
    let fx = Fixture::new();
    let cam = fx.db.add_camera("synthetic", "rtsp://none/", Some("lavfi:testsrc=size=320x180:rate=10"), fx.storage_id).unwrap();
    fx.db.update_camera(cam, json!({"record_sub": false, "detect_fps": 10}).as_object().unwrap()).unwrap();
    let ctx = Arc::new(zmng::detect::DetectCtx { db: fx.db.clone(), hub: fx.hub.clone(), ffmpeg: "ffmpeg".into(), thumb_dir: fx.thumb_dir.clone(), bus: fx.bus.clone(), preview_secs: 1, objects: None });
    zmng::detect::reconcile(ctx.clone()).await.unwrap();
    let store = Store::new(&fx.thumb_dir);
    let mut tiles = Vec::new();
    for _ in 0..80 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let hours = store.hours(cam);
        if let Some(h) = hours.last() {
            tiles = zmng::preview::scan(&store.file(cam, h)).unwrap();
            if tiles.len() >= 2 { break; }
        }
    }
    assert!(tiles.len() >= 2, "no preview tiles written: {tiles:?}");
    assert!(tiles[1].dts - tiles[0].dts >= dts(1));
    let jpeg = store.nearest(cam, tiles[0].dts, dts(1)).unwrap().unwrap();
    let img = image::load_from_memory(&jpeg).unwrap().to_rgb8();
    assert_eq!((img.width(), img.height()), (160, 90));
    // testsrc is colourful: the tile must not be grey
    let colourful = img.pixels().any(|p| (p[0] as i32 - p[1] as i32).abs() > 40 || (p[1] as i32 - p[2] as i32).abs() > 40);
    assert!(colourful);
    fx.db.update_camera(cam, json!({"enabled": false}).as_object().unwrap()).unwrap();
    zmng::detect::reconcile(ctx).await.unwrap();
}

#[test]
fn retention_prunes_old_preview_hours() {
    let fx = Fixture::new();
    let cam = fx.add_camera("front");
    fx.db.update_camera(cam, json!({"retention_days": 1}).as_object().unwrap()).unwrap();
    let store = Store::new(&fx.thumb_dir);
    let now = zmng::db::now_secs();
    store.append(cam, dts(now - 3 * 86_400), b"\xff\xd8old").unwrap();
    store.append(cam, dts(now - 600), b"\xff\xd8new").unwrap();
    zmng::retention::run_once(&fx.db, &fx.thumb_dir).unwrap();
    let hours = store.hours(cam);
    assert_eq!(hours, vec![hour_key(dts(now - 600))]);
}
