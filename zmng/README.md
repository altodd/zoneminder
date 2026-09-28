# zmng — ZoneMinder NG core

A small Rust NVR core built for high-resolution HEVC/H.264 IP cameras:

* **Passthrough recording**: the main RTSP stream is written as-is into 60 s fMP4 segment files. No decoding, no re-encoding. ~2 % of a core per 4 MP camera.
* **One index row per segment** (SQLite), with a 32-byte-per-GOP fragment index. No per-frame database rows.
* **Substream motion detection** at 5 fps through an ffmpeg child process; resolution-independent zones; one row and one JPEG per event.
* **Browser-native playback**: any time range is served as fMP4 for MSE, or as an HLS byte-range playlist; live view is the same fMP4 tapped at the GOP boundary. The browser decodes (Safari, Chrome/Edge with hardware HEVC; H.264 everywhere).
* **Retention as policy**: days per camera plus byte budget per volume; whole segments are unlinked, nothing is ever copied between volumes.
* **Users, roles, per-camera permissions** (viewers fail closed).
* **Import of existing ZoneMinder events** in place (no re-encode).

Design and rationale: `../docs/ARCHITECTURE.md`.

## Build

```
curl https://sh.rustup.rs -sSf | sh -s -- -y      # Rust toolchain
cargo build --release                           # target/release/zmng
cargo test
```
Runtime dependency: `ffmpeg` (Ubuntu: `apt install ffmpeg`).

## Run (development)

```
zmng example-config > zmng.toml                 # edit paths
zmng add-storage /data/zmng --max-gb 500 --reserve-gb 20
zmng add-camera "Front Door" rtsp://user:pass@10.10.0.101:554/Streaming/Channels/101 \
     --sub-url rtsp://user:pass@10.10.0.101:554/Streaming/Channels/102
zmng run                                        # http://localhost:8080 → create the first admin
```
Camera settings (retention, detection thresholds, zones, masks) are edited in the Admin page or with `PATCH /api/cameras/{id}`.

Detector test without a camera: `--sub-url "lavfi:testsrc=size=640x360:rate=5"`.

## Deploy (Ubuntu 24.04 VM)

`sudo deploy/install-ubuntu.sh` installs the binary, web files, a `zmng` user, `/etc/zmng/zmng.toml` and a systemd unit. See the script's output for the next steps. Production cut over from ZoneMinder (storage hand-over, legacy import, TLS with `deploy/Caddyfile`, Tailscale ACL, Home Assistant recipes, roll back and restore drill): `../docs/redesign/CUTOVER.md`.

## Operating it

```
zmng doctor [--cameras]      # ffmpeg, clock, database integrity, volumes writable + free space, camera URLs (and an RTSP DESCRIBE per camera with --cameras); exit 1 on any FAIL
zmng backup [--to FILE]      # consistent copy of the index (VACUUM INTO); the service also does this daily into backup_dir (put it on the other volume)
zmng restore FILE --stopped  # replace the index with a backup after checking it; the old file is kept as *.before-restore
```
The **Status** page in the UI shows the same report as `GET /api/status`: recording/detector state and last event per camera, ingest per day, and for every volume how many days of footage fit at the current rate before retention deletes. Issues (camera down, detector stopped, volume missing or under its reserve, archive unreachable) are listed at the top; `camera_down`/`camera_up`/`storage_low` notifications go to the webhook/MQTT/SSE sinks on state changes.

## Import ZoneMinder events

On the ZoneMinder host:
```
mysql -B -N zm -e "SELECT Id,MonitorId,UNIX_TIMESTAMP(StartDateTime),UNIX_TIMESTAMP(EndDateTime),DATE(StartDateTime),Length,Frames,AlarmFrames,MaxScore,Archived,IFNULL(Notes,''),DefaultVideo,StorageId FROM Events WHERE EndDateTime IS NOT NULL AND StorageId=1 ORDER BY Id" > events-storage1.tsv
```
Then, with a zmng storage row whose path is that ZoneMinder storage's events root (e.g. `/var/cache/zoneminder/events`):
```
zmng add-storage /var/cache/zoneminder/events --reserve-gb 100      # say it becomes storage 2
zmng import-zm events-storage1.tsv --storage 2 --camera-map 1=1,2=2,...   # identity map by default
```
Each event mp4 becomes a legacy segment (timestamps shifted on the fly when served); each event becomes an `event` row with a downscaled `snapshot.jpg` thumbnail. Repeat per ZoneMinder storage. Idempotent (already-imported events are skipped).

