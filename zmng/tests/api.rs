//! HTTP API integration tests against real segment files.
mod common;
use common::*;
use serde_json::json;
use zmng::mp4::TIMESCALE;

const T0: i64 = 1_800_000_000; // 2027-01-15 UTC, seconds
fn dts(secs: i64) -> i64 { secs * TIMESCALE as i64 }
fn ms(secs: i64) -> i64 { secs * 1000 }

struct World {
    fx: Fixture,
    cam: i64,
    cam2: i64,
    admin_tok: String,
    viewer_tok: String,
}

/// Two cameras; cam 1 has 3 consecutive 4 s segments (main + sub) starting
/// at T0 and two events; the viewer can see only cam 1.
fn world() -> World {
    let fx = Fixture::new();
    let cam = fx.add_camera("front");
    let cam2 = fx.add_camera("back");
    let enc = parse_encoded(&encode_fmp4(4, "libx264", 320, 180));
    for i in 0..3 {
        fx.write_segment(cam, "main", &enc, dts(T0 + i * 4), |fi| if i == 1 && fi == 2 { 200 } else { 0 });
        fx.write_segment(cam, "sub", &enc, dts(T0 + i * 4), |_| 0);
    }
    fx.write_segment(cam2, "main", &enc, dts(T0), |_| 0);
    let e1 = fx.db.insert_event(cam, dts(T0 + 5), "motion").unwrap();
    fx.db.update_event_progress(e1, 200, dts(T0 + 6)).unwrap();
    fx.db.close_event(e1, dts(T0 + 8), None, "{}").unwrap();
    let e2 = fx.db.insert_event(cam, dts(T0 + 9), "motion").unwrap();
    fx.db.update_event_progress(e2, 40, dts(T0 + 9)).unwrap();
    fx.db.close_event(e2, dts(T0 + 11), None, "{}").unwrap();
    let admin = fx.add_admin("admin", "password123");
    let viewer = fx.add_viewer("guest", "password123", &[cam]);
    let admin_tok = fx.token_for(admin);
    let viewer_tok = fx.token_for(viewer);
    World { fx, cam, cam2, admin_tok, viewer_tok }
}

#[tokio::test]
async fn unauthenticated_requests_are_rejected_except_public_routes() {
    let w = world();
    let r = w.fx.router();
    assert_eq!(call(&r, "GET", "/api/cameras", None, None).await.status, 401);
    assert_eq!(call(&r, "GET", "/api/health", None, None).await.status, 200);
    assert_eq!(call(&r, "GET", "/", None, None).await.status, 200);
    assert_eq!(call(&r, "GET", "/api/cameras", Some("bogus"), None).await.status, 401);
}

#[tokio::test]
async fn login_sets_cookie_and_throttles_failures() {
    let w = world();
    let r = w.fx.router();
    let ok = call(&r, "POST", "/api/login", None, Some(json!({"username": "admin", "password": "password123"}))).await;
    assert_eq!(ok.status, 200);
    assert!(ok.header("set-cookie").unwrap().contains("zmng_session="));
    let tok = ok.json()["token"].as_str().unwrap().to_string();
    assert_eq!(get(&r, "/api/me", &tok).await.json()["user"]["role"], "admin");
    let bad = call(&r, "POST", "/api/login", None, Some(json!({"username": "admin", "password": "wrong"}))).await;
    assert_eq!(bad.status, 401);
    // setup is refused once users exist
    let s = call(&r, "POST", "/api/setup", None, Some(json!({"username": "x", "password": "password123", "setup_token": "nope"}))).await;
    assert_eq!(s.status, 403);
}

