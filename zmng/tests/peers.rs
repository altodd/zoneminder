//! Federation: a second server ("barn") is proxied under /api/peers/barn.
mod common;
use common::*;
use serde_json::json;
use zmng::mp4::TIMESCALE;

const T0: i64 = 1_800_000_000;
fn dts(secs: i64) -> i64 { secs * TIMESCALE as i64 }
fn ms(secs: i64) -> i64 { secs * 1000 }

/// Start a peer server with two cameras; the returned token belongs to a
/// viewer that may see only the first.
async fn peer() -> (Fixture, u16, String, i64, i64) {
    let fx = Fixture::new();
    let cam = fx.add_camera("barn-door");
    let cam2 = fx.add_camera("barn-secret");
    let enc = parse_encoded(&encode_fmp4(3, "libx264", 160, 90));
    fx.write_segment(cam, "main", &enc, dts(T0), |_| 0);
    let ev = fx.db.insert_event(cam, dts(T0 + 1), "motion").unwrap();
    fx.db.close_event(ev, dts(T0 + 2), None, "{}").unwrap();
    let viewer = fx.add_viewer("site-a", "password123", &[cam]);
    let tok = fx.token_for(viewer);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let router = fx.router();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (fx, port, tok, cam, cam2)
}

#[tokio::test]
async fn proxy_serves_peer_cameras_events_and_media_with_the_peer_acl() {
    let (peer_fx, port, peer_tok, cam, cam2) = peer().await;
    let mut local = Fixture::new();
    local.cfg.peers = vec![zmng::peers::PeerConfig { name: "barn".into(), url: format!("http://127.0.0.1:{port}"), token: peer_tok }];
    let admin = local.add_admin("admin", "password123");
    let viewer = local.add_viewer("v", "password123", &[]);
    let (at, vt) = (local.token_for(admin), local.token_for(viewer));
    let r = local.router();
    assert_eq!(get(&r, "/api/me", &vt).await.json()["peers"], json!(["barn"]));
    // reachability: viewers get name + ok, admins also the url
    let p = get(&r, "/api/peers", &vt).await.json();
    assert_eq!(p, json!([{"name": "barn", "ok": true}]));
    assert!(get(&r, "/api/peers", &at).await.json()[0]["url"].as_str().unwrap().contains("127.0.0.1"));
    // cameras: only what the peer token may see, and no RTSP URLs
    let cams = get(&r, "/api/peers/barn/api/cameras", &vt).await;
    assert_eq!(cams.status, 200, "{}", cams.text());
    let cams = cams.json();
    assert_eq!(cams.as_array().unwrap().len(), 1);
    assert_eq!(cams[0]["name"], "barn-door");
    assert_eq!(cams[0]["main_url"], "");
    assert_eq!(get(&r, &format!("/api/peers/barn/api/cameras/{cam2}"), &vt).await.status, 404);
    // events, timeline, media are byte-identical to what the peer serves directly
    let evs = get(&r, "/api/peers/barn/api/events?limit=10", &vt).await.json();
    assert_eq!(evs.as_array().unwrap().len(), 1);
    let via = get(&r, &format!("/api/peers/barn/api/cameras/{cam}/video.mp4?start={}&end={}", ms(T0), ms(T0 + 2)), &vt).await;
    assert_eq!(via.status, 200);
    assert!(via.header("content-type").unwrap().starts_with("video/mp4"));
    assert_eq!(via.header("x-first-dts-ms").unwrap(), ms(T0).to_string());
    let direct = get(&peer_fx.router(), &format!("/api/cameras/{cam}/video.mp4?start={}&end={}", ms(T0), ms(T0 + 2)), &get_token(&peer_fx)).await;
    assert_eq!(via.body, direct.body);
    let tl = get(&r, &format!("/api/peers/barn/api/cameras/{cam}/timeline?start={}&end={}", ms(T0), ms(T0 + 5)), &vt).await.json();
    assert_eq!(tl["events"].as_array().unwrap().len(), 1);
    // Range requests pass through
    let seg = peer_fx.db.segments_in_range(cam, dts(T0), dts(T0 + 1)).unwrap()[0].clone();
    let req = axum::http::Request::builder().uri(format!("/api/peers/barn/api/segments/{}/file.mp4", seg.id)).header("authorization", format!("Bearer {vt}")).header("range", "bytes=0-99").body(axum::body::Body::empty()).unwrap();
    let res = tower::ServiceExt::oneshot(r.clone(), req).await.unwrap();
    assert_eq!(res.status(), 206);
    assert_eq!(res.headers()["content-range"].to_str().unwrap(), format!("bytes 0-99/{}", seg.bytes));
    // PATCH on events is proxied; the peer's own rules apply
    let ev_id = evs[0]["id"].as_i64().unwrap();
    assert_eq!(call(&r, "PATCH", &format!("/api/peers/barn/api/events/{ev_id}"), Some(&vt), Some(json!({"notes": "from site A"}))).await.status, 200);
    assert_eq!(peer_fx.db.event(ev_id).unwrap().unwrap().notes.as_deref(), Some("from site A"));
    // not proxied: users, tokens, storage, the peer's peers, the UI, writes on cameras
    for (m, p) in [("GET", "/api/peers/barn/api/users"), ("POST", "/api/peers/barn/api/tokens"), ("GET", "/api/peers/barn/api/storages"), ("GET", "/api/peers/barn/api/peers"), ("GET", "/api/peers/barn/index.html"), ("PATCH", &format!("/api/peers/barn/api/cameras/{cam}")), ("DELETE", &format!("/api/peers/barn/api/events/{ev_id}"))] {
        let res = call(&r, m, p, Some(&at), Some(json!({}))).await;
        assert_eq!(res.status, 404, "{m} {p}");
    }
    // PTZ through a peer is admin-only locally
    assert_eq!(call(&r, "POST", &format!("/api/peers/barn/api/cameras/{cam}/ptz"), Some(&vt), Some(json!({"action": "stop"}))).await.status, 403);
    assert_eq!(get(&r, "/api/peers/nope/api/cameras", &vt).await.status, 404);
    assert_eq!(call(&r, "GET", "/api/peers/barn/api/cameras", None, None).await.status, 401);
}

