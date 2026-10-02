//! Alerts: notifications to people's phones, decided by rules.
//!
//! The notification bus carries everything that happens (every event of
//! every camera); MQTT, the webhook (`alert_webhook`), SSE and the zmNinjaNg
//! event server pass all of it on. Alerts are the selective part: a rule
//! says which triggers count (people, vehicles, animals, any motion, a
//! camera that stopped recording, low storage), on which cameras or camera
//! groups and in which zones, when (always, or days and a time window that
//! may cross midnight), whether only while the system is *armed*, how often
//! at most per camera, and where to send it: an ntfy topic (phone push, with
//! the live picture attached and a link to the moment) or any webhook.
//!
//! Each event alerts a rule at most once: on the first notification about
//! it that matches (an event can become a "person" event a few seconds after
//! it started, when the detector answers).

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{info, warn};

use crate::db::Camera;
use crate::notify::{EventInfo, Notification};

pub const TRIGGERS: &[&str] = &["person", "vehicle", "animal", "motion", "camera_down", "storage_low"];
const VEHICLES: &[&str] = &["car", "truck", "bus", "motorcycle", "bicycle"];
const ANIMALS: &[&str] = &["dog", "cat", "bird", "horse", "sheep", "cow", "bear"];

/// Days (0 = Sunday .. 6 = Saturday) and a local time window "HH:MM"–"HH:MM";
/// a window whose end is before its start runs past midnight and counts for
/// the day it starts on.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Schedule {
    pub days: Vec<u8>,
    pub from: String,
    pub to: String,
}

fn hm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

impl Schedule {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(!self.days.is_empty() && self.days.iter().all(|d| *d < 7), "schedule: pick at least one day");
        anyhow::ensure!(hm(&self.from).is_some() && hm(&self.to).is_some(), "schedule: times are HH:MM");
        Ok(())
    }

    /// Whether local time `now` falls in the schedule.
    pub fn contains(&self, now: chrono::NaiveDateTime) -> bool {
        use chrono::{Datelike, Timelike};
        let (Some(a), Some(b)) = (hm(&self.from), hm(&self.to)) else { return false };
        let t = now.hour() * 60 + now.minute();
        let today = now.weekday().num_days_from_sunday() as u8;
        let yesterday = (today + 6) % 7;
        if a == b {
            return self.days.contains(&today); // all day
        }
        if a < b {
            self.days.contains(&today) && t >= a && t < b
        } else {
            (self.days.contains(&today) && t >= a) || (self.days.contains(&yesterday) && t < b)
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Rule {
    pub id: i64,
    pub name: String,
    pub enabled: bool,
    /// any of [`TRIGGERS`]
    pub triggers: Vec<String>,
    /// camera ids; with `groups` both empty = every camera
    pub cameras: Vec<i64>,
    pub groups: Vec<String>,
    /// zone names (case-insensitive); empty = anywhere
    pub zones: Vec<String>,
    /// None = always
    pub schedule: Option<Schedule>,
    pub only_when_armed: bool,
    /// at most one alert per camera this often (0 = every event)
    pub min_interval_secs: u32,
    /// "ntfy" or "webhook"
    pub target_kind: String,
    pub target_url: String,
    /// ntfy access token or webhook bearer token (never sent to the browser)
    pub target_token: String,
    /// attach the camera's live picture
    pub picture: bool,
}

impl Default for Rule {
    fn default() -> Self {
        Rule {
            id: 0, name: String::new(), enabled: true, triggers: vec!["person".into()], cameras: Vec::new(), groups: Vec::new(), zones: Vec::new(),
            schedule: None, only_when_armed: true, min_interval_secs: 60, target_kind: "ntfy".into(), target_url: String::new(), target_token: String::new(), picture: true,
        }
    }
}

impl Rule {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(!self.name.trim().is_empty() && self.name.chars().count() <= 80, "name: 1-80 characters");
        anyhow::ensure!(!self.triggers.is_empty(), "pick at least one trigger");
        if let Some(t) = self.triggers.iter().find(|t| !TRIGGERS.contains(&t.as_str())) {
            anyhow::bail!("unknown trigger {t:?} (one of {})", TRIGGERS.join(", "));
        }
        anyhow::ensure!(matches!(self.target_kind.as_str(), "ntfy" | "webhook"), "send to: ntfy or webhook");
        let u = url::Url::parse(self.target_url.trim()).map_err(|e| anyhow::anyhow!("send to: not a URL ({e})"))?;
        anyhow::ensure!(matches!(u.scheme(), "http" | "https") && u.host().is_some(), "send to: an http(s) URL");
        if self.target_kind == "ntfy" {
            anyhow::ensure!(u.path().trim_matches('/').split('/').count() == 1 && !u.path().trim_matches('/').is_empty(), "ntfy: the URL is the server and the topic, e.g. https://ntfy.sh/church-cameras-7f3a");
        }
        anyhow::ensure!(self.min_interval_secs <= 86_400, "minimum interval: at most a day");
        if let Some(s) = &self.schedule {
            s.validate()?;
        }
        Ok(())
    }

    /// Cameras in scope: the listed ones plus every camera in a listed group.
    fn covers(&self, camera_id: i64, cams: &[Camera]) -> bool {
        if self.cameras.is_empty() && self.groups.is_empty() {
            return true;
        }
        self.cameras.contains(&camera_id)
            || cams.iter().find(|c| c.id == camera_id).is_some_and(|c| c.tags.split(',').map(str::trim).any(|t| self.groups.iter().any(|g| g.eq_ignore_ascii_case(t))))
    }
}

/// What to send.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Alert {
    pub rule_id: i64,
    /// the trigger that matched
    pub trigger: String,
    pub title: String,
    pub message: String,
    /// ntfy tags (emoji shortcodes)
    pub tags: Vec<String>,
    /// ntfy priority 1..5
    pub priority: u8,
    pub camera_id: Option<i64>,
    pub event_id: Option<i64>,
    /// UI path to open (`#/camera/<id>/<ms>`)
    pub open: Option<String>,
    pub event: Option<EventInfo>,
}