/// While no user exists, an unauthenticated request is a pseudo-admin only
/// for /api/me; it must not be able to create accounts, cameras or tokens
/// and skip the setup token.
#[tokio::test]
async fn setup_mode_cannot_be_used_to_create_users_without_the_token() {
    let fx = Fixture::new();
    fx.add_camera("front");
    let app = fx.app();
    let r = zmng::api::router(app.clone());
    let me = call(&r, "GET", "/api/me", None, None).await;
    assert_eq!(me.status, 200);
    assert_eq!(me.json()["setup_needed"], true);
    for (m, p, b) in [
        ("POST", "/api/users", Some(json!({"username": "evil", "password": "password123", "role": "admin"}))),
        ("POST", "/api/cameras", Some(json!({"name": "x", "main_url": "rtsp://x/"}))),
        ("POST", "/api/tokens", None),
        ("GET", "/api/cameras", None),
        ("PATCH", "/api/cameras/1", Some(json!({"name": "y"}))),
    ] {
        let res = call(&r, m, p, None, b).await;
        assert!(res.status == 403 || res.status == 401, "{m} {p} -> {}", res.status);
    }
    assert_eq!(fx.db.user_count().unwrap(), 0);
    // the proper path: wrong token 403, short password 400, right token 200, then login works
    let bad = call(&r, "POST", "/api/setup", None, Some(json!({"username": "admin", "password": "password123", "setup_token": "nope"}))).await;
    assert_eq!(bad.status, 403);
    let short = call(&r, "POST", "/api/setup", None, Some(json!({"username": "admin", "password": "short", "setup_token": app.setup_token.as_str()}))).await;
    assert_eq!(short.status, 400);
    let ok = call(&r, "POST", "/api/setup", None, Some(json!({"username": "admin", "password": "password123", "setup_token": app.setup_token.as_str()}))).await;
    assert_eq!(ok.status, 200, "{}", ok.text());
    assert_eq!(fx.db.user_count().unwrap(), 1);
    assert_eq!(call(&r, "GET", "/api/cameras", None, None).await.status, 401);
}

#[tokio::test]
async fn viewers_fail_closed_and_never_see_rtsp_urls() {
    let w = world();
    let r = w.fx.router();
    let cams = get(&r, "/api/cameras", &w.viewer_tok).await.json();
    assert_eq!(cams.as_array().unwrap().len(), 1);
    assert_eq!(cams[0]["id"], w.cam);
    assert_eq!(cams[0]["main_url"], "");
    assert!(cams[0]["sub_url"].is_null());
    let admin = get(&r, "/api/cameras", &w.admin_tok).await.json();
    assert_eq!(admin.as_array().unwrap().len(), 2);
    assert!(admin[0]["main_url"].as_str().unwrap().starts_with("rtsp://"));
    // every media route 404s for an invisible camera
    for path in [
        format!("/api/cameras/{}", w.cam2),
        format!("/api/cameras/{}/timeline?start={}&end={}", w.cam2, ms(T0), ms(T0 + 10)),
        format!("/api/cameras/{}/video.mp4?start={}&end={}", w.cam2, ms(T0), ms(T0 + 2)),
        format!("/api/cameras/{}/playlist.m3u8?start={}&end={}", w.cam2, ms(T0), ms(T0 + 2)),
        format!("/api/cameras/{}/frame.jpg?t={}", w.cam2, ms(T0 + 1)),
        format!("/api/cameras/{}/snapshot.jpg", w.cam2),
    ] {
        assert_eq!(get(&r, &path, &w.viewer_tok).await.status, 404, "{path}");
    }
    let seg2 = w.fx.db.segments_in_range(w.cam2, dts(T0), dts(T0 + 1)).unwrap()[0].id;
    assert_eq!(get(&r, &format!("/api/segments/{seg2}/file.mp4"), &w.viewer_tok).await.status, 404);
    assert_eq!(get(&r, &format!("/api/segments/{seg2}/file.mp4"), &w.admin_tok).await.status, 200);
    // viewers cannot administer
    assert_eq!(call(&r, "POST", "/api/users", Some(&w.viewer_tok), Some(json!({"username": "z", "password": "password123"}))).await.status, 403);
    assert_eq!(call(&r, "PATCH", &format!("/api/cameras/{}", w.cam), Some(&w.viewer_tok), Some(json!({"name": "x"}))).await.status, 403);
    assert_eq!(get(&r, "/api/users", &w.viewer_tok).await.status, 403);
}