## HTTP API (all JSON unless noted; times are epoch milliseconds)

| Method/path | Purpose |
|---|---|
| `POST /api/setup` `{username,password}` | first admin (only while no users exist) |
| `POST /api/login`, `POST /api/logout`, `GET /api/me` | session cookie `zmng_session` or `Authorization: Bearer` |
| `GET/POST /api/cameras`, `GET/PATCH/DELETE /api/cameras/{id}` | cameras (admin for writes; viewers never see RTSP URLs) |
| `GET /api/cameras/{id}/timeline?start&end[&bucket]` | coverage intervals, motion histogram, events |
| `GET /api/cameras/{id}/video.mp4?start&end` | fMP4 for the range (≤ 6 h); header `X-First-Dts-Ms` |
| `GET /api/cameras/{id}/playlist.m3u8?start&end` | HLS (fMP4, byte ranges) |
| `GET /api/cameras/{id}/live.mp4` | live fMP4 (chunked) |
| `GET /api/cameras/{id}/snapshot.jpg?width` | latest keyframe as JPEG |
| `GET /api/cameras/{id}/frame.jpg?t&width[&stream=sub]` | keyframe nearest to time `t` (`stream=sub` ≈ 30 ms, used for scrub previews) |
| `GET /api/cameras/{id}/preview.jpg?t` | scrub-preview tile (160 px, colour) nearest `t`, taken from the detector's frames every `preview_secs`; one small file read, no decode |
| `GET /api/cameras/{id}/previews`, `GET /api/cameras/{id}/previews/{YYYYMMDDHH}.jpg|.json` | hours with previews; an hour as a sprite sheet (30 columns) plus its manifest `{cols, tile_w, tile_h, times}` |
| `GET /api/segments/{id}/file.mp4` (Range), `/init.mp4`, `/frag/{n}.m4s` | raw media for HLS |
| `GET /api/events?camera&start&end&min_score&kind&archived&before&limit&order` | keyset-paged list (`kind`: comma list of `motion`, `person`, `car`, ...); each event carries `objects` |
| `GET/PATCH /api/events/{id}`, `GET /api/events/{id}/thumb.jpg` | event detail, archive/notes, thumbnail |
| `GET/POST /api/users`, `PATCH/DELETE /api/users/{id}` | users and their camera lists (admin) |
| `GET /api/events/stream` | server-sent events: `event_start`, `event_end`, `camera_down`, `camera_up`, `storage_low` |
| `GET /api/status` | health report: per-camera recording/detector state, last event, ingest, per-volume headroom and effective retention days, issues list |
| `GET /api/metrics` | the same as Prometheus text (admin token; scrape with `bearer_token`) |
| `GET /api/storages`, `GET /api/stats`, `GET /api/health` | status |

## Object detection (people, vehicles, animals)

zmng keeps no neural network of its own. Point it at a detection server that speaks the DeepStack / CodeProject.AI API and the GPU box does the work:

```toml
[objects]
url = "http://127.0.0.1:32168/v1/vision/detection"
labels = ["person", "car", "truck", "bus", "motorcycle", "bicycle", "dog", "cat"]   # priority order
min_confidence = 0.5
interval_secs = 2        # per camera, while motion continues
timeout_ms = 3000
max_concurrent = 4
```
While the motion detector scores a camera, at most every `interval_secs` the current decoded substream frame is sent as a JPEG; boxes are filtered by label, confidence and the camera's zones/masks (a box counts where its bottom edge is), merged into the open event (`kind` = best label, `objects` = best box per label) and announced on the bus (`event_update`, or `event_start` when the camera has *Only keep events with a detected object* and this is the first object). Per camera: object detection on/off, a label override, and `require_object` (events with nothing detected are discarded at close). The Events view filters by kind (people, vehicles, animals, motion only). Set `detect_width`/`detect_height` to 640x360 on cameras with object detection: the JPEG is the detector's frame.

