//! Named zones through the API: stored only when valid, shown to
//! zmNinjaNg by name, recorded on events and usable as an events filter.
mod common;
use common::*;
use serde_json::json;
use zmng::mp4::TIMESCALE;

const T0: i64 = 1_800_000_000;
fn dts(secs: i64) -> i64 {
    secs * TIMESCALE as i64
}

#[tokio::test]
async fn named_zones_are_validated_recorded_and_filterable() {
    let fx = Fixture::new();
    let cam = fx.add_camera("front");
    let admin = fx.token_for(fx.add_admin("admin", "password123"));
    let r = fx.router();
    let zones = json!([
        {"name": "Door", "points": [[0.1, 0.2], [0.4, 0.2], [0.4, 0.9]], "min_area_pct": 0.3, "min_blob_pct": 0.1},
        {"name": "Road", "points": [[0.5, 0.0], [1.0, 0.0], [1.0, 0.4]]}
    ]);
    let masks = json!([{"name": "Tree", "points": [[0.8, 0.5], [1.0, 0.5], [1.0, 1.0]]}]);
    let res = call(&r, "PATCH", &format!("/api/cameras/{cam}"), Some(&admin), Some(json!({"zones_json": zones.to_string(), "masks_json": masks.to_string()}))).await;
    assert_eq!(res.status, 200, "{}", res.text());
    let c = get(&r, &format!("/api/cameras/{cam}"), &admin).await.json();
    assert_eq!(serde_json::from_str::<serde_json::Value>(c["zones_json"].as_str().unwrap()).unwrap()[0]["name"], "Door");
    // invalid zones are refused with a reason, and nothing is stored
    for bad in [
        json!([{"name": "x", "points": [[0, 0], [1, 0]]}]).to_string(),
        json!([{"name": "Door", "points": [[0, 0], [1, 0], [1, 1]]}, {"name": "door", "points": [[0, 0], [1, 0], [1, 1]]}]).to_string(),
        "garbage".to_string(),
    ] {
        let res = call(&r, "PATCH", &format!("/api/cameras/{cam}"), Some(&admin), Some(json!({"zones_json": bad}))).await;
        assert_eq!(res.status, 400, "{bad}");
    }
    let c = get(&r, &format!("/api/cameras/{cam}"), &admin).await.json();
    assert!(c["zones_json"].as_str().unwrap().contains("Road"));
    // zmNinjaNg sees the names
    let z = get(&r, &format!("/zm/api/zones.json?MonitorId={cam}"), &admin).await.json();
    let names: Vec<&str> = z["zones"].as_array().unwrap().iter().map(|r| r["Zone"]["Name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Door", "Road", "Tree"]);
    assert_eq!(z["zones"][2]["Zone"]["Type"], "Inactive");
    // events carry the zones that fired; ?zone= filters (case-insensitive, comma list)
    let close = |at: i64, zones: serde_json::Value| {
        let id = fx.db.insert_event(cam, dts(T0 + at), "motion").unwrap();
        fx.db.close_event(id, dts(T0 + at + 5), None, &json!({"zones": zones, "motion_frames": 10}).to_string()).unwrap();
        id
    };
    let door = close(0, json!(["Door"]));
    let both = close(10, json!(["Road", "Door"]));
    let road = close(20, json!(["Road"]));
    let none = close(30, json!([]));
    let ids = |path: String| {
        let r = r.clone();
        let admin = admin.clone();
        async move { get(&r, &path, &admin).await.json().as_array().unwrap().iter().map(|e| e["id"].as_i64().unwrap()).collect::<Vec<_>>() }
    };
    assert_eq!(ids("/api/events?zone=door&order=asc".into()).await, vec![door, both]);
    assert_eq!(ids("/api/events?zone=Road&order=asc".into()).await, vec![both, road]);
    assert_eq!(ids("/api/events?zone=Door,Road&order=asc".into()).await, vec![door, both, road]);
    assert_eq!(ids("/api/events?order=asc".into()).await, vec![door, both, road, none]);
    let ev = get(&r, &format!("/api/events/{both}"), &admin).await.json();
    assert_eq!(ev["zones"], json!(["Road", "Door"]));
}
