//! Notification bus and delivery sinks.
//!
//! Everything that happens in the recorder/detector that a human or a home
//! automation system might care about is published once on an in-process
//! broadcast bus as a [`Notification`]. Independent sinks subscribe:
//!
//! * `webhook`: JSON POST of every notification to `alert_webhook`
//!   (ntfy, Home Assistant webhook trigger, n8n, ...);
//! * `mqtt`: per-camera motion/recording topics plus Home Assistant MQTT
//!   discovery, so every camera shows up in HA as a device with a motion
//!   binary sensor, a recording sensor, a last-event sensor and a camera
//!   entity showing the latest event thumbnail;
//! * the HTTP API: `GET /api/events/stream` (SSE for the UI) and the
//!   zmNinja event-server websocket (`/zm/ws`).
//!
//! A slow or dead sink never blocks the detector: the bus is a bounded
//! broadcast channel and lagging receivers simply skip.

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::db::{Camera, Db, Event};
use crate::video::dts_to_ms;

/// One detected object (phase 3 object detection); empty for plain motion.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DetectedObject {
    pub label: String,
    /// 0..1
    pub confidence: f64,
    /// normalized [x0, y0, x1, y1]
    pub bbox: [f64; 4],
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EventInfo {
    pub id: i64,
    pub camera_id: i64,
    pub camera_name: String,
    pub start_ms: i64,
    pub end_ms: Option<i64>,
    pub score: u8,
    pub kind: String,
    /// relative URL of the thumbnail once it exists
    pub thumb: Option<String>,
    #[serde(default)]
    pub objects: Vec<DetectedObject>,
}

impl EventInfo {
    pub fn from_event(e: &Event, cam: &Camera) -> EventInfo {
        EventInfo {
            id: e.id,
            camera_id: cam.id,
            camera_name: cam.name.clone(),
            start_ms: dts_to_ms(e.start_dts),
            end_ms: e.end_dts.map(dts_to_ms),
            score: e.score,
            kind: e.kind.clone(),
            thumb: e.thumb_path.as_ref().map(|_| format!("/api/events/{}/thumb.jpg", e.id)),
            objects: Vec::new(),
        }
    }
}

/// Serialized as `{"event": "<snake_case variant>", ...fields}` so webhook
/// consumers can switch on one key. `camera_down`/`camera_up` keep the shape
/// the phase-1 webhook already sent.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Notification {
    ServiceStarted { version: String },
    CameraDown { camera_id: i64, name: String },
    CameraUp { camera_id: i64, name: String },
    EventStart(EventInfo),
    /// Score, kind or detected objects changed while the event is open.
    EventUpdate(EventInfo),
    EventEnd(EventInfo),
    /// The thumbnail JPEG of an event is ready (MQTT camera entity).
    #[serde(skip)]
    EventThumbnail { event_id: i64, camera_id: i64, jpeg: Bytes },
    StorageLow { storage_id: i64, path: String, free_bytes: u64, reserve_bytes: u64 },
}

impl Notification {
    pub fn camera_id(&self) -> Option<i64> {
        match self {
            Notification::CameraDown { camera_id, .. } | Notification::CameraUp { camera_id, .. } => Some(*camera_id),
            Notification::EventStart(e) | Notification::EventUpdate(e) | Notification::EventEnd(e) => Some(e.camera_id),
            Notification::EventThumbnail { camera_id, .. } => Some(*camera_id),
            Notification::ServiceStarted { .. } | Notification::StorageLow { .. } => None,
        }
    }
    /// Short snake_case name of the variant (the `event` key).
    pub fn name(&self) -> &'static str {
        match self {
            Notification::ServiceStarted { .. } => "service_started",
            Notification::CameraDown { .. } => "camera_down",
            Notification::CameraUp { .. } => "camera_up",
            Notification::EventStart(_) => "event_start",
            Notification::EventUpdate(_) => "event_update",
            Notification::EventEnd(_) => "event_end",
            Notification::EventThumbnail { .. } => "event_thumbnail",
            Notification::StorageLow { .. } => "storage_low",
        }
    }
    /// True for notifications that carry a JSON body worth delivering
    /// externally (thumbnail bytes are MQTT-only).
    pub fn is_external(&self) -> bool {
        !matches!(self, Notification::EventThumbnail { .. })
    }
}

pub struct Bus {
    tx: broadcast::Sender<Notification>,
}

impl Default for Bus {
    fn default() -> Self {
        Self::new()
    }
}