fn class_of(e: &EventInfo) -> Vec<&'static str> {
    let labels: Vec<String> = std::iter::once(e.kind.to_lowercase()).chain(e.objects.iter().map(|o| o.label.to_lowercase())).collect();
    let mut out = Vec::new();
    if labels.iter().any(|l| l == "person") {
        out.push("person");
    }
    if labels.iter().any(|l| VEHICLES.contains(&l.as_str())) {
        out.push("vehicle");
    }
    if labels.iter().any(|l| ANIMALS.contains(&l.as_str())) {
        out.push("animal");
    }
    out.push("motion");
    out
}

/// Decides which rules fire for a notification. Remembers which events
/// already alerted each rule and when each rule last alerted per camera.
#[derive(Default)]
pub struct Engine {
    sent: HashMap<(i64, i64), i64>,
    last: HashMap<(i64, i64), i64>,
}

impl Engine {
    /// Alerts for `n` at local time `now` (`now_ms` = the same instant).
    pub fn decide(&mut self, rules: &[Rule], cams: &[Camera], armed: bool, now: chrono::NaiveDateTime, now_ms: i64, n: &Notification) -> Vec<Alert> {
        // forget dedup entries older than a day
        self.sent.retain(|_, t| now_ms - *t < 86_400_000);
        let mut out = Vec::new();
        for r in rules.iter().filter(|r| r.enabled) {
            if r.only_when_armed && !armed {
                continue;
            }
            if r.schedule.as_ref().is_some_and(|s| !s.contains(now)) {
                continue;
            }
            match n {
                Notification::EventStart(e) | Notification::EventUpdate(e) | Notification::EventEnd(e) => {
                    if !r.covers(e.camera_id, cams) || self.sent.contains_key(&(r.id, e.id)) {
                        continue;
                    }
                    let classes = class_of(e);
                    let Some(trigger) = r.triggers.iter().find(|t| classes.contains(&t.as_str())) else { continue };
                    if !r.zones.is_empty() && !e.zones.iter().any(|z| r.zones.iter().any(|w| w.eq_ignore_ascii_case(z))) {
                        continue;
                    }
                    self.sent.insert((r.id, e.id), now_ms);
                    if !self.throttle_ok(r, e.camera_id, now_ms) {
                        continue;
                    }
                    let what = match trigger.as_str() {
                        "motion" => e.objects.first().map(|o| o.label.clone()).unwrap_or_else(|| "motion".into()),
                        t => e.objects.iter().find(|o| class_of(&EventInfo { kind: o.label.clone(), objects: vec![], ..e.clone() }).contains(&t)).map(|o| o.label.clone()).unwrap_or_else(|| t.to_string()),
                    };
                    let at = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(e.start_ms).unwrap_or_default().with_timezone(&chrono::Local).format("%-I:%M:%S %p");
                    let mut details = vec![at.to_string()];
                    if !e.zones.is_empty() {
                        details.push(format!("zone {}", e.zones.join(", ")));
                    }
                    for o in &e.objects {
                        details.push(format!("{} {:.0}%", o.label, o.confidence * 100.0));
                    }
                    out.push(Alert {
                        rule_id: r.id, trigger: trigger.clone(), title: format!("{}: {what}", e.camera_name),
                        message: format!("{} · {}", e.camera_name, details.join(" · ")),
                        tags: vec![match trigger.as_str() { "person" => "bust_in_silhouette", "vehicle" => "car", "animal" => "dog", _ => "eyes" }.into()],
                        priority: if matches!(trigger.as_str(), "person" | "vehicle") { 4 } else { 3 },
                        camera_id: Some(e.camera_id), event_id: Some(e.id), open: Some(format!("#/camera/{}/{}", e.camera_id, e.start_ms)), event: Some(e.clone()),
                    });
                }
                Notification::CameraDown { camera_id, name } if r.triggers.iter().any(|t| t == "camera_down") => {
                    if !r.covers(*camera_id, cams) || !self.throttle_ok(r, *camera_id, now_ms) {
                        continue;
                    }
                    out.push(Alert {
                        rule_id: r.id, trigger: "camera_down".into(), title: format!("{name} is not recording"),
                        message: format!("{name} stopped recording at {}. Status shows why.", now.format("%-I:%M %p")),
                        tags: vec!["warning".into()], priority: 4, camera_id: Some(*camera_id), event_id: None, open: Some("#/status".into()), event: None,
                    });
                }
                Notification::StorageLow { path, free_bytes, .. } if r.triggers.iter().any(|t| t == "storage_low") => {
                    if !self.throttle_ok(r, -1, now_ms) {
                        continue;
                    }
                    out.push(Alert {
                        rule_id: r.id, trigger: "storage_low".into(), title: "Recording storage is low".into(),
                        message: format!("{path}: {:.1} GB free", *free_bytes as f64 / 1e9),
                        tags: vec!["floppy_disk".into()], priority: 4, camera_id: None, event_id: None, open: Some("#/status".into()), event: None,
                    });
                }
                _ => {}
            }
        }
        out
    }