Servers that work: CodeProject.AI (`/v1/vision/detection`, YOLO on CUDA), DeepStack, and `deploy/detector/server.py` — a ~150-line ONNX Runtime server (YOLOv8/YOLO11 ONNX export, CUDA execution provider when present; the Quadro P2200 runs YOLOv8n at ~25 ms) with the same API. Object detection failing or absent never affects recording or motion events.

## Notifications (Home Assistant, ntfy, the UI, zmNinjaNg)

Every event start/end, camera outage/recovery, storage warning and service start is published on an in-process bus and delivered by whichever sinks are configured:

* **Webhook** — `alert_webhook = "https://host/hook"` in `zmng.toml`: one JSON POST per notification, `{"event": "event_start" | "event_end" | "camera_down" | "camera_up" | "storage_low" | "service_started", ...}`. Event payloads carry `id`, `camera_id`, `camera_name`, `start_ms`, `end_ms`, `score`, `kind`, `thumb` (URL) and `objects` (phase 3 detections).
* **MQTT + Home Assistant discovery** —
  ```toml
  [mqtt]
  host = "10.10.100.5"
  port = 1883
  username = "zmng"
  password = "..."
  topic_prefix = "zmng"                # zmng/status, zmng/camera/<id>/{motion,recording,event,thumbnail}
  discovery_prefix = "homeassistant"   # "" disables discovery
  ```
  Each camera appears in HA as a device with a *Motion* binary sensor (`ON` for the length of the event), a *Recording* sensor, a *Last event* sensor (attributes = the event JSON) and a *Last event thumbnail* camera entity. `zmng/status` is retained `online`/`offline` (last will) and is the availability topic. Live images for HA dashboards: the `generic` camera platform on `/api/cameras/<id>/snapshot.jpg?width=1280` with a long-lived token (`Authorization: Bearer`, see `POST /api/tokens`).
* **UI** — `GET /api/events/stream` is a server-sent-events feed of the same notifications, filtered to the cameras the user may see; the web UI shows toasts from it.
* **zmNinjaNg** — `/zm/ws` speaks the zmeventnotification websocket protocol (auth, `control/version`, `control/filter` with `monlist`/`intlist`, `push/token` registration, `alarm` messages with `DetectionJson`). Point the app's event-server URL at `ws(s)://<host>/zm/ws`.

## ZoneMinder-compatible API (zmNinjaNg)

Point zmNinjaNg at `http(s)://<host>:8080/zm` (portal URL). Supported: login/refresh (`?token=`), monitors, events list/detail/archive/delete with ZoneMinder's filter grammar, thumbnails (`index.php?view=image`), MP4 playback (`view_video`, byte ranges), HLS (`view_event_hls`), MJPEG/snapshot live (`cgi-bin/nph-zms`), notification token registration. Not yet: push delivery (needs the zmNinjaNg FCM relay), PTZ, ES websocket.

## Storage tiering and retention

* `zmng add-storage /raid --archive-to <archive id> --archive-after-days 3` (or edit in Admin → Storage): segments older than 3 days are copied to the archive volume (checksummed, rate limited), then removed from the RAID. Space pressure on the RAID also triggers moves. A missing archive volume never stops recording.
* Per camera: `retention_days` (continuous footage) and `event_retention_days` (segments that overlap a motion event); archived events keep their footage.
* Substream recording (`record_sub`, default on) stores `<camera>/sub/<day>/*.mp4`; use `?stream=sub` on the video/playlist/live endpoints.

## Layout on disk

```
<storage>/<camera_id>/<YYYYMMDD>/<HHMMSS>.mp4     60 s fMP4 segments (UTC names)
<thumb_dir>/<camera_id>/<event_id>.jpg            event thumbnails
<thumb_dir>/<camera_id>/preview/<YYYYMMDDHH>.pvs  scrub-preview tiles (append-only, one file per hour, ~2 MB)
zmng.db                                           SQLite index (WAL)
```
A segment file is self-describing: `zmng reindex` re-adopts anything not in the index (also done automatically at startup).
