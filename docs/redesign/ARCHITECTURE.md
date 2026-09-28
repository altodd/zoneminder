# ZoneMinder NG — architecture and plan

*Prepared 2026-09-27 for the church security server (10.10.100.100). Companion documents: `research/00-live-server-profile.md`, `research/01-online-research.md`, `research/02-alternative-architectures.md`, `reviews/01-capture-pipeline-review.md`, `reviews/02-web-events-review.md`, `reviews/03-storage-db-review.md`, `QUICK-WINS-PRODUCTION.md`. Working code: `../zmng/`.*

## 1. What is actually wrong today

Measured on the production box (23 Hikvision cameras, HEVC 2688x1520 @ 18 fps, one 4K; 2x Xeon Silver 4116, 125 GB RAM, Quadro P2200; ZoneMinder 1.38.4):

| Symptom | Root cause (with evidence) |
|---|---|
| Load average 20; every `zmc` burns 0.6–1.5 cores; NVDEC pinned at 100 % | ZoneMinder decodes **every frame of every full-resolution HEVC main stream**. `AnalysisSource=Secondary` is loaded but never used in this code base (`zm_monitor.cpp:404`), so the substream config we relied on does nothing. Each decoded frame is copied GPU→CPU (6 MB NV12), converted with swscale, timestamp-annotated and memcpy'd into a 98 MB shared-memory ring: ~31 MB of memory traffic per frame, ~550 MB/s per camera, on a memory bus shared by 23 processes. 23 × 18 fps × 4.08 Mpx = 1.7 Gpx/s is more than one Pascal NVDEC engine can do, hence 100 %. |
| Motion detection is unreliable ("every large event was the sunset shadow line") | Zones are pixel polygons drawn years ago at 1920x1080 / 1280x720 and are applied **unscaled** on the 2688x1520 frame (`zm_zone.cpp:76-79`), so each zone silently covers only ~51 % (top-left) of the picture. The reference image is blended at 18 fps regardless of `AnalysisFPSLimit`. |
| Live view is 640x360 | Browsers cannot play HEVC through WebRTC/Janus; ZM's own live path is MJPEG from decoded frames. go2rtc can carry HEVC to Safari/Chrome (MSE) but ZM only wires the substream. |
| mysqld at a constant 38 % CPU | **243 million rows in `Frames`** (34 GB) and 52 M in `Stats`, with `innodb_buffer_pool_size` at the 128 MB default. ~126 Frames inserts/s fleet-wide (a row per analysed frame while in alarm, plus per-batch `UPDATE Events` + summary-table triggers). `zmaudit` every 15 min runs orphan scans over Frames/Stats (`zmaudit.pl.in:713-734`). Deleting one 10-minute event removes ~4,000 Frames + ~900 Stats rows from cold index pages; ~13 M row deletes/day at steady state. |
| Events list "almost unusable" | `web/ajax/events.php` has **no SQL LIMIT** (buffers all matching rows into PHP, then `array_slice`); the default StartTime-asc sort with no date term produces an HTTP 500 (PHP memory exhausted). Per-row `folder_size()` on 1,379 events with NULL DiskSpace. Each of 25 thumbnails is a separate PHP bootstrap on a per-monitor port, GD-decodes a 4 MP JPEG, is sent twice (missing `ob_end_clean`) and carries an hourly-rotating auth hash that defeats caching. Every page `exec`s `zmdc.pl check` (200 ms) plus four `disk_free_space`. |
| Event playback / hover / montage review stall | HEVC events are not given a `<video>` tag; they fall back to `zms` MJPEG, which **software-decodes 2688x1520 HEVC per viewer** (hover preview at 60 fps!). Montage review requests one `nph-zms mode=single` per monitor per tick (0.75–1.4 s and 200 KB each × 23). Timeline loads every alarm Frames row for the range (~700 k/day) into PHP. |
| Both volumes permanently at 94–95 % | Retention is emergent: purge-at-95 % filters with limits, plus `AutoMove` copying 200 × 240 MB events from the RAID to the USB disk per pass through the page cache while 23 recorders write. |

The good news: recording itself is already **passthrough** (no encode; 1.5 Mbit/s HEVC files) and the muxer/videostore code is fine. Everything expensive is happening in service of things the user never asked for: full-res decode for a live view that is not used, per-frame DB rows for an index nobody queries by frame, and JPEG re-encoding for playback.

## 2. Requirements