#[tokio::test]
async fn timeline_reports_coverage_motion_and_events() {
    let w = world();
    let r = w.fx.router();
    let tl = get(&r, &format!("/api/cameras/{}/timeline?start={}&end={}&bucket=1", w.cam, ms(T0 - 2), ms(T0 + 20)), &w.viewer_tok).await.json();
    // three back-to-back 4 s segments merge into one 12 s coverage interval
    let cov = tl["coverage"].as_array().unwrap();
    assert_eq!(cov.len(), 1);
    assert_eq!(cov[0][0], ms(T0));
    assert_eq!(cov[0][1], ms(T0 + 12));
    // motion at segment 1, fragment 2 = T0+4+2 s => bucket index 8 (start at T0-2)
    let motion = tl["motion"].as_array().unwrap();
    assert_eq!(motion[8], 200);
    assert!(motion.iter().filter(|m| m.as_u64().unwrap() > 0).count() == 1);
    let evs = tl["events"].as_array().unwrap();
    assert_eq!(evs.len(), 2);
    assert_eq!(evs[0]["score"], 200);
    assert_eq!(evs[0]["start"], ms(T0 + 5));
    // range validation
    assert_eq!(get(&r, &format!("/api/cameras/{}/timeline?start=5&end=5", w.cam), &w.viewer_tok).await.status, 400);
}

#[tokio::test]
async fn video_range_streams_decodable_fmp4_across_segments() {
    let w = world();
    let r = w.fx.router();
    // 3 s .. 9 s spans segments 0, 1 and 2: expect GOP-aligned fragments covering the range
    let res = get(&r, &format!("/api/cameras/{}/video.mp4?start={}&end={}", w.cam, ms(T0 + 3), ms(T0 + 9)), &w.viewer_tok).await;
    assert_eq!(res.status, 200);
    assert!(res.header("content-type").unwrap().starts_with("video/mp4; codecs=\"avc1."));
    assert_eq!(res.header("x-first-dts-ms").unwrap(), ms(T0 + 3).to_string());
    assert_eq!(res.header("content-length").unwrap(), res.body.len().to_string());
    // fragments are 1 s GOPs; [3,9) touches 6 fragments = 60 frames
    assert_eq!(count_frames(&res.body), 60);
    // substream works the same way
    let sub = get(&r, &format!("/api/cameras/{}/video.mp4?stream=sub&start={}&end={}", w.cam, ms(T0), ms(T0 + 2)), &w.viewer_tok).await;
    assert_eq!(sub.status, 200);
    assert_eq!(count_frames(&sub.body), 20);
    // nothing recorded there
    assert_eq!(get(&r, &format!("/api/cameras/{}/video.mp4?start={}&end={}", w.cam, ms(T0 + 100), ms(T0 + 101)), &w.viewer_tok).await.status, 404);
    assert_eq!(get(&r, &format!("/api/cameras/{}/video.mp4?stream=bogus&start={}&end={}", w.cam, ms(T0), ms(T0 + 1)), &w.viewer_tok).await.status, 400);
}

#[tokio::test]
async fn hls_playlist_uses_byte_ranges_into_segment_files() {
    let w = world();
    let r = w.fx.router();
    let res = get(&r, &format!("/api/cameras/{}/playlist.m3u8?start={}&end={}", w.cam, ms(T0 + 3), ms(T0 + 5)), &w.viewer_tok).await;
    assert_eq!(res.status, 200);
    let m3u8 = res.text();
    assert!(m3u8.starts_with("#EXTM3U"));
    assert!(m3u8.contains("#EXT-X-MAP:URI=\"/api/segments/"));
    assert!(m3u8.contains("#EXT-X-BYTERANGE:"));
    assert!(m3u8.contains("#EXT-X-PROGRAM-DATE-TIME:2027-"));
    assert!(m3u8.trim_end().ends_with("#EXT-X-ENDLIST"));
    // the byte-range target honours Range requests
    let seg = w.fx.db.segments_in_range(w.cam, dts(T0), dts(T0 + 1)).unwrap()[0].clone();
    let f = seg.index[1];
    let req = axum::http::Request::builder()
        .uri(format!("/api/segments/{}/file.mp4", seg.id))
        .header("authorization", format!("Bearer {}", w.viewer_tok))
        .header("range", format!("bytes={}-{}", f.offset, f.offset + f.len as u64 - 1))
        .body(axum::body::Body::empty())
        .unwrap();
    let res = tower::ServiceExt::oneshot(r.clone(), req).await.unwrap();
    assert_eq!(res.status(), 206);
    let body = http_body_util::BodyExt::collect(res.into_body()).await.unwrap().to_bytes();
    assert_eq!(body.len(), f.len as usize);
    assert_eq!(&body[4..8], b"moof");
}

