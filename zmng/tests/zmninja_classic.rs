//! The original zmNinja (Ionic/Cordova, ZoneMinder/zmNinja) against the
//! ZoneMinder-compatible API: the exact URLs it builds in NVR.js,
//! EventCtrl.js, EventModalCtrl.js and MontageCtrl.js.
mod common;
use common::*;
use zmng::mp4::TIMESCALE;

const T0: i64 = 1_800_000_000;
fn dts(secs: i64) -> i64 {
    secs * TIMESCALE as i64
}

/// NVR.js getMonitors: `monitors/index/Type!=:WebSite/Deleted=:0/Capturing!=:None.json`
/// (the path it builds for a server newer than 1.37.39). It used to fall
/// through to the catch-all and read as zero monitors.
#[tokio::test]
async fn monitor_list_with_zoneminder_path_filters() {
    let fx = Fixture::new();
    let front = fx.add_camera("front");
    fx.add_camera("back");
    let admin = fx.token_for(fx.add_admin("admin", "password123"));
    let viewer = fx.token_for(fx.add_viewer("guest", "password123", &[front]));
    let r = fx.router();
    let url = "/zm/api/monitors/index/Type!=:WebSite/Deleted=:0/Capturing!=:None.json";
    let all = get(&r, url, &admin).await.json();
    assert_eq!(all["monitors"].as_array().unwrap().len(), 2, "{all}");
    let one = get(&r, url, &viewer).await.json();
    assert_eq!(one["monitors"].as_array().unwrap().len(), 1);
    assert_eq!(one["monitors"][0]["Monitor"]["Id"], front.to_string());
    // older servers' variant
    assert_eq!(get(&r, "/zm/api/monitors/index/Type!=:WebSite/Function!=:None.json", &admin).await.json()["monitors"].as_array().unwrap().len(), 2);
    // a single monitor still resolves
    assert_eq!(get(&r, &format!("/zm/api/monitors/{front}.json"), &admin).await.json()["monitor"]["Monitor"]["Id"], front.to_string());
}

/// Events carry ZoneMinder's virtual fields (StartTime, EndTime and their
/// epoch seconds) and the Monitor; the list filters per monitor with
/// `MonitorId =:` and prev/next use strict `StartTime >:` / `<:`.
#[tokio::test]
async fn events_and_prev_next_navigation() {
    let fx = Fixture::new();
    let front = fx.add_camera("front");
    let back = fx.add_camera("back");
    let admin = fx.token_for(fx.add_admin("admin", "password123"));
    let r = fx.router();
    let ev = |cam: i64, start: i64, half: bool| {
        let s = dts(start) + if half { TIMESCALE as i64 / 2 } else { 0 };
        fx.db.insert_event_full(cam, s, s + dts(10), "motion", 50, None, None, "{}", false, None).unwrap()
    };
    let e1 = ev(front, T0, false);
    let e2 = ev(front, T0 + 60, true); // starts mid-second: "StartTime >:" its own (truncated) time must skip it
    let e3 = ev(front, T0 + 120, false);
    ev(back, T0 + 90, false);

    // EventCtrl: per-monitor list
    let list = get(&r, &format!("/zm/api/events/index/MonitorId%20=:{front}.json?sort=StartTime&direction=desc"), &admin).await.json();
    let evs = list["events"].as_array().unwrap();
    assert_eq!(evs.len(), 3, "{list}");
    let e = &evs[1]["Event"];
    assert_eq!(e["Id"], e2.to_string());
    assert_eq!(e["StartTime"], e["StartDateTime"]);
    assert_eq!(e["EndTime"], e["EndDateTime"]);
    assert_eq!(e["StartTimeSecs"], (T0 + 60).to_string());
    assert_eq!(e["EndTimeSecs"], (T0 + 70).to_string());
    assert_eq!(evs[1]["Monitor"]["Id"], front.to_string());

    // EventModalCtrl neighbours, built from the current event's StartTime
    let start2 = e["StartTime"].as_str().unwrap().replace(' ', "%20");
    let next = get(&r, &format!("/zm/api/events/index/StartTime%20%3E:{start2}/MonitorId%20=:{front}.json?sort=StartTime&direction=asc&limit=1"), &admin).await.json();
    assert_eq!(next["events"][0]["Event"]["Id"], e3.to_string(), "{next}");
    let prev = get(&r, &format!("/zm/api/events/index/StartTime%20%3C:{start2}/MonitorId%20=:{front}.json?sort=StartTime&direction=desc&limit=1"), &admin).await.json();
    assert_eq!(prev["events"][0]["Event"]["Id"], e1.to_string(), "{prev}");
    // across monitors (followSameMonitor off): the back camera's event is next
    let next_any = get(&r, &format!("/zm/api/events/index/StartTime%20%3E:{start2}.json?sort=StartTime&direction=asc&limit=1"), &admin).await.json();
    assert_eq!(next_any["events"][0]["Event"]["MonitorId"], back.to_string());

    // NVR.js timeline: StartTime <= to, EndTime >= from
    let from = get(&r, &format!("/zm/api/events/{e2}.json"), &admin).await.json()["event"]["Event"]["EndTime"].as_str().unwrap().replace(' ', "%20");
    let window = get(&r, &format!("/zm/api/events/index/MonitorId:{front}/EndTime%20%3E=:{from}.json"), &admin).await.json();
    let ids: Vec<String> = window["events"].as_array().unwrap().iter().map(|e| e["Event"]["Id"].as_str().unwrap().to_string()).collect();
    assert_eq!(ids.len(), 2, "events ending at or after e2's end: {ids:?}");
    assert!(!ids.contains(&e1.to_string()));

    // event detail: the Monitor next to the Event (neighborEvents reads Monitor.Id)
    let one = get(&r, &format!("/zm/api/events/{e2}.json"), &admin).await.json();
    assert_eq!(one["event"]["Monitor"]["Id"], front.to_string());
    assert_eq!(one["event"]["Event"]["StartTime"], one["event"]["Event"]["StartDateTime"]);
}