    fn throttle_ok(&mut self, r: &Rule, camera: i64, now_ms: i64) -> bool {
        let key = (r.id, camera);
        if self.last.get(&key).is_some_and(|t| now_ms - t < r.min_interval_secs as i64 * 1000) {
            return false;
        }
        self.last.insert(key, now_ms);
        true
    }
}

/// RFC 2047 encoding for header values that are not plain ASCII (ntfy reads
/// it; HTTP headers cannot carry UTF-8 otherwise).
fn header_text(s: &str) -> String {
    let s: String = s.chars().filter(|c| !c.is_control()).collect();
    if s.is_ascii() {
        return s;
    }
    use base64::Engine as _;
    format!("=?UTF-8?B?{}?=", base64::engine::general_purpose::STANDARD.encode(s.as_bytes()))
}

/// Send one alert. `picture` is a JPEG to attach; `public_url` makes the
/// tap/click link absolute.
pub async fn send(http: &reqwest::Client, rule: &Rule, a: &Alert, picture: Option<bytes::Bytes>, public_url: Option<&str>) -> Result<()> {
    let link = match (public_url, &a.open) {
        (Some(base), Some(path)) => Some(format!("{}/{}", base.trim_end_matches('/'), path)),
        _ => None,
    };
    let auth = (!rule.target_token.trim().is_empty()).then(|| format!("Bearer {}", rule.target_token.trim()));
    let req = match rule.target_kind.as_str() {
        "ntfy" => {
            let mut req = match &picture {
                // the picture is the body; the text travels in headers
                Some(jpeg) => http.put(&rule.target_url).header("Filename", "camera.jpg").header("Message", header_text(&a.message)).body(jpeg.clone()),
                None => http.post(&rule.target_url).body(a.message.clone()),
            };
            req = req.header("Title", header_text(&a.title)).header("Tags", a.tags.join(",")).header("Priority", a.priority.to_string());
            if let Some(l) = &link {
                req = req.header("Click", l.as_str());
            }
            req
        }
        _ => {
            use base64::Engine as _;
            let body = serde_json::json!({
                "alert": a.trigger, "rule": rule.name, "title": a.title, "message": a.message, "camera_id": a.camera_id, "event_id": a.event_id,
                "event": a.event, "url": link, "picture_jpeg_base64": picture.map(|p| base64::engine::general_purpose::STANDARD.encode(&p)),
            });
            http.post(&rule.target_url).json(&body)
        }
    };
    let req = match &auth {
        Some(v) => req.header("Authorization", v.as_str()),
        None => req,
    };
    let res = req.timeout(std::time::Duration::from_secs(15)).send().await?;
    anyhow::ensure!(res.status().is_success(), "{} answered {}", rule.target_kind, res.status());
    Ok(())
}