1. Record every camera continuously at native resolution and codec (HEVC 4 MP / 8 MP) with **zero decoding and zero re-encoding** on the server.
2. Motion detection (later: object detection) on the **substream**, at ~5 fps, with correctly scaled zones and masks.
3. **Instant** event and timeline browsing: any list, timeline or thumbnail request is O(rows returned), never touches per-frame data, never spawns ffmpeg for a page view.
4. Full-resolution **live view and playback in the browser** with the browser doing the decoding (MSE/HLS-fMP4 HEVC on Safari and on Chrome/Edge with hardware decode; H.264 substream everywhere else), plus low-latency WebRTC via go2rtc.
5. Scrubbing, clip export, frame export.
6. Multi-user with per-camera permissions that fail closed (the CBA guest group over Tailscale must keep working).
7. Retention as a policy (days per camera, byte budget per volume), never "purge when full", never copying files between volumes.
8. Use the GPU for what it is good at (object detection, an on-demand NVENC transcode for clients without HEVC), not for decoding 23 main streams.
9. One small, memory-safe service that a VM can run; simple to operate; the existing 14 TB of ZoneMinder events importable without re-encoding.

## 3. Design

### 3.1 Pipeline per camera

```
                    ┌──────────────────────────┐   fMP4 fragments (one GOP each)
RTSP main (HEVC) ──►│ retina RTSP → depacketize │──► segment writer ──► /vol/<cam>/<day>/<HHMMSS>.mp4
                    └──────────────────────────┘        │   (60 s files, ftyp+moov + moof/mdat…)
                                                        │
                                                        ├──► SQLite: one `segment` row + 32-byte/fragment index blob
                                                        └──► live broadcast (init + fragments) → MSE viewers

RTSP substream (H.264 640x360) ──► ffmpeg child: fps=5, scale=320x180, gray ──► motion detector (Rust)
                                                                                 │ score/frame
                                                                                 ├──► per-fragment motion score (timeline bar)
                                                                                 └──► event state machine → one `event` row
                                                                                                          → one JPEG (keyframe decode)
```

