//! Object detection (phase 3), motion-gated.
//!
//! zmng does not link a neural-network runtime. When the motion detector
//! scores a frame, and at most every `interval_secs` per camera, the current
//! decoded substream frame is sent as a JPEG to an external detection server
//! speaking the DeepStack / CodeProject.AI protocol (`POST /v1/vision/
//! detection`, multipart field `image`, JSON `predictions[]` with `label`,
//! `confidence`, `x_min`..`y_max`). `deploy/detector/server.py` is a small
//! reference implementation on ONNX Runtime (YOLO, CUDA when available) so
//! the GPU does what it is good at without the recorder depending on it.
//!
//! Results are filtered by label, confidence and the camera's zones (a box
//! counts when its bottom-centre lies inside a zone), merged into the open
//! event (`event.kind` becomes the best label, `meta_json.objects` keeps the
//! best box per label) and announced on the bus. Losing the detection server
//! never affects motion events or recording.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, warn};

use crate::notify::DetectedObject;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ObjectsConfig {
    /// Detection endpoint, e.g. `http://127.0.0.1:32168/v1/vision/detection`.
    pub url: String,
    /// Sent as `X-API-Key` (CodeProject.AI) when set.
    pub api_key: Option<String>,
    /// Labels worth an event, in priority order (first = most important).
    pub labels: Vec<String>,
    pub min_confidence: f64,
    /// Minimum seconds between detections per camera while motion continues.
    pub interval_secs: f64,
    pub timeout_ms: u64,
    /// Detections in flight across all cameras.
    pub max_concurrent: usize,
}

impl Default for ObjectsConfig {
    fn default() -> Self {
        ObjectsConfig {
            url: "http://127.0.0.1:32168/v1/vision/detection".into(),
            api_key: None,
            labels: ["person", "car", "truck", "bus", "motorcycle", "bicycle", "dog", "cat"].iter().map(|s| s.to_string()).collect(),
            min_confidence: 0.5,
            interval_secs: 2.0,
            timeout_ms: 3000,
            max_concurrent: 4,
        }
    }
}

