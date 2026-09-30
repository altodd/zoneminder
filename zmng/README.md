# zmng — ZoneMinder NG core

A small Rust NVR core built for high-resolution HEVC/H.264 IP cameras:

* **Passthrough recording**: the main RTSP stream is written as-is into 60 s fMP4 segment files. No decoding, no re-encoding. ~2 % of a core per 4 MP camera.
* **One index row per segment** (SQLite), with a 32-byte-per-GOP fragment index. No per-frame database rows.
* **Substream motion detection** at 5 fps through an ffmpeg child process; resolution-independent zones; one row and one JPEG per event.
* **Browser-native playback**: any time range is served as fMP4 for MSE, or as an HLS byte-range playlist; live view is the same fMP4 tapped at the GOP boundary. The browser decodes (Safari, Chrome/Edge with hardware HEVC; H.264 everywhere).
* **Retention as policy**: days per camera plus byte budget per volume; whole segments are unlinked, nothing is ever copied between volumes.
* **Users, roles, per-camera permissions** (viewers fail closed). **Camera groups** (a camera's comma-separated `tags`) are managed in Admin as a cameras × groups grid and pick cameras with one click on Live and Review.
* **Import of existing ZoneMinder events** in place (no re-encode), served from read-only storages.
* **Notifications**: webhook, MQTT with Home Assistant discovery, server-sent events for the UI, zmNinjaNg event-server websocket.
* **Operability**: Status page and Prometheus metrics with effective retention days per volume; `doctor`, `backup`, `restore`.
* **Depth (phase 3)**: scrub-preview strips from the detector's frames, motion-gated object detection through an external detector, on-demand H.264 transcode for browsers without HEVC, ONVIF PTZ, federation of several servers behind one UI, opt-in AAC audio, installable PWA.

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
zmng backup [--to FILE]      # consistent copy of the index (VACUUM INTO on its own read-only connection, fsynced); the service also does this daily into backup_dir (put it on the other volume; a missing volume is an error, never a copy onto the root disk)
zmng restore FILE --stopped  # replace the index with a backup after checking it; refuses while the service holds its lock; the old index is checkpointed and kept complete as *.before-restore
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
| `GET /api/cameras/{id}/video.mp4?start&end[&stream=sub][&codec=h264]` | fMP4 for the range (≤ 6 h); header `X-First-Dts-Ms`; `codec=h264` transcodes HEVC on demand |
| `GET /api/cameras/{id}/playlist.m3u8?start&end` | HLS (fMP4, byte ranges) |
| `GET /api/cameras/{id}/live.mp4[?stream=sub][&codec=h264]` | live fMP4 (chunked) |
| `GET /api/cameras/{id}/snapshot.jpg?width&stream` | latest keyframe as JPEG, decoded from the substream when it runs and can serve `width` (never upscaled; `stream=main` forces the main stream); `X-Stream` says which |
| `GET /api/cameras/{id}/frame.jpg?t&width[&stream=sub]` | keyframe nearest to time `t` (`stream=sub` ≈ 30 ms, used for scrub previews) |
| `GET /api/cameras/{id}/preview.jpg?t` | scrub-preview tile (160 px, colour) nearest `t`, taken from the detector's frames every `preview_secs`; one small file read, no decode |
| `GET /api/cameras/{id}/previews`, `GET /api/cameras/{id}/previews/{YYYYMMDDHH}.jpg|.json` | hours with previews; an hour as a sprite sheet (30 columns) plus its manifest `{cols, tile_w, tile_h, times}` |
| `GET /api/segments/{id}/file.mp4` (Range), `/init.mp4`, `/frag/{n}.m4s` | raw media for HLS |
| `GET /api/events?camera&start&end&min_score&kind&archived&before&limit&order` | keyset-paged list (`kind`: comma list of `motion`, `person`, `car`, ...); each event carries `objects` |
| `GET/PATCH /api/events/{id}`, `GET /api/events/{id}/thumb.jpg` | event detail, archive/notes, thumbnail |
| `GET/POST /api/users`, `PATCH/DELETE /api/users/{id}` | users and their camera lists (admin) |
| `GET /api/events/stream` | server-sent events: `event_start`, `event_update`, `event_end`, `camera_down`, `camera_up`, `storage_low` (ACL re-checked per message) |
| `GET /api/status` | health report: per-camera recording/detector state, last event, ingest, per-volume headroom and effective retention days, issues list |
| `GET /api/metrics` | the same as Prometheus text (admin token; scrape with `bearer_token`) |
| `GET /api/peers`, `/api/peers/{name}/api/...` | federation: peer reachability; proxied read API of a peer |
| `GET /api/storages`, `GET /api/stats`, `GET /api/health` | status |

## Several servers, one UI (federation)

Each server records its own cameras; one of them (or all of them) lists the others as peers:

```toml
[[peers]]
name = "barn"                         # [a-z0-9-]+, used in URLs and labels
url = "http://10.10.100.101:8080"
token = "…"                           # created on the peer: POST /api/tokens while logged in as the account this site may use
```
`/api/peers/<name>/api/...` proxies the peer's *read* API with that token (cameras, events, segments, previews, timeline, media, status; PATCH on events; PTZ for local admins), never its users, tokens, storage or its own peers. Create the token on the peer for a **viewer** account limited to the cameras this site is allowed to see: the peer's ACL for that account is what the proxy returns. The browser merges the peers' cameras into the live wall, Review and Events (labelled `name @peer`) and plays their media through the proxy; `GET /api/peers` shows which peers answer. Recording, retention, detection, notifications and the Status page stay per server. Peer events are not pushed over SSE and their go2rtc/WebRTC tiles are not available through the proxy.

## PTZ (ONVIF)

For a camera with a motorized head, `Admin → Cameras → Edit → Probe PTZ (ONVIF)` asks the device service (`onvif_url`, default `http://<camera host>/onvif/device_service`, with the RTSP credentials unless `onvif_user`/`onvif_pass` are set) for its Media and PTZ services and a PTZ-capable profile, and stores them. The camera page then shows a pad: hold an arrow to move (ContinuousMove, auto-stop after 10 s as a safety net), release to stop, zoom, presets and home. Admins only: a move changes what every viewer sees.

| Method/path | Purpose |
|---|---|
| `POST /api/cameras/{id}/ptz/probe` | discover ONVIF media/PTZ URLs and profile, mark the camera `ptz` |
| `POST /api/cameras/{id}/ptz` `{action: move, pan, tilt, zoom, seconds}` / `{action: stop}` / `{action: preset, preset}` / `{action: set_preset, name}` / `{action: home}` | ContinuousMove (velocities -1..1, optional auto-stop), Stop, GotoPreset, SetPreset, GotoHomePosition |
| `GET /api/cameras/{id}/ptz/presets` | `[{token, name}]` |

zmNinjaNg sees PTZ cameras as `Controllable` with the generic control `/zm/api/controls/1.json`; its `moveCon*`, `zoomCon*`, `moveStop`, `presetGoto` and `presetHome` requests map onto the same ONVIF calls.

## H.264 fallback for browsers without HEVC

Safari, and Chrome/Edge on machines with an HEVC decoder, play the 4 MP HEVC recording directly. Everything else (Firefox, Linux desktops, older Windows boxes) gets the H.264 substream — or, with a transcoder configured, the full-resolution picture re-encoded on demand:

```toml
[transcode]
encoder = "h264_nvenc"   # or libx264 (CPU), h264_vaapi, h264_qsv
preset = "p4"            # nvenc; libx264: veryfast
max_height = 1080        # 0 = source size
bitrate_kbps = 4000
gop_secs = 1             # keyframe = fragment = live latency
max_sessions = 3         # the P2200 has one NVENC engine; a 4th viewer gets 503
```
`GET /api/cameras/{id}/video.mp4?codec=h264&start&end` and `live.mp4?codec=h264` then pipe the fMP4 through ffmpeg and re-stamp every fragment with the recording's absolute timestamps, so the browser treats the result exactly like a native stream (same `X-First-Dts-Ms`, same MSE code). An H.264 source is passed through untouched; without `[transcode]` the request is answered with 501 and the UI keeps using the substream. `GET /api/me` reports `transcode: true` when it is available.

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
While the motion detector scores a camera, at most every `interval_secs` the current decoded substream frame is sent as a JPEG; boxes are filtered by label, confidence and the camera's zones/masks (a box counts where its bottom edge is), merged into the open event (`kind` = best label, `objects` = best box per label) and announced on the bus (`event_update`, or `event_start` when the camera has *Only keep events with a detected object* and this is the first object). Per camera: object detection on/off, a label override, and `require_object` (events with nothing detected are discarded at close). When an event closes, its thumbnail (the main-stream keyframe at the motion peak, 640 px wide) gets a second detection pass, so people too small for the 320 px motion frames are still labelled. The Events view and the Review page's *Show* filter select by kind (people, vehicles, animals, motion only); a kind filter matches any label seen in the event, not only its top one (`GET /api/events?kind=car,truck` finds a person-and-car event). Events recorded before the detector existed: Admin → Object detection → *Label the last 7 days of events* (`POST /api/events/classify {"days": 7, "camera": 3, "again": false}`, runs in the background, admin only). `GET /api/me` reports `objects: true` when a detector is configured (the UI hides object filters otherwise) and Status lists "detector failing" when it stops answering. Set `detect_width`/`detect_height` to 640x360 on cameras with object detection to give the live pass a sharper frame.

Servers that work: CodeProject.AI (`/v1/vision/detection`, YOLO on CUDA), DeepStack, and `deploy/detector/server.py` — a ~150-line ONNX Runtime server (YOLOv8/YOLO11 ONNX export, CUDA execution provider when present; the Quadro P2200 runs YOLOv8n at ~25 ms) with the same API. On the CPU it is fine for a few cameras (YOLO11s ~0.4 s per frame on the church box at nice 19; see `deploy/detector/README.md`). Object detection failing or absent never affects recording or motion events.

## Notifications (Home Assistant, ntfy, the UI, zmNinjaNg)

Every event start/end, camera outage/recovery, storage warning and service start is published on an in-process bus and delivered by whichever sinks are configured:

* **Webhook** — `alert_webhook = "https://host/hook"` in `zmng.toml`: one JSON POST per notification, `{"event": "event_start" | "event_update" | "event_end" | "camera_down" | "camera_up" | "storage_low" | "service_started", ...}`. Event payloads carry `id`, `camera_id`, `camera_name`, `start_ms`, `end_ms`, `score`, `kind`, `thumb` (URL) and `objects`. `event_start` fires as soon as motion is confirmed (so `thumb` is still null and `objects` empty); `event_update` follows whenever objects are detected and once more when the thumbnail exists, a few seconds after `event_end` (the peak frame is read from the segment still being written), so automations that want a picture trigger on `event_update` with `thumb` set (or on the MQTT thumbnail entity).
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
  Each camera appears in HA as a device with a *Motion* binary sensor (`ON` for the length of the event), a *Recording* sensor, a *Last event* sensor (attributes = the event JSON) and a *Last event thumbnail* camera entity. `zmng/status` is retained `online`/`offline` (last will) and is the availability topic. While the broker is unreachable, notifications are dropped (state topics are republished on reconnect); a camera removed from zmng keeps its stale discovery entry in HA until you delete the device there. Live images for HA dashboards: the `generic` camera platform on `/api/cameras/<id>/snapshot.jpg?width=1280&token=<long-lived token>` (HA's generic camera cannot send a bearer header; `POST /api/tokens` makes the token).
* **UI** — `GET /api/events/stream` is a server-sent-events feed of the same notifications, filtered to the cameras the user may see; the web UI shows toasts from it.
* **zmNinjaNg** — `/zm/ws` speaks the zmeventnotification websocket protocol (auth, `control/version`, `control/filter` with `monlist`/`intlist`, `push/token` registration, `alarm` messages with `DetectionJson`). Point the app's event-server URL at `ws(s)://<host>/zm/ws`.

## ZoneMinder-compatible API (zmNinjaNg)

Point zmNinjaNg at `http(s)://<host>:8080/zm` (portal URL). Supported: login/refresh (`?token=`), monitors, events list/detail/archive (delete is admin-only) with ZoneMinder's filter grammar, thumbnails (`index.php?view=image`), MP4 playback (`view_video`, byte ranges), HLS (`view_event_hls`), MJPEG/snapshot live (`cgi-bin/nph-zms`), notification token registration, PTZ control for probed cameras, and the event-server websocket at `/zm/ws`. Not yet: push delivery (needs the zmNinjaNg FCM relay).

**Live video in the app is MSE, not MJPEG.** zmNinjaNg plays live through go2rtc's `video-rtc` player when a monitor has `Go2RTCEnabled` and `ZM_GO2RTC_PATH` is set. zmng advertises its own endpoint (`ZM_GO2RTC_PATH` = `<scheme>://<host the app used>/zm/go2rtc`) and speaks the MSE part of go2rtc's websocket protocol at `/zm/go2rtc/ws?src=<id>[_CameraDirectSecondary]&token=…`: the recorder's own fMP4 fragments (substream for `_CameraDirectSecondary`, main when the phone lists `hvc1`), rebased to start at 0, first picture in ~0.1 s, no transcoding. WebRTC/HLS offers are declined so the player keeps MSE. Set `zmninja_mse = false` to go back to MJPEG, or `go2rtc_url` to advertise a real go2rtc instead. Snapshot tiles (`nph-zms?mode=single`) and MJPEG come from the substream (a 640 px tile never decodes the 4 MP main stream) and are cached per keyframe; `index.php?view=request&command=17&connkey=N` ends that MJPEG stream at once.

## Audio (opt-in)

`record_audio` on a camera (Admin → Cameras → Edit) records the main stream's AAC track alongside the video: a second `trak` in the init segment and a second `traf` per fragment, still one file per minute and one index row per segment (the index counts video only). Playback, HLS and export carry the audio; the MSE MIME type becomes `video/mp4; codecs="hvc1…,mp4a.40.2"`; the camera page's player has the browser's own mute control. Cameras without an AAC track, or whose audio SETUP fails, record video only with a warning. Substream recordings and the detector never carry audio. Off by default: audio is a legal question before it is a technical one.

## Storage tiering and retention

* `zmng add-storage /raid --archive-to <archive id> --archive-after-days 3` (or edit in Admin → Storage): segments older than 3 days are copied to the archive volume (checksummed, rate limited by `archive_rate_mbps`), then removed from the RAID. Space pressure on the RAID (byte cap or reserve reached) also triggers moves, unthrottled. Tiering is its own task; a pass that hits its batch limit runs again at once. A missing archive volume never stops recording.
* Hard floor: while the archive is reachable, retention leaves the primary to tiering, but once free space falls under **half** the reserve it deletes the oldest segments on the primary anyway (ENOSPC on the recording volume would stop every recorder). Status shows the archive backlog (bytes past the cutoff still on the primary) and warns when it exceeds a day of ingest: raise `archive_rate_mbps`.
* Mount marker: `add-storage` writes `.zmng-storage` at the storage root. A root without it is treated as unmounted: no recording, deleting, moving or reindexing there, an error line in Status and in `zmng doctor`. Read-only storages only have to exist. Re-run `add-storage` (or `touch <root>/.zmng-storage`) after moving a volume by hand.
* `zmng add-storage /var/cache/zoneminder/events --read-only`: a read-only storage is never deleted from, never drained by tiering and never written to (doctor only checks it is readable); use it for imported ZoneMinder event trees. Flip the flag in Admin → Storage when those files may finally go.
* Per camera: `retention_days` (continuous footage) and `event_retention_days` (segments that overlap a motion event). Archived events keep their footage past both ages, and the space budget deletes unpinned footage first; only at the hard floor does archived footage go too.
* Substream recording (`record_sub`, default on) stores `<camera>/sub/<day>/*.mp4`; use `?stream=sub` on the video/playlist/live endpoints.

## Layout on disk

```
<storage>/<camera_id>/<YYYYMMDD>/<HHMMSS>.mp4     60 s fMP4 segments (UTC names)
<thumb_dir>/<camera_id>/<event_id>.jpg            event thumbnails
<thumb_dir>/<camera_id>/preview/<YYYYMMDDHH>.pvs  scrub-preview tiles (append-only, one file per hour, ~2 MB)
zmng.db                                           SQLite index (WAL)
```
A segment file is self-describing: `zmng reindex` re-adopts anything not in the index (also done automatically at startup).