* **No decode of the main stream, ever.** The recorder only parses NAL headers (keyframe flag, parameter sets). CPU per camera is a few percent for RTSP parsing and I/O (retina, the same library moonfire-nvr uses, records 6×1080p on a Raspberry Pi 2 under 10 % CPU).
* **Timestamps are absolute.** Every fragment's `tfdt` is 90 kHz ticks since the Unix epoch. Fragments from different files concatenate without rewriting; HLS byte-range playlists point straight into files; serving any time range is "send init + copy byte ranges".
* **Segments, not events, are the unit of storage.** A segment is a 60 s file (closed at the first keyframe past 60 s). Retention deletes whole files: one `unlink` and one row. Events are metadata intervals over the continuous recording, so an event's footage is never lost to a purge filter before the event is.
* **Index per segment, not per frame.** Each segment row carries a blob of 32 bytes per fragment (dts, duration, byte offset, length, sample count, motion score). For our fleet that is ~1 s GOPs → ~60 entries/segment → ~2 MB/day/camera of index in SQLite; the `Frames` table disappears.
* **Detection is decoupled.** The detector reads the substream via an ffmpeg child process (Frigate's proven pattern). Losing the substream never affects recording. Scores flow back to the recorder to tag fragments, and to an event state machine (pre-roll, cooldown, post-roll, 10-minute hard cap). Zones are normalized 0..1 polygons, so they are resolution-independent by construction.
* **Live view = the recording.** A new viewer gets the current init segment plus the last closed fragment, then every fragment as it closes: full-resolution HEVC live in Safari/Chrome at ~1 GOP latency (1–2 s here) with zero server CPU. go2rtc stays as the sub-second WebRTC path for the H.264 substream (grid tiles), and can carry HEVC WebRTC to Chrome 136+/Safari 18+.

### 3.2 Data model (SQLite, WAL, on the SSD)

```
storage(id, path, max_bytes, reserve_bytes)
camera(id, name, main_url, sub_url, enabled, storage_id, retention_days,
       detect_fps, detect_width, detect_height, pixel_threshold, min_area_pct, min_blob_pct,
       pre_secs, post_secs, cooldown_secs, zones_json, masks_json, sort_order)
sample_entry(id, sha256, codec, rfc6381, width, height, data)      -- the avc1/hvc1 box, deduplicated
segment(id, camera_id, storage_id, sample_entry_id, start_dts, end_dts, path, bytes, init_len,
        frames, motion_max, index_blob)  INDEX (camera_id,start_dts), (camera_id,end_dts), (storage_id,start_dts)
event(id, camera_id, start_dts, end_dts, kind, score, peak_dts, thumb_path, meta_json, archived, notes)
      INDEX (camera_id,start_dts), (start_dts)
user(id, username, pass_hash(argon2), role)   user_camera(user_id, camera_id)   session(token, user_id, expires_at)
```

Queries the UI makes, all index range scans:
* timeline for camera X, [t1,t2): `segment WHERE camera_id=? AND end_dts>? AND start_dts<?` (≈ 180 rows for 3 h) → coverage intervals + motion histogram from the index blobs, plus `event` rows in range.
* events list: `event WHERE camera_id IN (…) AND score>=? AND start_dts… ORDER BY start_dts DESC LIMIT 60` with keyset paging (`id < last`).
* video for [t1,t2): the same segment query, then fragments overlapping the range.

Compare: ZoneMinder's Events list joins Monitors/Events_Tags/Tags with GROUP BY and filesort, then touches the filesystem per row.

### 3.3 Serving video

| Need | How | Server cost |
|---|---|---|
| Play a time range (event, clip, scrub target) | `GET /api/cameras/{id}/video.mp4?start&end` → init + fragments streamed from files; MSE in the browser with `timestampOffset = -start` | file reads only (60 s of 4 MP HEVC = 22 MB in ~30 ms) |
| Safari/iOS native, hls.js | `GET /api/cameras/{id}/playlist.m3u8?start&end` → `EXT-X-MAP` + `EXT-X-BYTERANGE` into `/api/segments/{id}/file.mp4` (Range supported) | none |
| Live full-res | `GET /api/cameras/{id}/live.mp4` chunked fMP4 | none |
| Live low-latency / non-HEVC clients | go2rtc WebRTC of the H.264 substream (already deployed) | go2rtc |
| Thumbnail / frame at time t | decode **one keyframe** with ffmpeg from init + the one fragment containing t (`-frames:v 1`) | ~60 ms once per event; cached immutably by URL |
| Export | same range endpoint with `download` | none |
| Clients that cannot decode HEVC and need full res | (phase 3) on-demand NVENC HEVC→H.264 of one stream per viewer; the P2200's NVENC is idle today; cap concurrent sessions | GPU |

### 3.4 Process model

One Rust binary (`zmng`) running: recorder task per camera, detector task per camera (each supervising one ffmpeg child), retention task, HTTP API (axum) serving the static SPA. SQLite in-process. Crash safety: segments are append-only fMP4; on start, files missing from the index are re-scanned (`moof`/`tfdt`/`trun` walk) and adopted; open events are closed. A panic in one camera task is isolated; the supervisor restarts it with backoff. Systemd unit + TOML config. No PHP, Perl, Apache, MySQL, or shared memory.

### 3.5 Storage policy

* Cameras are assigned to a volume (11 TB SATA as the bulk volume, 6.3 TB RAID for the most important cameras, or split by bitrate). Nothing is ever copied between volumes.
* Retention runs every 30 s: delete segments older than `camera.retention_days`; then, per volume, while `used > max_bytes` or `free < reserve_bytes`, delete the oldest segments of that volume; then delete events whose footage is entirely gone (archived events keep their metadata and thumbnail).
* Capacity today: ~70–80 Mbit/s aggregate ≈ 0.8–0.9 TB/day → ~17 days on 17 TB at HEVC, ~9–11 days if the cameras were switched to H.264. Keeping HEVC is the right call; browsers that lack HEVC get the H.264 substream or (phase 3) an NVENC transcode.

### 3.6 Security

* argon2 password hashes, HttpOnly session cookies (or Bearer tokens for automation), sessions expire.
* Viewers see only cameras explicitly assigned (`user_camera`); every media/thumbnail/timeline route checks the camera id. RTSP URLs (which contain camera passwords) are never returned to non-admins. Segment files are served by id, never by path, so there is no path traversal surface.
* go2rtc's `/api/streams` (which leaks RTSP credentials) stays un-proxied, as today.

### 3.7 GPU plan

* Phase 1 (now): GPU unused by the recorder. Substream decode is software (23 × 640x360 @ 5 fps ≈ 0.5 core total). Thumbnails are software I-frame decodes (~60 ms each, a few per minute).
* Phase 2: object detection (person/vehicle) on the detector's frames, motion-gated, via ONNX Runtime with the CUDA execution provider (TensorRT 10 dropped Pascal; CUDA 12 still runs YOLO-class models on the P2200 at ~20–100 ms). Detections become `event.kind`/`meta_json` and filterable in the UI. A ~$100 Intel Arc A310 or a used RTX A2000 would be a better detection card than the P2200 if this matters.
* Phase 3: on-demand NVENC transcode of one main stream to H.264 for clients without HEVC decode (Firefox, some Windows machines), limited to a few concurrent sessions.

## 4. What exists now (`../zmng`, ~3,000 lines of Rust + a no-build SPA)

Verified 2026-09-27 against the real Front Door camera from this Mac (read-only extra RTSP client):

* `mp4.rs` — fMP4 init/fragment writer, index blob codec, file scanner (unit-tested).
* `recorder.rs` — retina RTSP ingest, GOP fragments, 60 s segments, wallclock anchoring with drift re-anchor, live broadcast, reconnect with backoff, hot-reconcile of camera config.
* `db.rs` — schema above and all queries (unit-tested).
* `detect.rs` — ffmpeg substream pipe, motion detector (reference blend, threshold, 8×8 block blobs, normalized zones/masks; unit-tested), event state machine, thumbnail generation.
* `video.rs` — range planner/streamer, HLS byte-range playlist, live fMP4.
* `retention.rs` — age + byte-budget retention, orphan event cleanup, crash re-index.
* `api.rs` — auth (setup flow, login, sessions, roles, per-camera ACL), cameras/users/storage admin, timeline, events (keyset paging), video range, HLS, segment Range serving, live, snapshot, frame-at-time.
* `web/` — Live grid (MSE full-res, go2rtc WebRTC, or snapshots), Review (timeline with coverage/motion/event bars, click-to-seek, wheel zoom, keyboard, thumbnail strip, clip export), Events (instant paging, thumbnails, archive/notes, open-in-timeline, download), Admin (cameras, users, storage, browser capability report).

Measured on the Mac: a 60 s 4 MP HEVC range = 22.7 MB served in 0.03 s; frame-at-time JPEG 61 ms; live stream starts within one GOP; the recorder holds 18 fps at 2.8 Mbit/s with negligible CPU.

## 5. Plan

### Phase 0 — relief on the current server (hours; needs Aaron's approval, see QUICK-WINS-PRODUCTION.md)
Buffer pool 16 GB; `ZM_RECORD_EVENT_STATS=0`; slow/disable `zmaudit` orphan scans; `Decoding=KeyFrames` or the two-monitor pattern; `DefaultCodec=MP4HLS` for `<video>` playback; fix the 4K monitor dimensions; redraw zones in percent units; fix the events list LIMIT bug (fork patch). Expected: load 20 → ~5, NVDEC 100 % → ~10 %, events list from 1.5 s/500 to ~0.1 s.

### Phase 1 — zmng on the test VM (this week)
1. VM: Ubuntu 24.04, `ffmpeg`, the `zmng` binary, systemd unit (`deploy/`), one volume. Point at 2–3 cameras (extra RTSP clients are harmless; Hikvision allows several).
2. Validate in real browsers: Safari and Chrome HEVC MSE playback, iOS HLS, Firefox fallback to snapshots/WebRTC. Tune motion thresholds per camera; draw zones.
3. Import: `zmng import-zm` reads the ZoneMinder `Events` table (read-only MySQL) and adopts each existing event mp4 as a legacy segment with a `tfdt` base offset (ZoneMinder's files are already fMP4 with one fragment per keyframe), alarm intervals become events, `snapshot.jpg` becomes the thumbnail. No re-encode, no file moves. (Design ready; not yet implemented.)
4. Run in parallel with ZoneMinder for a week on all 23 cameras with a small retention budget on the RAID volume.

### Phase 2 — cut over (after parallel run)
Stop `zmc`/`zma`; zmng owns both volumes; ZoneMinder's MySQL is retired after import. Tailscale ACL: only port 80/443 to the new server (the per-monitor 10001–10023 ports go away). Home Assistant: REST/`snapshot.jpg` per camera; MQTT/webhook events (small addition).

### Phase 3 — depth
Object detection (ONNX/CUDA), per-hour low-res preview strips for scrubbing on slow links (Frigate's approach; decode nothing extra: build them from the detector's frames), NVENC fallback transcode, multi-server (each server records its cameras; one UI federates), PTZ/ONVIF, audio track passthrough, mobile PWA polish.

## 6. Decisions and trade-offs

* **Rewrite vs patch ZoneMinder.** The PHP/Perl layer (357 k + 52 k lines) and the decode-everything C++ core are the problem, not incidental to it; the useful ZoneMinder pieces (passthrough muxer, zone scorer) are reproduced in ~3 k lines. The fork is kept for phase 0 patches and as the reference implementation.
* **Rust.** Memory-safe long-running network service, `retina` for RTSP (H.264/H.265, TCP interleaved, keepalives, parameter-set tracking), `axum`/`tokio`, `rusqlite`. Compiles to one static-ish binary; ffmpeg is used as a child process only for decode (detector, thumbnails), where it is unbeatable and crash-isolated.
* **Own fMP4 writer instead of ffmpeg `-c copy -f segment`.** Gives absolute timestamps, exact fragment index, a GOP-granular live tap and crash-tolerant files, with ~400 lines. Frigate's ffmpeg-segment approach needs `ffprobe` per segment and nginx-vod to package.
* **SQLite instead of MySQL/Postgres.** Tens of thousands of rows/day, single writer, in-process; WAL mode; trivially backed up. Postgres is unnecessary at this scale.
* **HEVC stays on the cameras.** Storage ≈ 1.5–1.9× better than H.264; HEVC decode is available in Safari everywhere and in Chrome/Edge on hardware with HEVC (the church staff machines). Firefox/no-HEVC clients get the substream, later an NVENC transcode.
* **No `sidx`.** Fragments are addressed by our own index; MSE and HLS byte ranges never need `sidx`. (ZoneMinder's stalls with ffmpeg reading whole event files came from this; we never hand a whole file to a player.)
* **Substream recording** (Blue Iris/UniFi "medium" ladder) is deferred: at 23 × 0.5 Mbit/s it costs 124 GB/day and only helps multi-camera synchronized playback on weak clients. Cheap to add later (a second recorder per camera).

## 7. Risks

* HEVC MSE on Chrome depends on OS/GPU hardware decode; Linux desktops and some Windows boxes lack it → the UI probes `isTypeSupported` and falls back; phase 3 NVENC covers the rest.
* RTP timestamp drift vs wallclock: handled by anchoring at connect and re-anchoring at a keyframe after 30 s of >1 s drift; NTP on cameras and server keeps this rare.
* B-frames (none on Hikvision) would need `ctts`/composition offsets; detected and logged.
* Motion tuning: the simple detector is ZoneMinder-class; object detection in phase 2 is what really removes shadow/wind false positives.
* Two writers (ZoneMinder and zmng) reading the same cameras during the parallel run add one RTSP client per stream; Hikvision handles 6+.

## 8. Review outcomes (same day)

Two independent reviews were run against this plan and the code: `reviews/05-architecture-design-review.md` (design) and `reviews/04-zmng-code-review.md` (code). Their verdict on the shape of the design was positive (root causes correct, passthrough segments + per-segment index + substream detection + browser decode + SQLite are right). What they found, and what was done about it tonight:

| Finding | Status |
|---|---|
| A write error (disk full) was logged but the fragment was still indexed | Fixed: write failure aborts the session, nothing bad is indexed; supervisor reconnects with backoff |
| A panicking camera/detector task was never restarted | Fixed: supervisor loop restarts panicked tasks after 5 s |
| fsync + SQLite insert on tokio worker threads | Fixed: segment close runs on the blocking pool |
| Per-fragment motion score taken before the detector's tail arrived | Fixed: recomputed at segment close |
| Backward wallclock step would freeze timestamps; `HHMMSS.mp4` could overwrite | Fixed: discontinuity at a new segment; `create_new` with suffix |
| `storage_used_bytes` full-table SUM up to 50× per pass; unlink failure still deleted the row | Fixed |
| 10-minute event cap measured from peak instead of start | Fixed |
| RTSP password visible on ffmpeg's command line (`ps`) | Fixed: URL handed to ffmpeg through a 0600 concat list with per-entry options |
| Any LAN host could create the first admin on a fresh install; no login throttling; no long-lived API tokens for Home Assistant | Fixed: setup token printed in the log; global brute-force limiter; `POST /api/tokens` |
| No alerting, no index backups | Fixed: camera-down/up webhook (`alert_webhook`), daily `VACUUM INTO` backup |
| Live grid tried 23 full-resolution HEVC MSE tiles (impossible on client GPUs; HTTP/1.1 6-connection limit) | Fixed: tiles are snapshots (or go2rtc WebRTC substreams); click a tile for the one full-res MSE stream |
| MSE playback fetched 10-minute ranges and ignored `QuotaExceededError` | Fixed: 2-minute chained windows, eviction behind the playhead |
| Live MSE timeline started at 1.79e9 s | Fixed: `X-First-Dts-Ms` on the live endpoint |
| Legacy import read whole 240 MB files into RAM; ffmpeg-written fragments carry absolute `base_data_offset` | Fixed: seeking scanner; fragments are rebased (`default-base-is-moof`) and time-shifted when served; verified decoding a real ZoneMinder event through the range, HLS and thumbnail paths |
| Progressive `video.mp4` has no `Accept-Ranges` (iPhone `<video>` fallback) | Open: use the HLS playlist on iOS (already served); add `Accept-Ranges` later |
| Substream recording (multi-camera synchronized review, non-HEVC clients, iPhones) should be phase 1 | Accepted into phase 1 (second recorder task per camera; ~124 GB/day at 23 × 0.5 Mbit/s) |
| go2rtc iframe exposes port 1984 (unauthenticated API with RTSP credentials) to browsers | Open: keep go2rtc LAN-only or proxy it behind zmng auth; the UI marks the option |
| Detector stamps frames with wallclock at read time (±0.5 s) and ffmpeg is a second RTSP client | Open (design accepted): pull the substream with retina in-process and pipe Annex-B to ffmpeg, which also gives substream recording |
| `isTypeSupported` lies on some Windows/Chrome builds | Open: real probe (append init + one GOP) before choosing MSE |
| Archived events keep only metadata once their segments age out | Open: export-on-archive (write the clip to a protected directory) |
| TLS: none | Open: Caddy/nginx in front on the LAN, `tailscale serve` for guests |
| Volunteer operability: health page, migrations, zone editor | Open |
| Code review: open segment lost on any session exit (stream error, stop, disable); only re-adopted at next restart | Fixed: session end flushes the pending sample and closes/indexes the segment; SIGTERM/Ctrl-C closes all segments before exit (verified) |
| Code review: on a mid-stream parameter change the old segment was indexed with the new sample entry | Fixed |
| Code review: `timeline?bucket=0` divided by zero; no range cap; unbounded ffmpeg spawns for snapshot/frame | Fixed: `max(1)`, 8-day cap, semaphore of 4 concurrent decodes |
| Code review: event keyset paging on `id` while sorting by `start_dts` skipped/duplicated rows | Fixed: `(start_dts, id)` keyset (verified across pages) |
| Code review: detector cached the recorder handle once (scores lost after a recorder restart) | Fixed |
| Code review: `parse_moof`/`extract_sample_entry` could panic on a corrupt file at boot; imported ZoneMinder files got a 0-duration last fragment (`tfhd` default duration ignored) | Fixed: bounds-checked parsing, default_sample_duration honoured |
| Code review: `PATCH /api/cameras` accepted any type/range (e.g. `detect_width=0` → busy loop); deleting a camera with segments failed on the FK | Fixed: field validation; delete stops the recorder, removes files, then rows |
| Code review: two concurrent `/api/setup` calls could both succeed; setup-mode requests could read RTSP URLs | Fixed: atomic first-admin insert; URLs hidden in setup mode |
| Code review: live viewers were dropped when a recorder reconnected | Fixed: client resubscribes |
| Code review: verified correct — ISO BMFF box layouts and flags, index codec, HEVC codec strings, per-camera ACL on every media route, no path traversal, CSRF posture, XSS-free DOM. The ffmpeg "non monotonically increasing dts" message seen in tests is an artifact of `-f null` rescaling, not a file defect (`-c copy` is silent; sample durations are 2970/3060/5940/6030 ticks = the camera's 30 fps clock at 18 fps). | — |
| **Build zmng vs deploy Frigate** | The design reviewer's honest verdict: zmng is justified if (a) someone will maintain a Rust service, (b) the two-volume policy and/or the 14 TB legacy import are hard requirements, and (c) full-resolution HEVC in the browser without go2rtc matters more than Frigate's object-detection UX and community. Recommendation adopted for the VM week: **run both** (zmng and Frigate's `-tensorrt` image, 3 cameras each) and decide with evidence. Phase 0 quick wins proceed regardless. |