/// Run the alert engine until the bus closes: decide, then deliver each
/// alert on its own task (a slow destination never holds up the others),
/// retrying once after 5 s, and record the outcome on the rule.
pub async fn run(app: crate::api::App, mut rx: tokio::sync::broadcast::Receiver<Notification>) {
    let mut engine = Engine::default();
    loop {
        let n = match rx.recv().await {
            Ok(n) => n,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(k)) => {
                warn!(skipped = k, "alert engine lagged");
                continue;
            }
            Err(_) => return,
        };
        if !matches!(n, Notification::EventStart(_) | Notification::EventUpdate(_) | Notification::EventEnd(_) | Notification::CameraDown { .. } | Notification::StorageLow { .. }) {
            continue;
        }
        let rules: Vec<Rule> = app.db.alert_rules().unwrap_or_default().into_iter().map(|(r, _, _)| r).filter(|r| r.enabled).collect();
        if rules.is_empty() {
            continue;
        }
        let cams = app.db.cameras().unwrap_or_default();
        let now = chrono::Local::now();
        let alerts = engine.decide(&rules, &cams, app.db.armed().unwrap_or(true), now.naive_local(), now.timestamp_millis(), &n);
        for a in alerts {
            let Some(rule) = rules.iter().find(|r| r.id == a.rule_id).cloned() else { continue };
            let app = app.clone();
            tokio::spawn(async move { deliver(&app, &rule, &a).await });
        }
    }
}

