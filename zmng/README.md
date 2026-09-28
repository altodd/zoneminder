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

`sudo deploy/install-ubuntu.sh` installs the binary, web files, a `zmng` user, `/etc/zmng/zmng.toml` and a systemd unit. See the script's output for the next steps.

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
| `GET /api/cameras/{id}/frame.jpg?t&width` | keyframe nearest to time `t` |
| `GET /api/segments/{id}/file.mp4` (Range), `/init.mp4`, `/frag/{n}.m4s` | raw media for HLS |
| `GET /api/events?camera&start&end&min_score&archived&before&limit&order` | keyset-paged list |
| `GET/PATCH /api/events/{id}`, `GET /api/events/{id}/thumb.jpg` | event detail, archive/notes, thumbnail |
| `GET/POST /api/users`, `PATCH/DELETE /api/users/{id}` | users and their camera lists (admin) |
| `GET /api/storages`, `GET /api/stats`, `GET /api/health` | status |

## Layout on disk

```
<storage>/<camera_id>/<YYYYMMDD>/<HHMMSS>.mp4     60 s fMP4 segments (UTC names)
<thumb_dir>/<camera_id>/<event_id>.jpg            event thumbnails
zmng.db                                           SQLite index (WAL)
```
A segment file is self-describing: `zmng reindex` re-adopts anything not in the index (also done automatically at startup).
