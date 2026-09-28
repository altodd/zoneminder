//! ONVIF PTZ against a mock camera: WS-Security digest verification, probe,
//! move/stop/presets through the API, auto-stop, ACL, zmNinja control mapping.
mod common;
use common::*;
use base64::Engine;
use serde_json::json;
use sha1::Digest;
use std::sync::{Arc, Mutex};

const PASSWORD: &str = "cam-secret";

struct Mock {
    port: u16,
    /// SOAP operation names in the order received
    ops: Arc<Mutex<Vec<String>>>,
    /// last ContinuousMove velocities (pan, tilt, zoom)
    last_move: Arc<Mutex<Option<(f64, f64, f64)>>>,
}

fn attr<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let i = xml.find(&format!("{name}=\""))? + name.len() + 2;
    let rest = &xml[i..];
    Some(&rest[..rest.find('"')?])
}

/// Verify the WS-Security PasswordDigest of a request against PASSWORD.
fn auth_ok(body: &str) -> bool {
    let Some(nonce) = zmng::onvif::xml_first(body, "Nonce") else { return false };
    let Some(created) = zmng::onvif::xml_first(body, "Created") else { return false };
    let Some(digest) = zmng::onvif::xml_first(body, "Password") else { return false };
    let nonce = base64::engine::general_purpose::STANDARD.decode(nonce).unwrap_or_default();
    let mut h = sha1::Sha1::new();
    h.update(&nonce);
    h.update(created.as_bytes());
    h.update(PASSWORD.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(h.finalize()) == digest
}

async fn mock_camera() -> Mock {
    let ops = Arc::new(Mutex::new(Vec::new()));
    let last_move = Arc::new(Mutex::new(None));
    let (o2, m2) = (ops.clone(), last_move.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handler = move |axum::extract::Path(svc): axum::extract::Path<String>, body: String| {
        let (ops, last_move) = (o2.clone(), m2.clone());
        async move {
            if !auth_ok(&body) {
                return (axum::http::StatusCode::BAD_REQUEST, "<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\"><s:Body><s:Fault><s:Code><s:Value>s:Sender</s:Value></s:Code><s:Reason><s:Text xml:lang=\"en\">Sender not Authorized</s:Text></s:Reason></s:Fault></s:Body></s:Envelope>".to_string());
            }
            let soap_body = zmng::onvif::xml_section(&body, "Body").unwrap_or("");
            let op = soap_body.trim_start_matches("<s:Body>").trim_start().trim_start_matches('<').split(|c: char| c.is_whitespace() || c == '>').next().unwrap_or("").rsplit(':').next().unwrap_or("").to_string();
            ops.lock().unwrap().push(op.clone());
            let reply = match (svc.as_str(), op.as_str()) {
                ("device_service", "GetCapabilities") => format!("<tds:GetCapabilitiesResponse><tds:Capabilities><tt:Media><tt:XAddr>http://127.0.0.1:{port}/onvif/Media</tt:XAddr></tt:Media><tt:PTZ><tt:XAddr>http://127.0.0.1:{port}/onvif/PTZ</tt:XAddr></tt:PTZ></tds:Capabilities></tds:GetCapabilitiesResponse>"),
                ("Media", "GetProfiles") => "<trt:GetProfilesResponse><trt:Profiles token=\"Profile_1\"><tt:Name>main</tt:Name></trt:Profiles><trt:Profiles token=\"Profile_2\"><tt:Name>sub</tt:Name><tt:PTZConfiguration token=\"PTZ_1\"><tt:Name>ptz</tt:Name></tt:PTZConfiguration></trt:Profiles></trt:GetProfilesResponse>".into(),
                ("PTZ", "ContinuousMove") => {
                    let pt = zmng::onvif::xml_section(&body, "PanTilt").unwrap_or("");
                    let z = zmng::onvif::xml_section(&body, "Zoom").unwrap_or("");
                    let f = |s: &str, a: &str| attr(s, a).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
                    *last_move.lock().unwrap() = Some((f(pt, "x"), f(pt, "y"), f(z, "x")));
                    assert_eq!(zmng::onvif::xml_first(&body, "ProfileToken"), Some("Profile_2"));
                    "<tptz:ContinuousMoveResponse/>".into()
                }
                ("PTZ", "Stop") => "<tptz:StopResponse/>".into(),
                ("PTZ", "GetPresets") => "<tptz:GetPresetsResponse><tptz:Preset token=\"1\"><tt:Name>Door</tt:Name></tptz:Preset><tptz:Preset token=\"2\"><tt:Name>Lot</tt:Name></tptz:Preset></tptz:GetPresetsResponse>".into(),
                ("PTZ", "GotoPreset") => { assert_eq!(zmng::onvif::xml_first(&body, "PresetToken"), Some("2")); "<tptz:GotoPresetResponse/>".into() }
                ("PTZ", "GotoHomePosition") => "<tptz:GotoHomePositionResponse/>".into(),
                ("PTZ", "SetPreset") => "<tptz:SetPresetResponse><tptz:PresetToken>9</tptz:PresetToken></tptz:SetPresetResponse>".into(),
                _ => return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "<s:Envelope><s:Body><s:Fault><s:Reason><s:Text>unknown op</s:Text></s:Reason></s:Fault></s:Body></s:Envelope>".to_string()),
            };
            (axum::http::StatusCode::OK, format!("<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\"><s:Body>{reply}</s:Body></s:Envelope>"))
        }
    };
    let app = axum::Router::new().route("/onvif/{svc}", axum::routing::post(handler));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Mock { port, ops, last_move }
}

fn setup(fx: &Fixture, port: u16) -> (i64, String, String) {
    let cam = fx.db.add_camera("dome", &format!("rtsp://admin:{PASSWORD}@127.0.0.1:{port}/Streaming/Channels/101"), None, fx.storage_id).unwrap();
    fx.db.update_camera(cam, json!({"onvif_url": format!("http://127.0.0.1:{port}/onvif/device_service")}).as_object().unwrap()).unwrap();
    let admin = fx.add_admin("admin", "password123");
    let viewer = fx.add_viewer("v", "password123", &[cam]);
    (cam, fx.token_for(admin), fx.token_for(viewer))
}

#[tokio::test]
async fn probe_move_stop_presets_through_the_api() {
    let m = mock_camera().await;
    let fx = Fixture::new();
    let (cam, admin, viewer) = setup(&fx, m.port);
    let r = fx.router();
    // before the probe there is no PTZ
    assert_eq!(call(&r, "POST", &format!("/api/cameras/{cam}/ptz"), Some(&admin), Some(json!({"action": "stop"}))).await.status, 400);
    assert_eq!(call(&r, "POST", &format!("/api/cameras/{cam}/ptz/probe"), Some(&viewer), None).await.status, 403);
    let p = call(&r, "POST", &format!("/api/cameras/{cam}/ptz/probe"), Some(&admin), None).await;
    assert_eq!(p.status, 200, "{}", p.text());
    let p = p.json();
    assert_eq!(p["profile"], "Profile_2", "the PTZ-capable profile");
    assert_eq!(p["ptz_url"], format!("http://127.0.0.1:{}/onvif/PTZ", m.port));
    let c = fx.db.camera(cam).unwrap().unwrap();
    assert!(c.ptz && c.ptz_profile == "Profile_2");
    // viewers see ptz=true but no ONVIF URLs, and never a password
    let v = get(&r, &format!("/api/cameras/{cam}"), &viewer).await.json();
    assert_eq!(v["ptz"], true);
    assert_eq!(v["onvif_url"], "");
    assert!(v.get("onvif_pass").is_none());
    let a = get(&r, &format!("/api/cameras/{cam}"), &admin).await.json();
    assert!(a["onvif_url"].as_str().unwrap().contains("device_service"));
    assert!(a.get("onvif_pass").is_none());
    // move with auto-stop
    let mv = call(&r, "POST", &format!("/api/cameras/{cam}/ptz"), Some(&admin), Some(json!({"action": "move", "pan": 0.5, "tilt": -0.25, "zoom": 0, "seconds": 0.3}))).await;
    assert_eq!(mv.status, 200, "{}", mv.text());
    assert_eq!(*m.last_move.lock().unwrap(), Some((0.5, -0.25, 0.0)));
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    assert_eq!(m.ops.lock().unwrap().last().map(|s| s.as_str()), Some("Stop"), "auto-stop after 0.3 s: {:?}", m.ops.lock().unwrap());
    // a new move cancels the pending stop; explicit stop
    assert_eq!(call(&r, "POST", &format!("/api/cameras/{cam}/ptz"), Some(&admin), Some(json!({"action": "move", "pan": -1, "tilt": 0, "zoom": 0, "seconds": 5}))).await.status, 200);
    assert_eq!(call(&r, "POST", &format!("/api/cameras/{cam}/ptz"), Some(&admin), Some(json!({"action": "stop"}))).await.status, 200);
    assert_eq!(call(&r, "POST", &format!("/api/cameras/{cam}/ptz"), Some(&admin), Some(json!({"action": "move", "pan": 2, "tilt": 0, "zoom": 0}))).await.status, 400);
    // presets (viewers may list), goto, set, home
    let ps = get(&r, &format!("/api/cameras/{cam}/ptz/presets"), &viewer).await.json();
    assert_eq!(ps, json!([{"token": "1", "name": "Door"}, {"token": "2", "name": "Lot"}]));
    assert_eq!(call(&r, "POST", &format!("/api/cameras/{cam}/ptz"), Some(&admin), Some(json!({"action": "preset", "preset": "2"}))).await.status, 200);
    assert_eq!(call(&r, "POST", &format!("/api/cameras/{cam}/ptz"), Some(&admin), Some(json!({"action": "set_preset", "name": "Gate"}))).await.json()["preset"], "9");
    assert_eq!(call(&r, "POST", &format!("/api/cameras/{cam}/ptz"), Some(&admin), Some(json!({"action": "home"}))).await.status, 200);
    assert_eq!(call(&r, "POST", &format!("/api/cameras/{cam}/ptz"), Some(&viewer), Some(json!({"action": "home"}))).await.status, 403);
    let ops = m.ops.lock().unwrap().clone();
    assert!(ops.iter().any(|o| o == "GotoPreset") && ops.iter().any(|o| o == "SetPreset") && ops.iter().any(|o| o == "GotoHomePosition"), "{ops:?}");
}

#[tokio::test]
async fn wrong_credentials_are_reported_not_hidden() {
    let m = mock_camera().await;
    let fx = Fixture::new();
    let (cam, admin, _) = setup(&fx, m.port);
    fx.db.update_camera(cam, json!({"onvif_user": "admin", "onvif_pass": "wrong"}).as_object().unwrap()).unwrap();
    let r = fx.router();
    let p = call(&r, "POST", &format!("/api/cameras/{cam}/ptz/probe"), Some(&admin), None).await;
    assert_eq!(p.status, 502);
    assert!(p.text().contains("Sender not Authorized"), "{}", p.text());
    assert!(!p.text().contains("wrong"));
    // a camera that is not there at all
    fx.db.update_camera(cam, json!({"onvif_url": "http://127.0.0.1:1/onvif/device_service"}).as_object().unwrap()).unwrap();
    assert_eq!(call(&r, "POST", &format!("/api/cameras/{cam}/ptz/probe"), Some(&admin), None).await.status, 502);
}

#[tokio::test]
async fn zmninja_control_requests_map_to_onvif() {
    let m = mock_camera().await;
    let fx = Fixture::new();
    let (cam, admin, viewer) = setup(&fx, m.port);
    let r = fx.router();
    // not controllable before the probe
    let mon = call(&r, "GET", &format!("/zm/api/monitors/{cam}.json?token={admin}"), None, None).await.json();
    assert_eq!(mon["monitor"]["Monitor"]["Controllable"], "0");
    assert_eq!(call(&r, "POST", &format!("/api/cameras/{cam}/ptz/probe"), Some(&admin), None).await.status, 200);
    let mon = call(&r, "GET", &format!("/zm/api/monitors/{cam}.json?token={admin}"), None, None).await.json();
    assert_eq!(mon["monitor"]["Monitor"]["Controllable"], "1");
    assert_eq!(mon["monitor"]["Monitor"]["ControlId"], "1");
    let ctl = call(&r, "GET", &format!("/zm/api/controls/1.json?token={admin}"), None, None).await.json();
    assert_eq!(ctl["control"]["Control"]["CanMoveCon"], "1");
    for (cmd, want) in [("moveConUpLeft", (-1.0, 1.0, 0.0)), ("moveConDown", (0.0, -1.0, 0.0)), ("zoomConTele", (0.0, 0.0, 1.0))] {
        let res = call(&r, "GET", &format!("/zm/index.php?view=request&request=control&id={cam}&control={cmd}&token={admin}"), None, None).await;
        assert_eq!(res.status, 200, "{cmd}: {}", res.text());
        assert_eq!(*m.last_move.lock().unwrap(), Some(want), "{cmd}");
    }
    assert_eq!(call(&r, "GET", &format!("/zm/index.php?view=request&request=control&id={cam}&control=moveStop&token={admin}"), None, None).await.status, 200);
    assert_eq!(call(&r, "GET", &format!("/zm/index.php?view=request&request=control&id={cam}&control=presetGoto&preset=2&token={admin}"), None, None).await.status, 200);
    assert_eq!(call(&r, "GET", &format!("/zm/index.php?view=request&request=control&id={cam}&control=bogus&token={admin}"), None, None).await.status, 400);
    assert_eq!(call(&r, "GET", &format!("/zm/index.php?view=request&request=control&id={cam}&control=moveConUp&token={viewer}"), None, None).await.status, 403);
    let ops = m.ops.lock().unwrap().clone();
    assert_eq!(ops.iter().filter(|o| *o == "Stop").count(), 1);
    assert_eq!(ops.iter().filter(|o| *o == "GotoPreset").count(), 1);
}