impl Bus {
    pub fn new() -> Bus {
        let (tx, _) = broadcast::channel(256);
        Bus { tx }
    }
    pub fn publish(&self, n: Notification) {
        debug!(event = n.name(), camera = ?n.camera_id(), "notify");
        let _ = self.tx.send(n);
    }
    pub fn subscribe(&self) -> broadcast::Receiver<Notification> {
        self.tx.subscribe()
    }
    /// Convenience: look the event and camera up and publish.
    pub fn publish_event(&self, db: &Db, event_id: i64, what: fn(EventInfo) -> Notification) {
        let Ok(Some(e)) = db.event(event_id) else { return };
        let Ok(Some(cam)) = db.camera(e.camera_id) else { return };
        let mut info = EventInfo::from_event(&e, &cam);
        info.objects = objects_from_meta(&e);
        self.publish(what(info));
    }
}

/// Detected objects stored in `event.meta_json` (`{"objects": [...]}`).
pub fn objects_from_meta(e: &Event) -> Vec<DetectedObject> {
    serde_json::from_str::<serde_json::Value>(&e.meta_json)
        .ok()
        .and_then(|m| m.get("objects").cloned())
        .and_then(|o| serde_json::from_value(o).ok())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// webhook sink
// ---------------------------------------------------------------------------

/// POST every external notification as JSON to `url` until the bus closes.
pub async fn webhook_sink(bus: Arc<Bus>, url: String) {
    let client = match reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build() {
        Ok(c) => c,
        Err(e) => {
            warn!("webhook client: {e}");
            return;
        }
    };
    let mut rx = bus.subscribe();
    loop {
        let n = match rx.recv().await {
            Ok(n) => n,
            Err(broadcast::error::RecvError::Lagged(k)) => {
                warn!(skipped = k, "webhook sink lagged");
                continue;
            }
            Err(_) => return,
        };
        if !n.is_external() {
            continue;
        }
        match client.post(&url).json(&n).send().await {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => warn!(status = %r.status(), event = n.name(), "webhook rejected"),
            Err(e) => warn!(event = n.name(), "webhook failed: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// MQTT sink with Home Assistant discovery
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct MqttConfig {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    /// Topic prefix for state topics (`<prefix>/camera/<id>/motion`, ...).
    pub topic_prefix: String,
    /// Home Assistant discovery prefix; empty disables discovery.
    pub discovery_prefix: String,
    pub client_id: String,
}

impl Default for MqttConfig {
    fn default() -> Self {
        MqttConfig {
            host: "localhost".into(),
            port: 1883,
            username: None,
            password: None,
            topic_prefix: "zmng".into(),
            discovery_prefix: "homeassistant".into(),
            client_id: "zmng".into(),
        }
    }
}

/// Topics and payloads, pure so they can be unit-tested without a broker.
pub struct MqttTopics<'a> {
    pub cfg: &'a MqttConfig,
}

impl<'a> MqttTopics<'a> {
    pub fn status(&self) -> String {
        format!("{}/status", self.cfg.topic_prefix)
    }
    pub fn camera(&self, id: i64, leaf: &str) -> String {
        format!("{}/camera/{id}/{leaf}", self.cfg.topic_prefix)
    }
    /// Home Assistant discovery messages for one camera: (topic, retained JSON).
    pub fn discovery(&self, cam: &Camera) -> Vec<(String, serde_json::Value)> {
        if self.cfg.discovery_prefix.is_empty() {
            return Vec::new();
        }
        let dp = &self.cfg.discovery_prefix;
        let device = serde_json::json!({
            "identifiers": [format!("zmng_camera_{}", cam.id)],
            "name": cam.name,
            "manufacturer": "ZoneMinder NG",
            "model": "camera",
        });
        let avail = serde_json::json!([{"topic": self.status()}]);
        let uid = |leaf: &str| format!("zmng_{}_{leaf}", cam.id);
        vec![
            (
                format!("{dp}/binary_sensor/{}/config", uid("motion")),
                serde_json::json!({
                    "name": "Motion", "unique_id": uid("motion"), "device_class": "motion",
                    "state_topic": self.camera(cam.id, "motion"), "payload_on": "ON", "payload_off": "OFF",
                    "availability": avail, "device": device,
                }),
            ),
            (
                format!("{dp}/binary_sensor/{}/config", uid("recording")),
                serde_json::json!({
                    "name": "Recording", "unique_id": uid("recording"), "device_class": "running",
                    "state_topic": self.camera(cam.id, "recording"), "payload_on": "ON", "payload_off": "OFF",
                    "availability": avail, "device": device,
                }),
            ),
            (
                format!("{dp}/sensor/{}/config", uid("last_event")),
                serde_json::json!({
                    "name": "Last event", "unique_id": uid("last_event"),
                    "state_topic": self.camera(cam.id, "event"), "value_template": "{{ value_json.kind }}",
                    "json_attributes_topic": self.camera(cam.id, "event"),
                    "availability": avail, "device": device,
                }),
            ),
            (
                format!("{dp}/camera/{}/config", uid("thumbnail")),
                serde_json::json!({
                    "name": "Last event thumbnail", "unique_id": uid("thumbnail"),
                    "topic": self.camera(cam.id, "thumbnail"),
                    "availability": avail, "device": device,
                }),
            ),
        ]
    }
}

/// What the MQTT sink publishes for one notification: (topic, retain, payload).
pub fn mqtt_messages(cfg: &MqttConfig, n: &Notification) -> Vec<(String, bool, Vec<u8>)> {
    let t = MqttTopics { cfg };
    let json = |v: &Notification| serde_json::to_vec(v).unwrap_or_default();
    match n {
        Notification::ServiceStarted { .. } => vec![(t.status(), true, b"online".to_vec())],
        Notification::CameraDown { camera_id, .. } => vec![(t.camera(*camera_id, "recording"), true, b"OFF".to_vec())],
        Notification::CameraUp { camera_id, .. } => vec![(t.camera(*camera_id, "recording"), true, b"ON".to_vec())],
        Notification::EventStart(e) => vec![
            (t.camera(e.camera_id, "motion"), true, b"ON".to_vec()),
            (t.camera(e.camera_id, "event"), true, json(n)),
        ],
        Notification::EventUpdate(e) => vec![(t.camera(e.camera_id, "event"), true, json(n))],
        Notification::EventEnd(e) => vec![
            (t.camera(e.camera_id, "motion"), true, b"OFF".to_vec()),
            (t.camera(e.camera_id, "event"), true, json(n)),
        ],
        Notification::EventThumbnail { camera_id, jpeg, .. } => vec![(t.camera(*camera_id, "thumbnail"), true, jpeg.to_vec())],
        Notification::StorageLow { .. } => vec![(format!("{}/storage", cfg.topic_prefix), false, json(n))],
    }
}

/// Run the MQTT client until the bus closes. Reconnects with backoff; on
/// every (re)connect it republishes availability, discovery for every camera
/// and the current recording state.
pub async fn mqtt_sink(bus: Arc<Bus>, db: Db, cfg: MqttConfig, hub: Arc<crate::recorder::LiveHub>) {
    use rumqttc::{AsyncClient, Event as MEvent, Incoming, LastWill, MqttOptions, QoS};
    let t = MqttTopics { cfg: &cfg };
    let mut opts = MqttOptions::new(cfg.client_id.clone(), cfg.host.clone(), cfg.port);
    opts.set_keep_alive(std::time::Duration::from_secs(30));
    if let Some(u) = &cfg.username {
        opts.set_credentials(u.clone(), cfg.password.clone().unwrap_or_default());
    }
    opts.set_last_will(LastWill::new(t.status(), "offline", QoS::AtLeastOnce, true));
    let (client, mut eventloop) = AsyncClient::new(opts, 64);
    let mut rx = bus.subscribe();
    let publish = |client: AsyncClient, msgs: Vec<(String, bool, Vec<u8>)>| async move {
        for (topic, retain, payload) in msgs {
            if let Err(e) = client.publish(topic, QoS::AtLeastOnce, retain, payload).await {
                warn!("mqtt publish: {e}");
            }
        }
    };
    loop {
        tokio::select! {
            ev = eventloop.poll() => match ev {
                Ok(MEvent::Incoming(Incoming::ConnAck(_))) => {
                    info!(host = %cfg.host, port = cfg.port, "mqtt connected");
                    let mut msgs = vec![(t.status(), true, b"online".to_vec())];
                    for cam in db.cameras().unwrap_or_default().into_iter().filter(|c| c.enabled) {
                        for (topic, v) in t.discovery(&cam) {
                            msgs.push((topic, true, serde_json::to_vec(&v).unwrap_or_default()));
                        }
                        let up = hub.get(cam.id).map(|h| h.status.read().connected).unwrap_or(false);
                        msgs.push((t.camera(cam.id, "recording"), true, if up { b"ON".to_vec() } else { b"OFF".to_vec() }));
                        msgs.push((t.camera(cam.id, "motion"), true, b"OFF".to_vec()));
                    }
                    publish(client.clone(), msgs).await;
                }
                Ok(_) => {}
                Err(e) => {
                    warn!("mqtt: {e}; reconnecting in 5 s");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            },
            n = rx.recv() => match n {
                Ok(n) => publish(client.clone(), mqtt_messages(&cfg, &n)).await,
                Err(broadcast::error::RecvError::Lagged(k)) => warn!(skipped = k, "mqtt sink lagged"),
                Err(_) => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cam() -> Camera {
        Camera {
            id: 7, name: "Front".into(), main_url: "rtsp://x".into(), sub_url: None, enabled: true, storage_id: 1,
            retention_days: 1.0, detect_fps: 5.0, detect_width: 320, detect_height: 180, pixel_threshold: 25,
            min_area_pct: 1.0, min_blob_pct: 0.5, pre_secs: 5.0, post_secs: 8.0, cooldown_secs: 10.0,
            zones_json: "[]".into(), masks_json: "[]".into(), sort_order: 0, record_sub: true,
            event_retention_days: 0.0, tags: String::new(), objects: true, object_labels: String::new(), require_object: false,
        }
    }

    #[test]
    fn notification_json_has_an_event_key() {
        let n = Notification::CameraDown { camera_id: 3, name: "Back".into() };
        let v = serde_json::to_value(&n).unwrap();
        assert_eq!(v, serde_json::json!({"event": "camera_down", "camera_id": 3, "name": "Back"}));
        let e = Notification::EventStart(EventInfo { id: 1, camera_id: 7, camera_name: "Front".into(), start_ms: 5, end_ms: None, score: 9, kind: "motion".into(), thumb: None, objects: vec![] });
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["event"], "event_start");
        assert_eq!(v["camera_name"], "Front");
        let back: Notification = serde_json::from_value(v).unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn mqtt_topics_and_discovery() {
        let cfg = MqttConfig::default();
        let t = MqttTopics { cfg: &cfg };
        assert_eq!(t.status(), "zmng/status");
        assert_eq!(t.camera(7, "motion"), "zmng/camera/7/motion");
        let d = t.discovery(&cam());
        assert_eq!(d.len(), 4);
        assert_eq!(d[0].0, "homeassistant/binary_sensor/zmng_7_motion/config");
        assert_eq!(d[0].1["state_topic"], "zmng/camera/7/motion");
        assert_eq!(d[0].1["device"]["identifiers"][0], "zmng_camera_7");
        assert_eq!(d[3].1["topic"], "zmng/camera/7/thumbnail");
        let none = MqttConfig { discovery_prefix: String::new(), ..MqttConfig::default() };
        assert!(MqttTopics { cfg: &none }.discovery(&cam()).is_empty());
    }

    #[test]
    fn mqtt_messages_per_notification() {
        let cfg = MqttConfig::default();
        let info = EventInfo { id: 1, camera_id: 7, camera_name: "Front".into(), start_ms: 5, end_ms: None, score: 9, kind: "motion".into(), thumb: None, objects: vec![] };
        let m = mqtt_messages(&cfg, &Notification::EventStart(info.clone()));
        assert_eq!(m[0], ("zmng/camera/7/motion".to_string(), true, b"ON".to_vec()));
        assert_eq!(m[1].0, "zmng/camera/7/event");
        let m = mqtt_messages(&cfg, &Notification::EventEnd(info));
        assert_eq!(m[0].2, b"OFF");
        let m = mqtt_messages(&cfg, &Notification::CameraDown { camera_id: 7, name: "x".into() });
        assert_eq!(m, vec![("zmng/camera/7/recording".to_string(), true, b"OFF".to_vec())]);
        let m = mqtt_messages(&cfg, &Notification::EventThumbnail { event_id: 1, camera_id: 7, jpeg: Bytes::from_static(b"\xff\xd8") });
        assert_eq!(m[0].0, "zmng/camera/7/thumbnail");
    }

    #[test]
    fn objects_parse_from_meta() {
        let e = Event { id: 1, camera_id: 1, start_dts: 0, end_dts: None, kind: "person".into(), score: 1, peak_dts: None, thumb_path: None, archived: false, notes: None, meta_json: r#"{"objects":[{"label":"person","confidence":0.9,"bbox":[0,0,0.5,0.5]}]}"#.into() };
        let o = objects_from_meta(&e);
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].label, "person");
    }
}