/// NVR.js refreshes with `GET host/login.json?token=<refresh>` and can log in
/// with `GET host/login.json?username=&password=`; getTimeZone reads `.tz`.
#[tokio::test]
async fn token_refresh_get_login_and_timezone() {
    let fx = Fixture::new();
    fx.add_admin("admin", "password123");
    let r = fx.router();
    let login = call(&r, "GET", "/zm/api/host/login.json?username=admin&password=password123", None, None).await;
    assert_eq!(login.status, 200);
    let login = login.json();
    let refresh = login["refresh_token"].as_str().unwrap().to_string();
    assert!(login["credentials"].as_str().unwrap().starts_with("token="));
    let renewed = call(&r, "GET", &format!("/zm/api/host/login.json?token={refresh}"), None, None).await.json();
    let access = renewed["access_token"].as_str().unwrap();
    assert_ne!(access, login["access_token"].as_str().unwrap());
    assert_eq!(get(&r, "/zm/api/monitors.json", access).await.status, 200);
    assert_eq!(call(&r, "GET", "/zm/api/host/login.json?token=nonsense", None, None).await.status, 401);
    assert_eq!(call(&r, "GET", "/zm/api/host/login.json?username=admin&password=wrong", None, None).await.status, 401);
    // no credentials: still the version probe it always was
    assert!(call(&r, "GET", "/zm/api/host/login.json", None, None).await.json()["version"].is_string());
    let tz = get(&r, "/zm/api/host/getTimeZone.json", access).await.json();
    assert!(tz["tz"].is_string(), "{tz}");
    assert_eq!(tz["tz"], tz["DateTime"]["TimeZone"]);
}

/// Classic zmNinja's montage opens one MJPEG stream per tile: the pool
/// size is configurable (23 cameras need more than the default 8).
#[test]
fn mjpeg_pool_size_is_configurable() {
    assert_eq!(zmng::config::Config::default().mjpeg_max_streams, 8);
    let c: zmng::config::Config = toml::from_str("mjpeg_max_streams = 32").unwrap();
    assert_eq!(c.mjpeg_max_streams, 32);
    let fx = Fixture::new();
    let mut cfg = fx.cfg.clone();
    cfg.mjpeg_max_streams = 32;
    let app = zmng::api::App::new(cfg, fx.db.clone(), fx.hub.clone(), fx.bus.clone());
    assert_eq!(app.mjpeg_sem.available_permits(), 32);
}