/// Parse a DeepStack/CodeProject.AI response into normalized boxes.
pub fn parse_predictions(v: &serde_json::Value, w: u32, h: u32) -> Vec<DetectedObject> {
    let (fw, fh) = (w.max(1) as f64, h.max(1) as f64);
    v.get("predictions")
        .and_then(|p| p.as_array())
        .map(|preds| {
            preds
                .iter()
                .filter_map(|p| {
                    let label = p.get("label")?.as_str()?.to_string();
                    let confidence = p.get("confidence")?.as_f64()?;
                    let g = |k: &str| p.get(k).and_then(|x| x.as_f64());
                    let bbox = [
                        (g("x_min")? / fw).clamp(0.0, 1.0),
                        (g("y_min")? / fh).clamp(0.0, 1.0),
                        (g("x_max")? / fw).clamp(0.0, 1.0),
                        (g("y_max")? / fh).clamp(0.0, 1.0),
                    ];
                    Some(DetectedObject { label, confidence, bbox })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Point-in-polygon (even-odd) on normalized coordinates.
pub fn inside(poly: &[(f64, f64)], x: f64, y: f64) -> bool {
    let mut c = false;
    let n = poly.len();
    if n < 3 {
        return false;
    }
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = poly[i];
        let (xj, yj) = poly[j];
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            c = !c;
        }
        j = i;
    }
    c
}

/// Keep detections whose label is wanted, confidence high enough, and whose
/// bottom-centre (where feet/wheels are) lies inside a zone (no zones =
/// whole frame) and outside every mask.
pub fn filter(objs: Vec<DetectedObject>, labels: &[String], min_confidence: f64, zones: &[Vec<(f64, f64)>], masks: &[Vec<(f64, f64)>]) -> Vec<DetectedObject> {
    objs.into_iter()
        .filter(|o| o.confidence >= min_confidence)
        .filter(|o| labels.is_empty() || labels.iter().any(|l| l.eq_ignore_ascii_case(&o.label)))
        .filter(|o| {
            let (x, y) = ((o.bbox[0] + o.bbox[2]) / 2.0, o.bbox[3]);
            (zones.is_empty() || zones.iter().any(|z| inside(z, x, y))) && !masks.iter().any(|m| inside(m, x, y))
        })
        .collect()
}

/// Objects seen during one event: the best box per label.
#[derive(Default, Debug, Clone)]
pub struct EventObjects {
    pub best: HashMap<String, DetectedObject>,
    pub detections: u32,
}

impl EventObjects {
    /// Merge one detection result; returns true when the set of labels or a
    /// best confidence changed (worth a DB update + notification).
    pub fn merge(&mut self, objs: &[DetectedObject]) -> bool {
        self.detections += 1;
        let mut changed = false;
        for o in objs {
            match self.best.get(&o.label) {
                Some(b) if b.confidence >= o.confidence => {}
                _ => {
                    self.best.insert(o.label.clone(), o.clone());
                    changed = true;
                }
            }
        }
        changed
    }
    pub fn is_empty(&self) -> bool {
        self.best.is_empty()
    }
    /// The event kind: the highest-priority label seen (by `labels` order),
    /// else the most confident one; `motion` when nothing was seen.
    pub fn kind(&self, labels: &[String]) -> String {
        if let Some(l) = labels.iter().find(|l| self.best.keys().any(|k| k.eq_ignore_ascii_case(l))) {
            return l.to_lowercase();
        }
        self.best.values().max_by(|a, b| a.confidence.total_cmp(&b.confidence)).map(|o| o.label.to_lowercase()).unwrap_or_else(|| "motion".into())
    }
    /// Sorted by confidence, best first.
    pub fn list(&self) -> Vec<DetectedObject> {
        let mut v: Vec<DetectedObject> = self.best.values().cloned().collect();
        v.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
        v
    }
}

/// Outcome of the most recent requests, for the status page.
#[derive(Clone, Debug, Default)]
pub struct DetectorHealth {
    /// epoch ms of the last successful detection
    pub last_ok_ms: Option<i64>,
    /// epoch ms and text of the last failure ("detector busy" is not one)
    pub last_err: Option<(i64, String)>,
}

pub struct ObjectDetector {
    pub cfg: ObjectsConfig,
    client: reqwest::Client,
    sem: Arc<tokio::sync::Semaphore>,
    health: parking_lot::Mutex<DetectorHealth>,
}

impl ObjectDetector {
    pub fn new(cfg: ObjectsConfig) -> Result<ObjectDetector> {
        let client = reqwest::Client::builder().timeout(std::time::Duration::from_millis(cfg.timeout_ms.max(100))).redirect(reqwest::redirect::Policy::none()).build()?;
        Ok(ObjectDetector { sem: Arc::new(tokio::sync::Semaphore::new(cfg.max_concurrent.max(1))), cfg, client, health: Default::default() })
    }

    pub fn health(&self) -> DetectorHealth {
        self.health.lock().clone()
    }

    /// A failure newer than the last success, within the last `window_ms`.
    pub fn failing(&self, now_ms: i64, window_ms: i64) -> Option<String> {
        let h = self.health.lock();
        match &h.last_err {
            Some((t, e)) if now_ms - t < window_ms && h.last_ok_ms.is_none_or(|ok| ok < *t) => Some(e.clone()),
            _ => None,
        }
    }

    /// Send one JPEG; returns normalized, unfiltered boxes. Waits for a slot
    /// only briefly: an overloaded detector drops frames rather than queueing.
    pub async fn detect(&self, jpeg: Vec<u8>, w: u32, h: u32) -> Result<Vec<DetectedObject>> {
        let permit = match tokio::time::timeout(std::time::Duration::from_millis(200), self.sem.acquire()).await {
            Ok(Ok(p)) => p,
            _ => anyhow::bail!("detector busy"),
        };
        let res = self.request(jpeg, w, h).await;
        drop(permit);
        let now = crate::db::now_dts() / 90;
        let mut hl = self.health.lock();
        match &res {
            Ok(_) => hl.last_ok_ms = Some(now),
            Err(e) => hl.last_err = Some((now, format!("{e:#}"))),
        }
        res
    }

    /// Like [`detect`](Self::detect) but waits for a slot (up to 30 s)
    /// instead of dropping: for one-off work such as classifying a finished
    /// event, where losing the frame means losing the label.
    pub async fn detect_patiently(&self, jpeg: Vec<u8>, w: u32, h: u32) -> Result<Vec<DetectedObject>> {
        for _ in 0..150 {
            if self.sem.available_permits() > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        self.detect(jpeg, w, h).await
    }

    async fn request(&self, jpeg: Vec<u8>, w: u32, h: u32) -> Result<Vec<DetectedObject>> {
        let part = reqwest::multipart::Part::bytes(jpeg).file_name("frame.jpg").mime_str("image/jpeg")?;
        let form = reqwest::multipart::Form::new().part("image", part).text("min_confidence", format!("{}", self.cfg.min_confidence * 0.5));
        let mut req = self.client.post(&self.cfg.url).multipart(form);
        if let Some(k) = &self.cfg.api_key {
            req = req.header("X-API-Key", k);
        }
        let res = req.send().await.context("detection request")?;
        if !res.status().is_success() {
            anyhow::bail!("detector returned {}", res.status());
        }
        let body = crate::onvif::read_capped(res, 1 << 20).await.context("detection response")?;
        let v: serde_json::Value = serde_json::from_slice(&body).context("detection response")?;
        if v.get("success").and_then(|s| s.as_bool()) == Some(false) {
            anyhow::bail!("detector error: {}", v.get("error").and_then(|e| e.as_str()).unwrap_or("?"));
        }
        let objs = parse_predictions(&v, w, h);
        debug!(n = objs.len(), "detection");
        Ok(objs)
    }
}

/// Labels this camera wants: its own list, else the server default.
pub fn camera_labels(cfg: &ObjectsConfig, cam: &crate::db::Camera) -> Vec<String> {
    if cam.object_labels.trim().is_empty() {
        cfg.labels.clone()
    } else {
        cam.object_labels.split(',').map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect()
    }
}

/// Width and height of a JPEG from its header.
pub fn jpeg_size(jpeg: &[u8]) -> Result<(u32, u32)> {
    let r = image::ImageReader::with_format(std::io::Cursor::new(jpeg), image::ImageFormat::Jpeg);
    Ok(r.into_dimensions()?)
}

/// Outline colour per object kind, the same as the web UI's timeline colours.
fn box_colour(label: &str) -> [u8; 3] {
    match label {
        "person" => [0x3d, 0xdc, 0x84],
        "car" | "truck" | "bus" | "motorcycle" | "bicycle" => [0xc3, 0x8b, 0xff],
        "dog" | "cat" | "bird" | "horse" => [0x4f, 0xd1, 0xc5],
        _ => [0xf9, 0xb8, 0x4f],
    }
}

/// The event picture with each object's box drawn on it (ZoneMinder's
/// `objdetect.jpg`). Boxes are normalized, so any picture of the same field
/// of view works; the line width follows the picture size.
pub fn draw_boxes(jpeg: &[u8], objs: &[DetectedObject]) -> Result<Vec<u8>> {
    let mut img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)?.to_rgb8();
    let (w, h) = img.dimensions();
    if w < 2 || h < 2 {
        return Ok(jpeg.to_vec());
    }
    let t = (w / 240).clamp(2, 6);
    for o in objs {
        let c = image::Rgb(box_colour(&o.label));
        let px = |v: f64, max: u32| ((v.clamp(0.0, 1.0) * max as f64).round() as u32).min(max - 1);
        let (x0, y0, x1, y1) = (px(o.bbox[0], w), px(o.bbox[1], h), px(o.bbox[2], w), px(o.bbox[3], h));
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        for k in 0..t {
            let (top, bottom) = ((y0 + k).min(y1), y1.saturating_sub(k).max(y0));
            let (left, right) = ((x0 + k).min(x1), x1.saturating_sub(k).max(x0));
            for x in x0..=x1 {
                img.put_pixel(x, top, c);
                img.put_pixel(x, bottom, c);
            }
            for y in y0..=y1 {
                img.put_pixel(left, y, c);
                img.put_pixel(right, y, c);
            }
        }
    }
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85).encode(img.as_raw(), w, h, image::ExtendedColorType::Rgb8)?;
    Ok(out)
}

/// Intersection over union of two normalized boxes.
pub fn iou(a: [f64; 4], b: [f64; 4]) -> f64 {
    let iw = (a[2].min(b[2]) - a[0].max(b[0])).max(0.0);
    let ih = (a[3].min(b[3]) - a[1].max(b[1])).max(0.0);
    let inter = iw * ih;
    let area = |x: [f64; 4]| (x[2] - x[0]).max(0.0) * (x[3] - x[1]).max(0.0);
    let u = area(a) + area(b) - inter;
    if u <= 0.0 { 0.0 } else { inter / u }
}

/// Boxes that are background: the same non-person label in (nearly) the
/// same place in at least `min_events` different events of one camera (a
/// parked bus, a car in its usual spot). Used for events recorded before
/// motion cells were kept; newer events are filtered by what moved instead.
pub fn static_boxes(events: &[crate::db::Event], min_events: usize) -> Vec<(i64, String, [f64; 4])> {
    let mut seen: Vec<(i64, String, [f64; 4], std::collections::HashSet<i64>)> = Vec::new();
    for e in events {
        for o in crate::notify::objects_from_meta(e).into_iter().filter(|o| o.label != "person") {
            match seen.iter_mut().find(|(c, l, b, _)| *c == e.camera_id && *l == o.label && iou(*b, o.bbox) >= 0.6) {
                Some(s) => {
                    s.3.insert(e.id);
                }
                None => seen.push((e.camera_id, o.label.clone(), o.bbox, [e.id].into_iter().collect())),
            }
        }
    }
    seen.into_iter().filter(|s| s.3.len() >= min_events).map(|(c, l, b, _)| (c, l, b)).collect()
}

fn is_static(o: &DetectedObject, cam: i64, statics: &[(i64, String, [f64; 4])]) -> bool {
    statics.iter().any(|(c, l, b)| *c == cam && *l == o.label && iou(*b, o.bbox) >= 0.6)
}

/// Run the detector on a finished event's picture (its thumbnail: the
/// main-stream keyframe at the motion peak, 640 px wide, so small or distant
/// objects the 320 px motion frames miss are still found) and rebuild the
/// event's objects: what the live pass saw plus what the picture shows,
/// keeping only objects where something moved at the peak (`peak_cells` in
/// the event) — or, for older events without cells, dropping the `statics`
/// (background boxes, see [`static_boxes`]). The kind becomes the
/// highest-priority label left. Returns whether the objects or kind changed.
pub async fn classify_event(det: &ObjectDetector, db: &crate::db::Db, cam: &crate::db::Camera, event_id: i64, jpeg: Vec<u8>, statics: &[(i64, String, [f64; 4])]) -> Result<bool> {
    let (w, h) = jpeg_size(&jpeg)?;
    let found = det.detect_patiently(jpeg, w, h).await?;
    let labels = camera_labels(&det.cfg, cam);
    let found = filter(found, &labels, det.cfg.min_confidence, &crate::detect::parse_polys(&cam.zones_json), &crate::detect::parse_polys(&cam.masks_json));
    let Some(e) = db.event(event_id)? else { return Ok(false) };
    let mut meta: serde_json::Value = serde_json::from_str(&e.meta_json).unwrap_or_else(|_| serde_json::json!({}));
    if !meta.is_object() {
        meta = serde_json::json!({});
    }
    let cells = meta.get("peak_cells").and_then(crate::detect::MotionCells::from_json).filter(|c| c.any());
    let before = crate::notify::objects_from_meta(&e);
    // live-pass objects were already checked against the cells of their own frame
    let kept: Vec<DetectedObject> = before.iter().filter(|o| cells.is_some() || !is_static(o, cam.id, statics)).cloned().collect();
    let found: Vec<DetectedObject> = found
        .into_iter()
        .filter(|o| match &cells {
            Some(c) => c.overlaps(o.bbox, 2),
            None => !is_static(o, cam.id, statics),
        })
        .collect();
    let mut objs = EventObjects::default();
    objs.merge(&kept);
    objs.merge(&found);
    let list = objs.list();
    let kind = objs.kind(&labels);
    let changed = kind != e.kind || list.len() != before.len() || list.iter().any(|o| !before.iter().any(|b| b.label == o.label && (b.confidence - o.confidence).abs() < 1e-9));
    meta["classified"] = serde_json::json!(true);
    meta["objects"] = serde_json::to_value(&list)?;
    db.update_event_objects(event_id, &kind, &meta.to_string())?;
    if changed {
        debug!(event = event_id, %kind, "event objects rebuilt from its thumbnail");
    }
    Ok(changed)
}

/// Per-camera gate: decides when to send a frame and carries results back
/// to the detector loop without blocking it.
pub struct ObjectGate {
    det: Arc<ObjectDetector>,
    labels: Vec<String>,
    zones: Vec<Vec<(f64, f64)>>,
    masks: Vec<Vec<(f64, f64)>>,
    interval: i64,
    last_sent: i64,
    in_flight: bool,
    tx: tokio::sync::mpsc::UnboundedSender<GateResult>,
    rx: tokio::sync::mpsc::UnboundedReceiver<GateResult>,
}

/// (frame dts, event it was sent for, what moved in it, detector answer)
type GateResult = (i64, Option<i64>, crate::detect::MotionCells, Result<Vec<DetectedObject>>);

impl ObjectGate {
    pub fn new(det: Arc<ObjectDetector>, cam: &crate::db::Camera) -> ObjectGate {
        let labels = camera_labels(&det.cfg, cam);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        ObjectGate {
            interval: (det.cfg.interval_secs * crate::mp4::TIMESCALE as f64) as i64,
            det,
            labels,
            zones: crate::detect::parse_polys(&cam.zones_json),
            masks: crate::detect::parse_polys(&cam.masks_json),
            last_sent: 0,
            in_flight: false,
            tx,
            rx,
        }
    }
    pub fn labels(&self) -> &[String] {
        &self.labels
    }

    /// A detection request is outstanding.
    pub fn in_flight(&self) -> bool {
        self.in_flight
    }

    /// Called with every motion-positive frame (yuv420p at `w`x`h`) and the
    /// cells that changed in it; sends a detection when the interval allows
    /// and none is in flight. `event_id` is the event open at send time so
    /// the result can be attributed; only objects overlapping `cells` count.
    pub fn maybe_send(&mut self, dts: i64, yuv: &[u8], w: usize, h: usize, event_id: Option<i64>, cells: crate::detect::MotionCells) -> bool {
        if self.in_flight || dts - self.last_sent < self.interval {
            return false;
        }
        let rgb = crate::preview::yuv420_to_rgb_tile(yuv, w, h, w as u32, h as u32);
        let Ok(jpeg) = crate::preview::encode_jpeg(&rgb, w as u32, h as u32, 85) else { return false };
        self.in_flight = true;
        self.last_sent = dts;
        let (det, tx) = (self.det.clone(), self.tx.clone());
        let (w, h) = (w as u32, h as u32);
        tokio::spawn(async move {
            let r = det.detect(jpeg, w, h).await;
            let _ = tx.send((dts, event_id, cells, r));
        });
        true
    }

    /// Filtered results that arrived since the last call (non-blocking):
    /// (frame dts, event the frame was sent for, objects).
    pub fn poll(&mut self) -> Vec<(i64, Option<i64>, Vec<DetectedObject>)> {
        let mut out = Vec::new();
        while let Ok((dts, sent_for, cells, r)) = self.rx.try_recv() {
            self.in_flight = false;
            match r {
                Ok(objs) => {
                    let moving = filter(objs, &self.labels, self.det.cfg.min_confidence, &self.zones, &self.masks).into_iter().filter(|o| !cells.any() || cells.overlaps(o.bbox, 1)).collect();
                    out.push((dts, sent_for, moving))
                }
                Err(e) => warn!("object detection: {e:#}"),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(label: &str, c: f64, bbox: [f64; 4]) -> DetectedObject {
        DetectedObject { label: label.into(), confidence: c, bbox }
    }

    #[test]
    fn parses_deepstack_predictions_into_normalized_boxes() {
        let v = serde_json::json!({"success": true, "predictions": [
            {"label": "person", "confidence": 0.91, "x_min": 320, "y_min": 90, "x_max": 480, "y_max": 360},
            {"label": "junk"}
        ]});
        let o = parse_predictions(&v, 640, 360);
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].label, "person");
        assert_eq!(o[0].bbox, [0.5, 0.25, 0.75, 1.0]);
        assert!(parse_predictions(&serde_json::json!({"success": false}), 640, 360).is_empty());
    }

    #[test]
    fn filter_by_label_confidence_zone_and_mask() {
        let left = vec![(0.0, 0.0), (0.5, 0.0), (0.5, 1.0), (0.0, 1.0)];
        let objs = vec![
            obj("person", 0.9, [0.1, 0.1, 0.3, 0.9]),  // bottom centre (0.2, 0.9): in zone
            obj("person", 0.9, [0.6, 0.1, 0.8, 0.9]),  // (0.7, 0.9): outside zone
            obj("car", 0.3, [0.1, 0.1, 0.3, 0.9]),     // low confidence
            obj("bird", 0.99, [0.1, 0.1, 0.3, 0.9]),   // label not wanted
        ];
        let labels = vec!["person".to_string(), "car".to_string()];
        let kept = filter(objs.clone(), &labels, 0.5, std::slice::from_ref(&left), &[]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].bbox[0], 0.1);
        // a mask over the left half removes it; no zones = whole frame
        let kept = filter(objs.clone(), &labels, 0.5, &[], std::slice::from_ref(&left));
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].bbox[0], 0.6);
        assert_eq!(filter(objs, &[], 0.0, &[], &[]).len(), 4);
        assert!(!inside(&[(0.0, 0.0), (1.0, 1.0)], 0.5, 0.5));
    }

    #[test]
    fn draw_boxes_outlines_each_object_in_its_kind_colour() {
        let grey = image::RgbImage::from_pixel(320, 180, image::Rgb([128, 128, 128]));
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 95).encode(grey.as_raw(), 320, 180, image::ExtendedColorType::Rgb8).unwrap();
        let out = draw_boxes(&jpeg, &[obj("person", 0.9, [0.25, 0.25, 0.75, 0.75]), obj("car", 0.8, [0.9, 0.9, 0.9, 0.95])]).unwrap();
        let img = image::load_from_memory(&out).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (320, 180));
        // the person box's top edge is green, its middle untouched
        let edge = img.get_pixel(160, 46);
        assert!(edge[1] > 180 && edge[0] < 120, "edge {edge:?}");
        let mid = img.get_pixel(160, 90);
        assert!((mid[0] as i32 - 128).abs() < 12 && (mid[1] as i32 - 128).abs() < 12, "mid {mid:?}");
        // a zero-width box draws nothing and does not panic; garbage in is an error
        assert!(draw_boxes(b"not a jpeg", &[]).is_err());
    }

    #[test]
    fn event_objects_merge_and_kind() {
        let mut e = EventObjects::default();
        assert!(e.merge(&[obj("car", 0.6, [0.0; 4])]));
        assert!(!e.merge(&[obj("car", 0.5, [0.0; 4])]));
        assert!(e.merge(&[obj("car", 0.7, [0.0; 4]), obj("person", 0.55, [0.0; 4])]));
        assert_eq!(e.detections, 3);
        let prio = vec!["person".to_string(), "car".to_string()];
        assert_eq!(e.kind(&prio), "person");
        assert_eq!(e.kind(&[]), "car"); // most confident
        assert_eq!(EventObjects::default().kind(&prio), "motion");
        let l = e.list();
        assert_eq!(l[0].label, "car");
        assert_eq!(l[0].confidence, 0.7);
    }
}
