//! Alerts to phones: rules through the API, arming, and delivery to an
//! ntfy-style server and a webhook (a local fake that records requests).
mod common;
use common::*;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use zmng::notify::{DetectedObject, EventInfo, Notification};

#[derive(Clone, Debug)]
struct Got {
    method: String,
    path: String,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

/// A local HTTP server that records every request; answers `status`.
async fn receiver(status: u16) -> (u16, Arc<Mutex<Vec<Got>>>) {
    let got = Arc::new(Mutex::new(Vec::new()));
    let g = got.clone();
    let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
        let g = g.clone();
        async move {
            let (parts, body) = req.into_parts();
            let body = http_body_util::BodyExt::collect(body).await.unwrap().to_bytes().to_vec();
            g.lock().unwrap().push(Got { method: parts.method.to_string(), path: parts.uri.path().to_string(), headers: parts.headers, body });
            axum::http::StatusCode::from_u16(status).unwrap()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (port, got)
}

async fn wait_for(got: &Arc<Mutex<Vec<Got>>>, n: usize) -> Vec<Got> {
    for _ in 0..100 {
        if got.lock().unwrap().len() >= n {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    got.lock().unwrap().clone()
}

fn info(cam: i64, id: i64, objects: &[&str], zones: &[&str]) -> EventInfo {
    EventInfo {
        id, camera_id: cam, camera_name: "front".into(), start_ms: 1_800_000_000_000, end_ms: None, score: 120, kind: "motion".into(), thumb: None,
        objects: objects.iter().map(|l| DetectedObject { label: l.to_string(), confidence: 0.9, bbox: [0.0; 4] }).collect(),
        zones: zones.iter().map(|z| z.to_string()).collect(),
    }
}

fn h(g: &Got, k: &str) -> String {
    g.headers.get(k).map(|v| v.to_str().unwrap().to_string()).unwrap_or_default()
}

#[tokio::test]
async fn rules_api_hides_tokens_and_arming_is_admin_only() {
    let fx = Fixture::new();
    let cam = fx.add_camera("front");
    let admin = fx.token_for(fx.add_admin("admin", "password123"));
    let viewer = fx.token_for(fx.add_viewer("guest", "password123", &[cam]));
    let r = fx.router();
    let rule = json!({"name": "People at night", "triggers": ["person"], "cameras": [cam], "zones": ["Porch"], "schedule": {"days": [0,1,2,3,4,5,6], "from": "22:00", "to": "06:00"},
                      "target_kind": "ntfy", "target_url": "https://ntfy.example/church-cams", "target_token": "tk_secret", "picture": true});
    assert_eq!(call(&r, "POST", "/api/alerts/rules", Some(&viewer), Some(rule.clone())).await.status, 403);
    let res = call(&r, "POST", "/api/alerts/rules", Some(&admin), Some(rule.clone())).await;
    assert_eq!(res.status, 200, "{}", res.text());
    let id = res.json()["id"].as_i64().unwrap();
    let list = get(&r, "/api/alerts", &admin).await;
    assert!(!list.text().contains("tk_secret"), "the token never goes back to the browser");
    let v = list.json();
    assert_eq!(v["armed"], true, "armed by default");
    assert_eq!(v["rules"][0]["token_set"], true);
    assert_eq!(v["rules"][0]["schedule"]["from"], "22:00");
    // an update without a token keeps it; clear_token removes it
    let mut changed = rule.clone();
    changed["target_token"] = json!("");
    changed["name"] = json!("People at night (doors)");
    assert_eq!(call(&r, "PATCH", &format!("/api/alerts/rules/{id}"), Some(&admin), Some(changed.clone())).await.status, 200);
    assert_eq!(fx.db.alert_rule(id).unwrap().unwrap().target_token, "tk_secret");
    changed["clear_token"] = json!(true);
    call(&r, "PATCH", &format!("/api/alerts/rules/{id}"), Some(&admin), Some(changed)).await;
    assert_eq!(fx.db.alert_rule(id).unwrap().unwrap().target_token, "");
    // invalid rules
    for bad in [json!({"name": "", "triggers": ["person"], "target_url": "https://ntfy.sh/x"}), json!({"name": "x", "triggers": ["ghost"], "target_url": "https://ntfy.sh/x"}),
                json!({"name": "x", "triggers": ["person"], "target_url": "not a url"}), json!({"name": "x", "triggers": ["person"], "target_url": "https://ntfy.sh/x", "schedule": {"days": [], "from": "1:00", "to": "2:00"}})] {
        assert_eq!(call(&r, "POST", "/api/alerts/rules", Some(&admin), Some(bad.clone())).await.status, 400, "{bad}");
    }
    // arming: anyone sees it, admins change it, and the change is announced
    let mut rx = fx.bus.subscribe();
    assert_eq!(get(&r, "/api/arm", &viewer).await.json()["armed"], true);
    assert_eq!(call(&r, "POST", "/api/arm", Some(&viewer), Some(json!({"armed": false}))).await.status, 403);
    assert_eq!(call(&r, "POST", "/api/arm", Some(&admin), Some(json!({"armed": false}))).await.status, 200);
    assert_eq!(get(&r, "/api/me", &viewer).await.json()["armed"], false);
    match rx.recv().await.unwrap() {
        Notification::Armed { armed, by } => assert!(!armed && by == "admin"),
        other => panic!("{other:?}"),
    }
    assert_eq!(call(&r, "DELETE", &format!("/api/alerts/rules/{id}"), Some(&admin), None).await.status, 200);
    assert!(get(&r, "/api/alerts", &admin).await.json()["rules"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn alerts_reach_ntfy_and_webhooks_once_per_event() {
    let (port, got) = receiver(200).await;
    let mut fx = Fixture::new();
    fx.cfg.public_url = Some("https://security.example".into());
    let cam = fx.add_camera("front");
    let ntfy = zmng::alerts::Rule { name: "people".into(), triggers: vec!["person".into()], target_url: format!("http://127.0.0.1:{port}/church"), target_token: "tk_1".into(), picture: false, min_interval_secs: 0, ..Default::default() };
    let hook = zmng::alerts::Rule { name: "hook".into(), triggers: vec!["motion".into()], target_kind: "webhook".into(), target_url: format!("http://127.0.0.1:{port}/hook"), picture: false, min_interval_secs: 0, only_when_armed: false, ..Default::default() };
    fx.db.save_alert_rule(&ntfy).unwrap();
    fx.db.save_alert_rule(&hook).unwrap();
    let app = fx.app();
    tokio::spawn(zmng::alerts::run(app.clone(), fx.bus.subscribe()));
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    fx.bus.publish(Notification::EventStart(info(cam, 1, &[], &[])));
    fx.bus.publish(Notification::EventUpdate(info(cam, 1, &["person"], &["Porch"])));
    fx.bus.publish(Notification::EventEnd(info(cam, 1, &["person"], &["Porch"])));
    let all = wait_for(&got, 2).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let all = { let more = got.lock().unwrap().clone(); if more.len() > all.len() { more } else { all } };
    assert_eq!(all.len(), 2, "one per rule for the event: {:?}", all.iter().map(|g| &g.path).collect::<Vec<_>>());
    let n = all.iter().find(|g| g.path == "/church").unwrap();
    assert_eq!(n.method, "POST");
    assert_eq!(h(n, "title"), "front: person");
    assert_eq!(h(n, "priority"), "4");
    assert_eq!(h(n, "tags"), "bust_in_silhouette");
    assert_eq!(h(n, "authorization"), "Bearer tk_1");
    assert_eq!(h(n, "click"), format!("https://security.example/#/camera/{cam}/1800000000000"));
    assert!(String::from_utf8_lossy(&n.body).contains("zone Porch"), "{}", String::from_utf8_lossy(&n.body));
    let w = all.iter().find(|g| g.path == "/hook").unwrap();
    let v: Value = serde_json::from_slice(&w.body).unwrap();
    assert_eq!(v["alert"], "motion");
    assert_eq!(v["event"]["id"], 1);
    assert_eq!(v["url"], format!("https://security.example/#/camera/{cam}/1800000000000"));
    // disarmed: the ntfy rule (only when armed) stays quiet, the webhook (always) still fires
    fx.db.set_armed(false).unwrap();
    fx.bus.publish(Notification::EventUpdate(info(cam, 2, &["person"], &[])));
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let paths: Vec<String> = got.lock().unwrap().iter().skip(2).map(|g| g.path.clone()).collect();
    assert_eq!(paths, vec!["/hook".to_string()]);
    // delivery recorded on the rules
    let rules = fx.db.alert_rules().unwrap();
    assert!(rules.iter().all(|(_, sent, err)| sent.is_some() && err.is_none()), "{rules:?}");
}

#[tokio::test]
async fn ntfy_picture_upload_test_button_and_failures() {
    let (port, got) = receiver(200).await;
    let rule = zmng::alerts::Rule { name: "Küche".into(), triggers: vec!["person".into()], target_url: format!("http://127.0.0.1:{port}/t"), ..Default::default() };
    let a = zmng::alerts::Alert {
        rule_id: 1, trigger: "person".into(), title: "Küche: person".into(), message: "Küche · 2:14 PM".into(), tags: vec!["bust_in_silhouette".into()], priority: 4,
        camera_id: Some(1), event_id: Some(9), open: None, event: None,
    };
    zmng::alerts::send(&reqwest::Client::new(), &rule, &a, Some(bytes::Bytes::from_static(b"\xff\xd8jpeg")), None).await.unwrap();
    let g = wait_for(&got, 1).await;
    assert_eq!(g[0].method, "PUT", "a picture is uploaded as the body");
    assert_eq!(g[0].body, b"\xff\xd8jpeg");
    assert_eq!(h(&g[0], "filename"), "camera.jpg");
    assert_eq!(h(&g[0], "title"), "=?UTF-8?B?S8O8Y2hlOiBwZXJzb24=?=", "non-ASCII headers are RFC 2047 encoded");
    assert!(h(&g[0], "click").is_empty(), "no public_url, no link");
    // the Test button goes through the API, and failures are reported and recorded
    let fx = Fixture::new();
    fx.add_camera("front");
    let admin = fx.token_for(fx.add_admin("admin", "password123"));
    let id = fx.db.save_alert_rule(&zmng::alerts::Rule { name: "ok".into(), picture: false, target_url: format!("http://127.0.0.1:{port}/test"), ..Default::default() }).unwrap();
    let r = fx.router();
    let res = call(&r, "POST", &format!("/api/alerts/rules/{id}/test"), Some(&admin), None).await;
    assert_eq!(res.status, 200, "{}", res.text());
    let g = wait_for(&got, 2).await;
    assert_eq!(h(&g[1], "title"), "Test: ok");
    let (bad_port, _) = receiver(500).await;
    let id = fx.db.save_alert_rule(&zmng::alerts::Rule { name: "broken".into(), picture: false, target_url: format!("http://127.0.0.1:{bad_port}/x"), ..Default::default() }).unwrap();
    let res = call(&r, "POST", &format!("/api/alerts/rules/{id}/test"), Some(&admin), None).await;
    assert_eq!(res.status, 502);
    assert!(res.text().contains("500"), "{}", res.text());
    let list = get(&r, "/api/alerts", &admin).await.json();
    let broken = list["rules"].as_array().unwrap().iter().find(|x| x["name"] == "broken").unwrap().clone();
    assert!(broken["last_error"].as_str().unwrap().contains("500"));
}

#[tokio::test]
async fn home_assistant_arms_through_the_mqtt_switch() {
    let broker = common::mqtt::FakeBroker::start().await;
    let fx = Fixture::new();
    let cfg = zmng::notify::MqttConfig { host: "127.0.0.1".into(), port: broker.port, ..Default::default() };
    tokio::spawn(zmng::notify::mqtt_sink(fx.bus.subscribe(), fx.db.clone(), cfg, fx.hub.clone(), fx.bus.clone()));
    let disc = broker.wait_for("homeassistant/switch/zmng_armed/config", 5).await;
    let v: Value = serde_json::from_slice(&disc.payload).unwrap();
    assert_eq!(v["command_topic"], "zmng/armed/set");
    assert_eq!(broker.wait_for("zmng/armed", 5).await.payload, b"ON");
    broker.wait_subscribed("zmng/armed/set", 5).await;
    broker.send("zmng/armed/set", b"OFF").await;
    for _ in 0..100 {
        if !fx.db.armed().unwrap() { break; }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(!fx.db.armed().unwrap());
    // the new state is published back (retained) for HA's switch
    for _ in 0..100 {
        if broker.last("zmng/armed").unwrap().payload == b"OFF" { break; }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(broker.last("zmng/armed").unwrap().payload, b"OFF");
}