/// Picture, send (one retry), record the result on the rule.
pub async fn deliver(app: &crate::api::App, rule: &Rule, a: &Alert) -> Result<()> {
    let picture = match (rule.picture, a.camera_id) {
        (true, Some(cam)) => crate::api::live_jpeg(app, cam, Some(640), false).await.ok().map(|(j, _)| j),
        _ => None,
    };
    let mut res = send(&app.http, rule, a, picture.clone(), app.cfg.public_url.as_deref()).await;
    if res.is_err() {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        res = send(&app.http, rule, a, picture, app.cfg.public_url.as_deref()).await;
    }
    let now = crate::db::now_dts() / 90;
    match &res {
        Ok(()) => {
            info!(rule = %rule.name, title = %a.title, "alert sent");
            let _ = app.db.set_alert_result(rule.id, now, None);
        }
        Err(e) => {
            warn!(rule = %rule.name, "alert failed: {e:#}");
            let _ = app.db.set_alert_result(rule.id, now, Some(&format!("{e:#}")));
        }
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::DetectedObject;

    fn ev(id: i64, cam: i64, objects: &[&str], zones: &[&str]) -> EventInfo {
        EventInfo {
            id, camera_id: cam, camera_name: format!("cam{cam}"), start_ms: 1_800_000_000_000, end_ms: None, score: 100, kind: "motion".into(), thumb: None,
            objects: objects.iter().map(|l| DetectedObject { label: l.to_string(), confidence: 0.9, bbox: [0.0; 4] }).collect(), zones: zones.iter().map(|z| z.to_string()).collect(),
        }
    }
    fn at(h: u32, m: u32, weekday_offset: u32) -> chrono::NaiveDateTime {
        // 2026-10-04 is a Sunday
        chrono::NaiveDate::from_ymd_opt(2026, 10, 4 + weekday_offset).unwrap().and_hms_opt(h, m, 0).unwrap()
    }
    fn rule(id: i64, triggers: &[&str]) -> Rule {
        Rule { id, name: format!("r{id}"), triggers: triggers.iter().map(|t| t.to_string()).collect(), target_url: "https://ntfy.sh/x".into(), min_interval_secs: 0, ..Rule::default() }
    }
    fn cams() -> Vec<Camera> {
        let mut a = Camera::example();
        a.id = 1;
        a.tags = "Outside, Doors".into();
        let mut b = Camera::example();
        b.id = 2;
        b.tags = "Inside".into();
        vec![a, b]
    }

    #[test]
    fn schedules_cross_midnight_and_pick_days() {
        let night = Schedule { days: vec![0, 1, 2, 3, 4, 5, 6], from: "22:00".into(), to: "06:00".into() };
        assert!(night.contains(at(23, 0, 0)));
        assert!(night.contains(at(5, 59, 1)));
        assert!(!night.contains(at(6, 0, 1)));
        assert!(!night.contains(at(12, 0, 1)));
        // Sunday services: Sundays 7:00-13:00 only
        let sunday = Schedule { days: vec![0], from: "07:00".into(), to: "13:00".into() };
        assert!(sunday.contains(at(9, 30, 0)));
        assert!(!sunday.contains(at(9, 30, 1)));
        // Saturday night past midnight counts for Saturday
        let sat_night = Schedule { days: vec![6], from: "22:00".into(), to: "02:00".into() };
        assert!(sat_night.contains(at(1, 0, 0)), "Sunday 1 AM is Saturday night");
        assert!(!sat_night.contains(at(23, 0, 0)), "Sunday 11 PM is not");
        assert!(Schedule { days: vec![], from: "1:00".into(), to: "2:00".into() }.validate().is_err());
        assert!(Schedule { days: vec![1], from: "25:00".into(), to: "2:00".into() }.validate().is_err());
    }

    #[test]
    fn rules_match_triggers_cameras_groups_zones_and_alert_once_per_event() {
        let mut e = Engine::default();
        let now = at(12, 0, 1);
        let people_doors = Rule { groups: vec!["doors".into()], zones: vec!["Porch".into()], ..rule(1, &["person"]) };
        let any_inside = Rule { cameras: vec![2], ..rule(2, &["motion"]) };
        let rules = vec![people_doors, any_inside];
        // motion only on camera 1: nothing (rule 1 wants a person)
        assert!(e.decide(&rules, &cams(), true, now, 0, &Notification::EventStart(ev(10, 1, &[], &["Porch"]))).is_empty());
        // the same event becomes a person event in the porch: rule 1 fires, once
        let a = e.decide(&rules, &cams(), true, now, 1, &Notification::EventUpdate(ev(10, 1, &["person", "car"], &["Porch"])));
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].rule_id, 1);
        assert_eq!(a[0].title, "cam1: person");
        assert!(a[0].message.contains("zone Porch") && a[0].message.contains("person 90%"), "{}", a[0].message);
        assert_eq!(a[0].open.as_deref(), Some("#/camera/1/1800000000000"));
        assert!(e.decide(&rules, &cams(), true, now, 2, &Notification::EventEnd(ev(10, 1, &["person"], &["Porch"]))).is_empty(), "once per event");
        // a person elsewhere (zone Lawn) does not count for rule 1
        assert!(e.decide(&rules, &cams(), true, now, 3, &Notification::EventUpdate(ev(11, 1, &["person"], &["Lawn"]))).is_empty());
        // camera 2 is not in "Doors"; any motion there fires rule 2
        let a = e.decide(&rules, &cams(), true, now, 4, &Notification::EventStart(ev(12, 2, &[], &[])));
        assert_eq!(a.iter().map(|a| a.rule_id).collect::<Vec<_>>(), vec![2]);
        assert_eq!(a[0].title, "cam2: motion");
        // a vehicle rule names the vehicle
        let v = vec![rule(3, &["vehicle"])];
        let a = e.decide(&v, &cams(), true, now, 5, &Notification::EventUpdate(ev(13, 1, &["person", "truck"], &[])));
        assert_eq!(a[0].title, "cam1: truck");
    }

    #[test]
    fn armed_schedule_and_throttle() {
        let mut e = Engine::default();
        let night = Rule { schedule: Some(Schedule { days: (0..7).collect(), from: "22:00".into(), to: "06:00".into() }), min_interval_secs: 300, ..rule(1, &["motion"]) };
        let rules = vec![night.clone()];
        let s = |id| Notification::EventStart(ev(id, 1, &[], &[]));
        assert!(e.decide(&rules, &cams(), true, at(12, 0, 1), 0, &s(1)).is_empty(), "outside the schedule");
        assert!(e.decide(&rules, &cams(), false, at(23, 0, 1), 0, &s(2)).is_empty(), "disarmed");
        assert_eq!(e.decide(&rules, &cams(), true, at(23, 0, 1), 0, &s(3)).len(), 1);
        assert!(e.decide(&rules, &cams(), true, at(23, 1, 1), 60_000, &s(4)).is_empty(), "within 5 minutes of the last");
        assert_eq!(e.decide(&rules, &cams(), true, at(23, 6, 1), 360_000, &s(5)).len(), 1);
        // a rule that ignores arming fires while disarmed
        let always = vec![Rule { only_when_armed: false, ..rule(9, &["camera_down", "storage_low"]) }];
        let a = e.decide(&always, &cams(), false, at(12, 0, 1), 0, &Notification::CameraDown { camera_id: 2, name: "Hall".into() });
        assert_eq!(a[0].title, "Hall is not recording");
        let a = e.decide(&always, &cams(), false, at(12, 0, 1), 0, &Notification::StorageLow { storage_id: 1, path: "/raid".into(), free_bytes: 5_000_000_000, reserve_bytes: 0 });
        assert_eq!(a[0].message, "/raid: 5.0 GB free");
        // disabled rules never fire
        let off = vec![Rule { enabled: false, ..rule(4, &["motion"]) }];
        assert!(e.decide(&off, &cams(), true, at(12, 0, 1), 0, &s(6)).is_empty());
    }

    #[test]
    fn rule_validation_and_header_encoding() {
        assert!(rule(1, &["person"]).validate().is_ok());
        assert!(Rule { triggers: vec![], ..rule(1, &["person"]) }.validate().is_err());
        assert!(Rule { triggers: vec!["ghost".into()], ..rule(1, &["person"]) }.validate().is_err());
        assert!(Rule { target_url: "ntfy.sh/x".into(), ..rule(1, &["person"]) }.validate().is_err());
        assert!(Rule { target_url: "https://ntfy.sh/".into(), ..rule(1, &["person"]) }.validate().is_err(), "ntfy needs a topic");
        assert!(Rule { target_kind: "webhook".into(), target_url: "https://ha.local/api/webhook/abc".into(), ..rule(1, &["person"]) }.validate().is_ok());
        assert_eq!(header_text("Front Door: person"), "Front Door: person");
        assert_eq!(header_text("Küche: person"), "=?UTF-8?B?S8O8Y2hlOiBwZXJzb24=?=");
    }
}