#[tokio::test]
async fn unreachable_peer_is_reported_not_fatal() {
    let mut local = Fixture::new();
    local.cfg.peers = vec![zmng::peers::PeerConfig { name: "gone".into(), url: "http://127.0.0.1:1".into(), token: "x".into() }];
    let admin = local.add_admin("admin", "password123");
    let at = local.token_for(admin);
    let r = local.router();
    assert_eq!(get(&r, "/api/peers", &at).await.json()[0]["ok"], false);
    let res = get(&r, "/api/peers/gone/api/cameras", &at).await;
    assert_eq!(res.status, 502);
    assert!(res.text().contains("peer gone"));
    // local cameras are unaffected
    assert_eq!(get(&r, "/api/cameras", &at).await.status, 200);
}

#[test]
fn config_rejects_bad_peers() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("zmng.toml");
    std::fs::write(&p, "db_path = \"x.db\"\n[[peers]]\nname = \"Bad Name\"\nurl = \"http://a\"\ntoken = \"t\"\n").unwrap();
    let e = zmng::config::Config::load(&p).unwrap_err().to_string();
    assert!(e.contains("must match"), "{e}");
    std::fs::write(&p, "db_path = \"x.db\"\n[[peers]]\nname = \"barn\"\nurl = \"http://a:8080\"\ntoken = \"t\"\n").unwrap();
    let c = zmng::config::Config::load(&p).unwrap();
    assert_eq!(c.peers[0].name, "barn");
}

fn get_token(fx: &Fixture) -> String {
    let admin = fx.add_admin("peer-admin", "password123");
    fx.token_for(admin)
}