#[tokio::test]
async fn frame_at_time_decodes_one_keyframe() {
    let w = world();
    let r = w.fx.router();
    let res = get(&r, &format!("/api/cameras/{}/frame.jpg?t={}&width=160", w.cam, ms(T0 + 6)), &w.viewer_tok).await;
    assert_eq!(res.status, 200, "{}", res.text());
    assert_eq!(res.header("content-type").unwrap(), "image/jpeg");
    assert_eq!(&res.body[..2], &[0xFF, 0xD8]);
    let img = image::load_from_memory(&res.body).unwrap();
    assert_eq!(img.width(), 160);
    // substream previews fall back to main when there is no sub recording
    let res = get(&r, &format!("/api/cameras/{}/frame.jpg?t={}&width=160&stream=sub", w.cam2, ms(T0 + 1)), &w.admin_tok).await;
    assert_eq!(res.status, 200);
    assert_eq!(get(&r, &format!("/api/cameras/{}/frame.jpg?t={}", w.cam, ms(T0 + 500)), &w.viewer_tok).await.status, 404);
}

#[tokio::test]
async fn events_list_filters_and_pages_by_keyset() {
    let w = world();
    let r = w.fx.router();
    let all = get(&r, "/api/events", &w.viewer_tok).await.json();
    let all = all.as_array().unwrap();
    assert_eq!(all.len(), 2);
    assert!(all[0]["start"].as_i64().unwrap() > all[1]["start"].as_i64().unwrap(), "newest first");
    let strong = get(&r, "/api/events?min_score=100", &w.viewer_tok).await.json();
    assert_eq!(strong.as_array().unwrap().len(), 1);
    assert_eq!(strong[0]["score"], 200);
    // page of one, then continue with before=
    let p1 = get(&r, "/api/events?limit=1", &w.viewer_tok).await.json();
    let p2 = get(&r, &format!("/api/events?limit=1&before={}", p1[0]["id"]), &w.viewer_tok).await.json();
    assert_eq!(p2.as_array().unwrap().len(), 1);
    assert_ne!(p1[0]["id"], p2[0]["id"]);
    let p3 = get(&r, &format!("/api/events?limit=1&before={}", p2[0]["id"]), &w.viewer_tok).await.json();
    assert!(p3.as_array().unwrap().is_empty());
    // archive + notes round trip, and the detail endpoint
    let id = p1[0]["id"].as_i64().unwrap();
    assert_eq!(call(&r, "PATCH", &format!("/api/events/{id}"), Some(&w.viewer_tok), Some(json!({"archived": true, "notes": "cat"}))).await.status, 200);
    let e = get(&r, &format!("/api/events/{id}"), &w.viewer_tok).await.json();
    assert_eq!(e["archived"], true);
    assert_eq!(e["notes"], "cat");
    assert_eq!(get(&r, "/api/events?archived=true", &w.viewer_tok).await.json().as_array().unwrap().len(), 1);
    // camera filter silently drops cameras the viewer cannot see
    let x = get(&r, &format!("/api/events?camera={}", w.cam2), &w.viewer_tok).await.json();
    assert!(x.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn camera_admin_validates_patches() {
    let w = world();
    let r = w.fx.router();
    let bad = call(&r, "PATCH", &format!("/api/cameras/{}", w.cam), Some(&w.admin_tok), Some(json!({"detect_width": 0}))).await;
    assert_eq!(bad.status, 400);
    let bad = call(&r, "PATCH", &format!("/api/cameras/{}", w.cam), Some(&w.admin_tok), Some(json!({"created_at": 1}))).await;
    assert_eq!(bad.status, 400);
    let ok = call(&r, "PATCH", &format!("/api/cameras/{}", w.cam), Some(&w.admin_tok), Some(json!({"tags": "Outside, CBA", "retention_days": 3}))).await;
    assert_eq!(ok.status, 200);
    let c = get(&r, &format!("/api/cameras/{}", w.cam), &w.admin_tok).await.json();
    assert_eq!(c["tags"], "Outside, CBA");
    assert_eq!(c["retention_days"], 3.0);
    // storages endpoint reports usage from the index
    let st = get(&r, "/api/storages", &w.admin_tok).await.json();
    assert!(st[0]["used_bytes"].as_i64().unwrap() > 0);
    assert_eq!(st[0]["available"], true);
}

/// HEVC main streams: hvc1 sample entries, RFC 6381 codec string, decodable range.
#[tokio::test]
async fn hevc_segments_serve_with_hvc1_codec_string() {
    let fx = Fixture::new();
    let cam = fx.add_camera("hevc");
    let enc = parse_encoded(&encode_fmp4(3, "libx265", 320, 180));
    assert_eq!(enc.params.codec, zmng::mp4::Codec::H265);
    assert!(enc.params.rfc6381.starts_with("hvc1."), "{}", enc.params.rfc6381);
    assert_eq!(&enc.params.sample_entry[4..8], b"hvc1");
    fx.write_segment(cam, "main", &enc, dts(T0), |_| 0);
    let admin = fx.add_admin("admin", "password123");
    let tok = fx.token_for(admin);
    let r = fx.router();
    let res = get(&r, &format!("/api/cameras/{cam}/video.mp4?start={}&end={}", ms(T0), ms(T0 + 2)), &tok).await;
    assert_eq!(res.status, 200);
    assert!(res.header("content-type").unwrap().starts_with("video/mp4; codecs=\"hvc1."), "{:?}", res.header("content-type"));
    assert_eq!(count_frames(&res.body), 20);
    let c = get(&r, &format!("/api/cameras/{cam}"), &tok).await.json();
    assert_eq!(c["name"], "hevc");
    // the ffmpeg-written file used hev1 or hvc1; our init always says hvc1
    let mut hev = enc.params.sample_entry.to_vec();
    hev[4..8].copy_from_slice(b"hev1");
    zmng::mp4::prefer_hvc1(&mut hev);
    assert_eq!(&hev[4..8], b"hvc1");
}

/// Deleting events through the ZoneMinder-compatible API is an admin act;
/// viewers may still archive and annotate.
#[tokio::test]
async fn zm_event_delete_is_admin_only() {
    let w = world();
    let r = w.fx.router();
    let id = w.fx.db.events(Some(&[w.cam]), None, None, 0, false, None, 1, false, None).unwrap()[0].id;
    let del = call(&r, "DELETE", &format!("/zm/api/events/{id}.json?token={}", w.viewer_tok), None, None).await;
    assert_eq!(del.status, 403);
    assert!(w.fx.db.event(id).unwrap().is_some());
    let put = axum::http::Request::builder()
        .method("PUT")
        .uri(format!("/zm/api/events/{id}.json?token={}", w.viewer_tok))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(axum::body::Body::from("Event%5BArchived%5D=1&Event%5BNotes%5D=cat"))
        .unwrap();
    let res = tower::ServiceExt::oneshot(r.clone(), put).await.unwrap();
    assert_eq!(res.status(), 200);
    let e = w.fx.db.event(id).unwrap().unwrap();
    assert!(e.archived);
    assert_eq!(e.notes.as_deref(), Some("cat"));
    let del = call(&r, "DELETE", &format!("/zm/api/events/{id}.json?token={}", w.admin_tok), None, None).await;
    assert_eq!(del.status, 200);
    assert!(w.fx.db.event(id).unwrap().is_none());
}

#[tokio::test]
async fn zm_compatible_api_lists_monitors_and_events() {
    let w = world();
    let r = w.fx.router();
    let login = axum::http::Request::builder()
        .method("POST")
        .uri("/zm/api/host/login.json")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(axum::body::Body::from("user=guest&pass=password123"))
        .unwrap();
    let res = tower::ServiceExt::oneshot(r.clone(), login).await.unwrap();
    assert_eq!(res.status(), 200);
    let body: serde_json::Value = serde_json::from_slice(&http_body_util::BodyExt::collect(res.into_body()).await.unwrap().to_bytes()).unwrap();
    let tok = body["access_token"].as_str().unwrap().to_string();
    let mons = call(&r, "GET", &format!("/zm/api/monitors.json?token={tok}"), None, None).await.json();
    assert_eq!(mons["monitors"].as_array().unwrap().len(), 1);
    assert_eq!(mons["monitors"][0]["Monitor"]["Name"], "front");
    let evs = call(&r, "GET", &format!("/zm/api/events/index/MonitorId:{}.json?token={tok}&limit=10", w.cam), None, None).await.json();
    assert_eq!(evs["pagination"]["count"], 2);
    let eid = evs["events"][0]["Event"]["Id"].as_str().unwrap().to_string();
    let mp4 = call(&r, "GET", &format!("/zm/index.php?view=view_video&eid={eid}&token={tok}"), None, None).await;
    assert_eq!(mp4.status, 200);
    assert_eq!(mp4.header("accept-ranges").unwrap(), "bytes");
    assert!(count_frames(&mp4.body) >= 10);
}

/// The admin surface: users, tokens, storages, camera create/delete, and
/// the login throttle. Viewers are refused everywhere here.
#[tokio::test]
async fn admin_surface_users_tokens_storages_and_cameras() {
    let w = world();
    let r = w.fx.router();
    let (a, v) = (&w.admin_tok, &w.viewer_tok);
    // viewers: every admin route is 403
    for (m, p, body) in [
        ("GET", "/api/users", json!({})),
        ("POST", "/api/users", json!({"username": "x", "password": "password123"})),
        ("POST", "/api/tokens", json!({})),
        ("GET", "/api/storages", json!({})),
        ("POST", "/api/storages", json!({"path": "/tmp"})),
        ("POST", "/api/cameras", json!({"name": "x", "main_url": "rtsp://x/"})),
        ("DELETE", &format!("/api/cameras/{}", w.cam)[..], json!({})),
    ] {
        assert_eq!(call(&r, m, p, Some(v), Some(body)).await.status, 403, "{m} {p}");
    }
    // users: create with validation, list, patch cameras, delete (not yourself)
    assert_eq!(call(&r, "POST", "/api/users", Some(a), Some(json!({"username": "short", "password": "1234"}))).await.status, 400);
    assert_eq!(call(&r, "POST", "/api/users", Some(a), Some(json!({"username": "x", "password": "password123", "role": "root"}))).await.status, 400);
    let uid = call(&r, "POST", "/api/users", Some(a), Some(json!({"username": "ops", "password": "password123", "cameras": [w.cam]}))).await.json()["id"].as_i64().unwrap();
    assert_eq!(call(&r, "POST", "/api/users", Some(a), Some(json!({"username": "ops", "password": "password123"}))).await.status, 400, "duplicate name");
    let users = get(&r, "/api/users", a).await.json();
    let ops = users.as_array().unwrap().iter().find(|u| u["id"] == uid).unwrap();
    assert_eq!(ops["role"], "viewer");
    assert_eq!(ops["cameras"], json!([w.cam]));
    assert_eq!(call(&r, "PATCH", &format!("/api/users/{uid}"), Some(a), Some(json!({"cameras": []}))).await.status, 200);
    assert_eq!(get(&r, "/api/users", a).await.json().as_array().unwrap().iter().find(|u| u["id"] == uid).unwrap()["cameras"], json!([]));
    let me = get(&r, "/api/me", a).await.json()["user"]["id"].as_i64().unwrap();
    assert_eq!(call(&r, "DELETE", &format!("/api/users/{me}"), Some(a), None).await.status, 400);
    assert_eq!(call(&r, "DELETE", &format!("/api/users/{uid}"), Some(a), None).await.status, 200);
    assert!(get(&r, "/api/users", a).await.json().as_array().unwrap().iter().all(|u| u["id"] != uid));
    // tokens: a long-lived token works like a login
    let t = call(&r, "POST", "/api/tokens", Some(a), None).await.json()["token"].as_str().unwrap().to_string();
    assert_eq!(get(&r, "/api/me", &t).await.json()["user"]["role"], "admin");
    // storages: path must exist; create, list, patch
    assert_eq!(call(&r, "POST", "/api/storages", Some(a), Some(json!({"path": "/nonexistent/zmng"}))).await.status, 400);
    let dir = w.fx.storage_path().join("tier2");
    std::fs::create_dir_all(&dir).unwrap();
    let sid = call(&r, "POST", "/api/storages", Some(a), Some(json!({"path": dir.to_str().unwrap(), "max_gb": 1.0, "reserve_gb": 0.1}))).await.json()["id"].as_i64().unwrap();
    let st = get(&r, "/api/storages", a).await.json();
    let s2 = st.as_array().unwrap().iter().find(|s| s["id"] == sid).unwrap();
    assert_eq!(s2["max_bytes"], 1_000_000_000i64);
    assert_eq!(s2["available"], true);
    assert_eq!(call(&r, "PATCH", &format!("/api/storages/{sid}"), Some(a), Some(json!({"archive_after_days": 7}))).await.status, 200);
    assert_eq!(call(&r, "PATCH", &format!("/api/storages/{sid}"), Some(a), Some(json!({"bogus": 1}))).await.status, 400);
    // cameras: create, then delete; deleting an unknown id is 404, not 200
    let cid = call(&r, "POST", "/api/cameras", Some(a), Some(json!({"name": "new", "main_url": "rtsp://10.0.0.9/main"}))).await.json()["id"].as_i64().unwrap();
    assert_eq!(get(&r, &format!("/api/cameras/{cid}"), a).await.json()["name"], "new");
    assert_eq!(call(&r, "DELETE", &format!("/api/cameras/{cid}"), Some(a), None).await.status, 200);
    assert_eq!(get(&r, &format!("/api/cameras/{cid}"), a).await.status, 404);
    assert_eq!(call(&r, "DELETE", &format!("/api/cameras/{cid}"), Some(a), None).await.status, 404);
    assert_eq!(call(&r, "DELETE", "/api/cameras/999999", Some(a), None).await.status, 404);
}

/// Ten failed logins in a minute lock the login route with 429 (a token
/// still works, so an operator is never locked out by a scanner).
#[tokio::test]
async fn login_locks_after_ten_failures() {
    let w = world();
    let r = w.fx.router();
    let bad = json!({"username": "admin", "password": "wrong"});
    for _ in 0..10 {
        assert_eq!(call(&r, "POST", "/api/login", None, Some(bad.clone())).await.status, 401);
    }
    assert_eq!(call(&r, "POST", "/api/login", None, Some(json!({"username": "admin", "password": "password123"}))).await.status, 429);
    assert_eq!(get(&r, "/api/me", &w.admin_tok).await.status, 200);
}

/// Group permissions: a viewer granted a group sees every camera tagged with
/// it (and gains a camera as soon as it is tagged); renaming a group keeps the
/// grant; removing it revokes it. Users can be renamed, re-passworded and
/// re-roled, but the last admin cannot be demoted.
#[tokio::test]
async fn group_permissions_and_user_editing() {
    let w = world();
    let r = w.fx.router();
    let a = &w.admin_tok;
    let uid = call(&r, "POST", "/api/users", Some(a), Some(json!({"username": "cba.guest", "password": "password123", "groups": ["CBA"]}))).await.json()["id"].as_i64().unwrap();
    let vt = w.fx.token_for(uid);
    // nothing tagged CBA yet: fails closed
    assert!(get(&r, "/api/cameras", &vt).await.json().as_array().unwrap().is_empty());
    assert_eq!(call(&r, "PATCH", &format!("/api/cameras/{}", w.cam2), Some(a), Some(json!({"tags": "Outside, cba"}))).await.status, 200);
    let cams = get(&r, "/api/cameras", &vt).await.json();
    assert_eq!(cams.as_array().unwrap().len(), 1, "group names match case-insensitively");
    assert_eq!(cams[0]["id"], w.cam2);
    assert_eq!(cams[0]["main_url"], "", "still a viewer: no RTSP URLs");
    assert_eq!(get(&r, &format!("/api/cameras/{}/timeline?start={}&end={}", w.cam, ms(T0), ms(T0 + 10)), &vt).await.status, 404);
    let users = get(&r, "/api/users", a).await.json();
    let u = users.as_array().unwrap().iter().find(|u| u["id"] == uid).unwrap();
    assert_eq!(u["groups"], json!(["CBA"]));
    assert_eq!(u["visible"], json!([w.cam2]));
    // zmNinjaNg sees the group with only the monitors this user may see
    let g = get(&r, "/zm/api/groups.json", &vt).await.json();
    let names: Vec<&str> = g["groups"].as_array().unwrap().iter().map(|x| x["Group"]["Name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["cba", "Outside"]);
    assert_eq!(g["groups"][0]["Monitor"][0]["Id"], w.cam2.to_string());
    assert_eq!(get(&r, "/zm/api/users.json", &vt).await.json()["users"][0]["User"]["Groups"], "View");
    // rename the group: the grant follows
    let res = call(&r, "POST", "/api/groups/rename", Some(a), Some(json!({"from": "CBA", "to": "Charleston Bilingual"}))).await;
    assert_eq!(res.status, 200, "{}", res.text());
    assert_eq!(res.json()["cameras"], 1);
    assert_eq!(get(&r, &format!("/api/cameras/{}", w.cam2), a).await.json()["tags"], "Outside, Charleston Bilingual");
    assert_eq!(get(&r, "/api/cameras", &vt).await.json().as_array().unwrap().len(), 1);
    assert_eq!(call(&r, "POST", "/api/groups/rename", Some(a), Some(json!({"from": "Outside", "to": "a,b"}))).await.status, 400);
    assert_eq!(call(&r, "POST", "/api/groups/rename", Some(&vt), Some(json!({"from": "Outside", "to": "x"}))).await.status, 403);
    // remove it: access is gone, the camera's other group stays
    assert_eq!(call(&r, "POST", "/api/groups/rename", Some(a), Some(json!({"from": "charleston bilingual"}))).await.status, 200);
    assert_eq!(get(&r, &format!("/api/cameras/{}", w.cam2), a).await.json()["tags"], "Outside");
    assert!(get(&r, "/api/cameras", &vt).await.json().as_array().unwrap().is_empty());

    // edit users: rename (sign-ins and tokens survive), password, role
    let me = get(&r, "/api/me", a).await.json()["user"]["id"].as_i64().unwrap();
    assert_eq!(call(&r, "PATCH", &format!("/api/users/{me}"), Some(a), Some(json!({"username": "bad name"}))).await.status, 400);
    assert_eq!(call(&r, "PATCH", &format!("/api/users/{me}"), Some(a), Some(json!({"username": "cba.guest"}))).await.status, 400, "taken");
    assert_eq!(call(&r, "PATCH", &format!("/api/users/{me}"), Some(a), Some(json!({"username": "atodd"}))).await.status, 200);
    assert_eq!(get(&r, "/api/me", a).await.json()["user"]["username"], "atodd");
    assert_eq!(call(&r, "POST", "/api/login", None, Some(json!({"username": "atodd", "password": "password123"}))).await.status, 200);
    assert_eq!(call(&r, "PATCH", &format!("/api/users/{uid}"), Some(a), Some(json!({"password": "short"}))).await.status, 400);
    assert_eq!(call(&r, "PATCH", &format!("/api/users/{uid}"), Some(a), Some(json!({"password": "a-new-password"}))).await.status, 200);
    assert_eq!(call(&r, "POST", "/api/login", None, Some(json!({"username": "cba.guest", "password": "a-new-password"}))).await.status, 200);
    // the last admin cannot be demoted, and nobody demotes themselves
    assert_eq!(call(&r, "PATCH", &format!("/api/users/{me}"), Some(a), Some(json!({"role": "viewer"}))).await.status, 400);
    assert_eq!(call(&r, "PATCH", &format!("/api/users/{uid}"), Some(a), Some(json!({"role": "admin"}))).await.status, 200);
    assert_eq!(call(&r, "PATCH", &format!("/api/users/{me}"), Some(a), Some(json!({"role": "viewer"}))).await.status, 400, "not your own role");
    assert_eq!(call(&r, "PATCH", &format!("/api/users/{uid}"), Some(a), Some(json!({"role": "viewer"}))).await.status, 200, "another admin can be demoted");
    assert_eq!(call(&r, "PATCH", "/api/users/999999", Some(a), Some(json!({"role": "viewer"}))).await.status, 404);
}
