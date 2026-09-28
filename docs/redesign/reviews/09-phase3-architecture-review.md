# zmng phase 2/3 architecture review (read-only, 2026-09-28, HEAD 3b0d7ac)

Inputs: ARCHITECTURE.md §5/§9/§10, reviews/05, CUTOVER.md, zmng/README.md, all of `zmng/src`, `zmng/tests`, `zmng/deploy`, `zmng/web`. Line numbers are HEAD. Nothing was built or run.

**Verdict in one paragraph.** Phases 2 and 3 landed as designed and the module docs match the code; the passthrough recorder, per-segment index and browser-decode serving are intact. Three things will break a 23-camera / 17 TB / week-long run and none of them is in a phase-3 feature: (1) the tiering/retention interplay cannot keep up with ingest at its defaults and then deliberately refuses to delete, which ends in ENOSPC on the RAID and every recorder failing; (2) an unmounted volume is silently recorded onto the root SSD; (3) several every-30-s queries are full scans of a table that will hold ~1 M rows with 2 KB blobs, executed under the one global SQLite mutex that the async HTTP handlers also take. Everything else is a should-fix or a runbook correction. The phase-3 features themselves (previews, objects, transcode, PTZ, federation, audio, PWA) are small, isolated and fail-open toward recording, as the architecture demands; their remaining risks are ACL semantics in federation, GPU decode in the transcode path, and untested-against-hardware assumptions listed in §10.4.

---

## 1. Does the implementation honour the architecture's principles?

| Principle | Verdict | Evidence |
|---|---|---|
| No decode of the main stream | Recorder: yes. Serving: **the UI's default live wall decodes 23 main-stream 4 MP HEVC keyframes every 2 s per viewer** | `web/app.js:243` default `liveMode='snapshot'`; `fallbackSnapshot` `:284-292` hits `/snapshot.jpg` every 2 s; `api.rs:790-806 snapshot()` uses `app.hub.get(id)` (main stream) and spawns one ffmpeg per call. 23 tiles / 2 s = 11.5 4 MP software I-frame decodes/s per viewer, serialised by the 4-permit `decode_sem` (`api.rs:1255`). This is the "JPEG re-encode for a live view" pattern §1 condemned. Fix is trivial: decode the sub recorder's keyframe (`hub.get_stream(id, Sub)`) and default the wall to `sub` MSE (needs HTTP/2, see §3). |
| O(rows returned) queries | **No, on three paths** | (a) `db.rs:835-843 segments_in_range_stream`: `end_dts>? AND start_dts<?` with indexes `(camera_id,stream,start_dts)` / `(camera_id,end_dts)` (`db.rs:170-176`): one bound is always open, so a 3 h timeline query walks every segment of that camera on one side of the window (24 k rows × 2 KB `index_blob` after 17 days). Segments are ≤ `segment_secs`+GOP long, so rewrite as `start_dts >= start-70s AND start_dts < end`; same for `coverage` (`:884`) and, with the 10-min cap, `events_in_range` (`:1086`). (b) `storage_used_bytes` (`db.rs:549-557`) is `SUM(bytes)` over `segment_storage(storage_id,start_dts)`, not covering: a full read of every row page of the volume (~1.1 M rows main+sub × ~2 KB ≈ 2 GB of pages) — called every 30 s from `retention.rs:96`, `tier.rs:52`, and per Status page load (`health.rs:142`, polled every 30 s by `app.js:601`). §8 marked M4 "fixed" but only the call count changed. (c) `ingest_since` (`db.rs:909-915`): `WHERE start_dts>=? GROUP BY camera_id,storage_id` has no usable index → full scan per Status/metrics call. |
| Segments as the storage unit | Yes for video. Previews and thumbnails are a second, unbudgeted store | `preview.rs`: 720 tiles/h × 3 KB ≈ 2.1 MB per camera-hour → **1.2 GB/day fleet-wide on `thumb_dir` (the system SSD per CUTOVER §1)**; pruned by age only (`retention.rs:56`, `min(retention_days, event_retention_days)` = the *longer* of the two, default 30 d) → ~35 GB of previews on the root SSD, outliving the footage (budget-limited to ~17 days). No byte budget, no Status line, no README total. |
| Retention as policy, never "purge when full" | Age + budget: yes. **Tiering breaks it** (§2.1). Archived-event footage: **not kept under budget** | `retention.rs:105-119` space loop uses `oldest_segments` without `segment_pinned`; only the age loop (`:69`) pins. README ("archived events keep their footage", "archived events pin their footage") and `tests/retention.rs:38` (which checks the *row* survives) overstate it. With effective depth ≈ 17 d < `retention_days` 30 d, archived footage is always lost by the budget path. |
| Fail-closed ACLs | Local routes: yes on every new route (preview/sprite/PTZ presets `can_see`, PTZ commands admin, metrics admin, SSE re-checked per message). Federation: **site-level, not per-user** | `api.rs:1202-1235 peer_proxy`: any local user gets whatever the *peer token's account* may see; the local per-camera list is not intersected. A CBA viewer with one local camera sees every peer camera the site token can see. If the peer token belongs to an admin, proxied `GET /api/cameras` carries `main_url` (RTSP credentials) to every local viewer; nothing strips it or checks the token's role. Also: viewers may archive/annotate events locally (`api.rs:558-570`) and on peers (`peers.rs:57` PATCH allowed for non-admins) — S10 "operator role" still open. |
| One small binary | Yes (+ ffmpeg children, optional Python detector) | 23 ffmpeg decoders resident (one per camera) + ≤4 snapshot decoders + ≤3 transcoders + one per zmNinja MJPEG viewer. |
| Crash safety | Segments/events/previews: yes. **Unmounted volume: no** | `recorder.rs:398 create_dir_all(&cam_root)` under `storage.path`: if `/var/cache/zoneminder` or `/media/zmoverflow` is a bare mount point at boot, zmng creates `zmng/<id>/<day>/` on the root filesystem and records there; `fs_free_bytes` then reports root's free space, retention deletes nothing, the root SSD (which holds `zmng.db`) fills. `deploy/zmng.service` has no `RequiresMountsFor=`; `doctor` only checks `is_dir()`/writable. Sprite cache is written non-atomically (`preview.rs:240-246`, `std::fs::write` to the final name; a crash leaves a truncated `.jpg` served forever because `closed && jpg.is_file()`). |

Other principle-level notes:
* Thumbnails still wait for the segment to close: `detect.rs make_thumbnail` polls `segments_in_range` every 20 s up to 6× → `event_update` with `thumb` arrives 60–120 s after `event_end`; HA "person at the door with picture" is 1–2 min late. This was M12 (must-fix) in review 05 and is not in §8's fixed list; README documents the delay instead of removing it. The open segment's path and `frags` are on the recorder task; expose them on `CamHandle` (next to `recent`) and read the fragment immediately.
* `detect.rs:57-63 detect_key` omits `objects`, `object_labels`, `require_object`: toggling object detection in Admin does nothing until the detector restarts for another reason (the handle's `camera` is not even refreshed on the unchanged path). `record_audio` likewise only applies at the next reconnect (`recorder.rs reconcile` restarts only on URL/storage change).

## 2. Corners that will hurt at 23 cameras / 17 TB / a week

Ranked by damage.

1. **Tiering cannot keep up and retention then refuses to help → ENOSPC → all recorders fail.** `db.rs:29` `archive_rate_mbps` default 40 (5 MB/s ≈ 0.43 TB/day) versus ingest ≈ 0.85–0.95 TB/day (main + sub). `tier.rs:33` moves ≤ 20 segments per pass; at 5 MB/s a pass of 20 × 21 MB takes ~85 s, and the pass runs *inside* the 30 s maintenance loop (`main.rs:364-375`, awaited) so the loop period stretches to ~90 s. Once the RAID passes `archive_after_days × ingest` (3 d ≈ 2.9 TB) the "pressure" path (`tier.rs:55-61`) is still rate-limited, `retention.rs:87-95` `continue`s whenever the archive is reachable, free space goes under reserve, `flush_gop` write fails, sessions fail closed and reconnect into the same ENOSPC. CUTOVER §2 uses the default rate. Fix: default rate ≥ 2× ingest (or unlimited when under pressure), tiering in its own task with a continuous loop rather than 20-per-30 s, and a hard floor in retention (`free < reserve/2` → delete oldest on the primary even with a reachable archive). Validate on the VM with real ingest for ≥ 5 days.
2. **Root-filesystem recording on an unmounted volume** (§1 crash-safety row). Add `RequiresMountsFor=/var/cache/zoneminder /media/zmoverflow` to the unit, write a `.zmng-storage` marker at `add-storage` and refuse to open segments (and alert) when it is missing; `doctor` checks the marker and `mountpoint`.
3. **SQLite: one `parking_lot::Mutex<Connection>` (`db.rs:271`) taken synchronously from async handlers, and the scans in §1.** `api.rs` handlers (`timeline :462`, `events :519`, `me`, every `can_see`) lock it on tokio workers; the three periodic full scans (`storage_used_bytes` ×(storages+1), `ingest_since`) will each hold it for 0.3–2 s at 1 M rows. Every 30 s the API, SSE per-message ACL checks (`api.rs:1131`), recorder `insert_segment_ext` (on the blocking pool, so recording survives but segment close is delayed) and detector event writes queue behind them. Fix: (a) the bounded-range rewrite; (b) a `storage.used_bytes` counter maintained in `insert_segment_ext`/`delete_segment`/`move_segment` (or a covering index `(storage_id, bytes)` as a stopgap — still O(rows) but ~10× fewer pages); (c) per-day per-camera ingest from a small `ingest_day` table or an index on `(start_dts)`; (d) a read-connection pool used via `spawn_blocking` for handlers, leaving the writer connection to recorder/detector/retention. WAL already permits this.
4. **ffmpeg process pressure from the UI defaults** (§1 first row). Also `zmapi.rs:776` holds one of the four `decode_sem` permits for the *lifetime* of each zmNinja MJPEG stream: two phones on a 4-tile montage exhaust the semaphore and every `snapshot.jpg`/`frame.jpg` in the web UI waits indefinitely (no timeout on `acquire()`). Give MJPEG its own semaphore and a per-user cap.
5. **Maintenance loop serialisation** (`main.rs:306-375`): camera-down/up detection, `storage_low`, recorder/detector reconcile (i.e. cameras added in the UI), the daily backup (`spawn_blocking` awaited; a 2–4 GB `VACUUM INTO` onto the USB disk takes minutes) and tier+retention all run sequentially in one loop. During a backup or a slow tiering pass no camera-down alert fires and no new camera starts. Split into independent tasks with their own intervals.
6. **Memory**: bounded, no leaks found. Broadcast rings drop values once every receiver has read them; a stalled live viewer retains ≤ 64 main fragments (≈ 22–56 MB at 1–2.5 s GOP) per stalled connection (`recorder.rs` `broadcast::channel(64)`), the notification bus ≤ 256 × `EventThumbnail` JPEGs (`notify.rs:84,129`, ≤ ~25 MB). `stream_plan` copies every fragment (`video.rs:121`, `rebase_fragment` → `to_vec` even for native files) — 22 MB/min/viewer of churn, fine. `segment_file` (`api.rs:705`) reads the whole requested range into RAM (legacy 240 MB file without `Range` = 240 MB per request; S8 still open). `reindex` at boot loads every path into a `HashSet` (`retention.rs:174`, ~1 M strings ≈ 100 MB transient) and `readdir`s ~1,600 day directories; expect tens of seconds of no recording per restart on the USB disk — document it.
7. **Per-camera tasks/processes**: 2 recorder tasks + 2 supervisors, 1 detector + supervisor, 1 ffmpeg (+2 reader tasks), object-detection spawns per request; 23 resident ffmpeg decoders ≈ 23 × ~2 % core. Fine, but README's "~2 % of a core per 4 MP camera" describes the main recorder only; the honest per-camera figure (main + sub recorder + ffmpeg sub decode + preview JPEG + object JPEG) is 5–8 % and should be re-measured on the VM.
8. **Disk I/O**: `flush_gop` `write_all` (`recorder.rs` ~652) is synchronous on a tokio worker for 46 streams; page-cache writes normally, but a USB/RAID stall blocks workers (48 threads, so degradation not deadlock). Segment rotations of 46 streams are not staggered (05 §1.2). 66 k files/day; no `fallocate`/`fadvise` (05 nice-to-have). Tiering reads the source and re-reads the destination for the checksum (2× the copy in I/O on the USB disk) — acceptable at the right rate.
9. **Preview file counts**: 23 × 24 files/day + up to 2 cache files each → ~50 k files over 30 days in `thumb_dir`; `nearest()` (`preview.rs:165-188`) does ~720 `seek+read_exact` pairs per hour file per hover (up to 3 files) — page-cached, ~ms, but read the 2 MB file once instead. Hour keys are UTC while the runbook talks local time; fine.
10. **MQTT fan-out**: correct after the H1 fix (`try_publish`, 4096 queue, republish on ConnAck). Retained `event` JSON per `EventUpdate` (each new object) is a few hundred bytes; fine. Webhook sink (`notify.rs:165-191`) is *sequential* with a 10 s timeout: one slow ntfy/HA endpoint delays every later notification and lags the bus (dropped with a warning). Spawn per delivery with a bounded in-flight count.
11. **Transcode on one NVENC engine**: `transcode.rs:70-105 ffmpeg_args` has no `-hwaccel`; the 4 MP HEVC *decode* is software (1.5–3 cores per session) even with `h264_nvenc`; NVDEC sits idle. Add `-hwaccel cuda -hwaccel_output_format cuda` + `scale_cuda` when the encoder is nvenc (and `-threads` for libx264). `-probesize 1000000` on a live pipe delays start by ~3 fragments. Range transcodes run faster than real time, so each scrub in the camera page burns a session for ~10–20 s; 3 sessions and 503 for the 4th is right, but the UI shows the raw error instead of falling back to the substream.
12. **Federation latency**: two hops per media request, `peers()` health probe per Status view (3 s timeout each), Live wall polls `/api/peers/<p>/api/stats` every 3 s per peer per viewer; peer cameras are loaded once at boot (`app.js:729`) and missing until reload if the peer was down; Events merges only the peers' first page (`app.js:510-526`, "Load more" pages local only). Adequate for 2–3 peers; document the paging limit.

## 3. Operability for volunteers: runbook/README vs code

Wrong or misleading:
* **CUTOVER §2 step order fails.** `add-storage /var/cache/zoneminder/zmng ... --archive-to 2` runs before storage 2 exists; `foreign_keys=ON` (`db.rs:429`) makes the `UPDATE storage SET archive_to=2` (`main.rs` AddStorage → `update_storage`) fail with a FK error after the row was inserted. Add the USB storage first, or set `archive_to` in Admin afterwards.
* **CUTOVER §5 "camera: platform: generic on snapshot.jpg with a long-lived token (`Authorization: Bearer`)"** contradicts `deploy/homeassistant.yaml:9-16`, which correctly says the generic camera cannot send headers and steers to the MQTT thumbnail entity.
* README "archived events keep their footage" — only against the age rule (§1).
* README "Set `detect_width`/`detect_height` to 640x360 on cameras with object detection" while the schema default is 320×180 (`db.rs:44-45`) and `objects` defaults to on: out of the box YOLO gets a 320×180 JPEG letterboxed to 640. Flip the default when `[objects]` is configured, or upscale-guard in `ObjectGate`.
* README performance claims (2 %/camera; "22.7 MB served in 0.03 s") are phase-1 single-camera numbers; §10 says nothing in phases 2/3 touched a camera.
* README "Run (development)" and `install-ubuntu.sh` default `listen = 0.0.0.0:8080`, plain HTTP: the substream-MSE live wall needs HTTP/2 (6-connection HTTP/1.1 limit → 6 tiles play, 17 stall with no message). Neither README nor the wall says so; CUTOVER §4 hints at it.
* `deploy/tailscale-acl.example.json` opens 443 while zmng listens on 8080; fine with Caddy/`tailscale serve` but the runbook should say the port mapping explicitly.

Missing:
* `thumb_dir` sizing (previews ≈ 1.2 GB/day, 30-day prune) and where it should live (not the root SSD, or with a budget).
* Upgrade procedure (stop → `zmng backup` → replace binary → start; migrations are automatic and forward-only; keep the old binary). Nothing in CUTOVER/README.
* `doctor` does not check what phase 3 added: `[transcode]` encoder works (`ffmpeg -f lavfi -i testsrc -frames:v 5 -c:v h264_nvenc -f null -`), `[objects].url` answers `/health`, MQTT broker reachable, peers reachable with a *viewer* token, `preview_secs` × cameras vs `thumb_dir` free space, storage path is a mount point.
* A camera that never connected since boot never produces `camera_down` (`main.rs:325-337`: `is_down && !was_down && last != 0`); after a reboot with one dead camera nobody is notified (Status page shows it, but that requires looking). Alert after `alert_after_minutes` from process start.
* `WatchdogSec=`/`sd_notify`, `MemoryMax=`, `TimeoutStopSec=` in the unit (05 M11 partial). A wedged runtime (all workers behind the DB mutex) is not restarted.
* Login limiter is global (`api.rs:47-63`): 10 failures/min from *anyone* on the tailnet locks every user out for a minute — a guest mistyping is a DoS on staff. Per-IP/per-username.
* Detector service install (`deploy/detector/zmng-detector.service` expects `/opt/zmng-detector/bin/python3`), CUDA/onnxruntime version pinning, and how to tell from the Status page that object detection is dead (there is no "objects" line in `report()`; failures are `warn!` only).
* ONVIF prerequisites on Hikvision: enable ONVIF, create a separate ONVIF user (RTSP credentials rarely work), NTP on the camera (WS-Security digest rejects > 5 min skew).
* Restore drill says "motion scores of segments recorded after the backup are lost" — also lost: previews are fine, but `thumb_path` rows for events after the backup are gone while the JPEGs remain (orphan files in `thumb_dir`, never pruned).

## 4. Module boundaries

Right as a first cut; four things to do before it grows:
1. **Extract `auth.rs`** from `api.rs` (sessions, tokens, `LoginGuard`, `visible_cameras`, `can_see`). `zmapi.rs` reaching into `api.rs` through `*_pub` shims (`api.rs:79-92`) is the tell. Then `api.rs` (1.3 k lines) splits naturally into `api/{cameras,media,events,admin,peers,ptz}.rs`.
2. **`ffmpeg.rs`**: every child-process spawn (`thumbs.rs`, `detect.rs` two variants, `zmapi.rs nph_zms`, `transcode.rs`) with the semaphores and stderr plumbing in one place; `thumbs.rs::read_range` is filesystem I/O used by five modules and belongs in an `io.rs`/`mp4.rs`, not "thumbs".
3. **`detect.rs`** carries the pure motion algorithm, the ffmpeg session, the event state machine and thumbnail generation: split `motion.rs` (pure, tested) and `events.rs` (state machine + thumbnail), keep `detect.rs` as the session driver. `objects.rs` depending on `preview::yuv420_to_rgb_tile`/`encode_jpeg` shows the YUV/JPEG helpers want a `frame.rs`.
4. **`main.rs`** holds the 30 s supervisor (alerts, backup, storage_low, reconcile, tiering, retention) — 0 % coverage per review 07 because it is unreachable from tests. Move to `supervisor.rs` with injectable intervals; split tiering into its own task (§2.5).
Smaller: `health.rs` mixes status/metrics with `doctor` and backup/restore/`ServiceLock` → `status.rs`, `doctor.rs`, `backup.rs`. `notify.rs` → `notify.rs` (bus + webhook) and `mqtt.rs`. `recorder::LiveHub` owning `detectors` (`recorder.rs:126-131`) is a layering inversion; rename to `hub.rs` holding all runtime handles, or keep detectors in `detect`. `tier.rs` → `archive.rs` (it is the archive mover; "tier" is also used for storages). `zmapi.rs` → `zm_compat.rs`.

## 5. Biggest remaining risks before cut-over, in order

| # | Risk | Recommendation | Needs the VM/cameras? |
|---|---|---|---|
| 1 | Tiering starves and retention refuses → RAID ENOSPC → all 23 recorders fail (§2.1) | Rate default ≥ 2× ingest / unthrottled under pressure; tiering as its own continuous task; retention hard floor on the primary; Status line "archive backlog (GB, hours)"; a test with a fake 100 MB/s ingest | Yes: ≥ 5 days at full 23-camera ingest with `archive_after_days=1` so the path is exercised inside the parallel week |
| 2 | Unmounted volume → recording onto the root SSD (§2.2) | `RequiresMountsFor`, storage marker file, doctor check, refuse-and-alert | Yes: boot with the USB disk unplugged; unplug it while running |
| 3 | SQLite full scans under the global mutex at 1 M rows (§2.3) | Bounded segment query; `used_bytes` counter; ingest aggregate; read pool via `spawn_blocking` | Partly: a synthetic 1 M-row DB (write_segment in a loop) reproduces it in CI |
| 4 | Live wall default = main-stream decode storm; MJPEG holds decode permits (§2.4) | Snapshot from the sub recorder; default `sub` MSE; separate MJPEG semaphore; state the HTTP/2 requirement | Yes: two staff PCs + one phone on the wall, watch `zmng_camera_*` and load |
| 5 | Runbook errors: `--archive-to` ordering, HA header claim, thumb_dir sizing, no upgrade procedure (§3) | Fix text; add the missing sections | No |
| 6 | Notifications with picture arrive 1–2 min late (M12 still open) | Thumbnail from the open segment via `CamHandle` | Yes: door test with HA on a phone |
| 7 | Federation ACL is per site; admin peer token leaks RTSP URLs to viewers | Startup check that the peer token is a viewer (`GET /api/me`), strip `main_url`/`sub_url` on proxied camera JSON regardless, per-user peer-camera lists or document "one ACL per site" in bold; add `operator` role for archive/notes | No (two VMs) |
| 8 | Transcode decodes 4 MP HEVC in software; nvenc never verified | `-hwaccel cuda` when nvenc; doctor encoder check; UI fallback to substream on 503 | Yes: P2200 with 3 concurrent Firefox viewers |
| 9 | Silent failures: never-connected camera never alerts; objects server dead = warn only; watchdog absent | Alert from process start; `objects` health in `report()`; `WatchdogSec` + `sd_notify` | Partly |
| 10 | Object detection quality at 320×180, zone bottom-centre semantics, `interval_secs` under fleet-wide motion (rain/sunset — the §1 failure mode) | Defaults tied to `[objects]`; measure detector p95 with 23 cameras in motion; drop policy already correct (`objects.rs:175` 200 ms wait) | Yes: real scenes, real GPU |
| 11 | Audio timestamps, HEVC MSE on office browsers, ONVIF on Hikvision, MQTT discovery names in real HA (§10.4) | As listed in §10.4; add G.711 → explicit "audio unsupported" status rather than a warning in the log | Yes |
| 12 | Power loss / restart behaviour at scale: reindex time with 1 M files, WAL loss window, previews | Pull the plug once during the parallel week; time the restart; verify `reindex` count and the Status page afterwards | Yes |

---

## Prioritised table

**Must-fix (before the parallel week is meaningful)**

| Item | Where | Effort |
|---|---|---|
| Tiering rate/loop + retention hard floor + own task | `tier.rs:33,94`, `db.rs:29`, `retention.rs:87-95`, `main.rs:364-375` | 1–2 d |
| Mount-point guard (`RequiresMountsFor`, marker file, doctor) | `deploy/zmng.service`, `recorder.rs:398`, `health.rs doctor` | 0.5 d |
| Bounded `segments_in_range*`/`coverage`/`events_in_range`; `used_bytes` counter; indexed ingest; read pool off the async workers | `db.rs:835-843,884,1086,549-557,909-915,271` | 2 d |
| Snapshot from the sub stream; wall default `sub`; MJPEG semaphore separate from `decode_sem` | `api.rs:790-806`, `app.js:243`, `zmapi.rs:776` | 0.5 d |
| Runbook: storage add order, HA header contradiction, thumb_dir sizing/location, HTTP/2 note, upgrade procedure | `CUTOVER.md` §2/§4/§5, README | 0.5 d |
| Thumbnail from the open segment | `recorder.rs` (`CamHandle`), `detect.rs make_thumbnail` | 1 d |

**Should-fix (before cut-over)**

| Item | Where | Effort |
|---|---|---|
| Federation: viewer-token check, strip URLs on proxied cameras, per-user peer ACL or documented limitation; `operator` role | `api.rs:1202-1235`, `peers.rs`, `api.rs:558-570` | 1–2 d |
| Preview store: budget or shorter prune, atomic sprite write, single-read `nearest` | `preview.rs:150,165-188,240-246`, `retention.rs:56` | 0.5 d |
| Archived footage: honour pin in the space loop *or* export-on-archive; fix README | `retention.rs:105-119` | 0.5–1 d |
| Transcode hwaccel + doctor encoder check + UI 503 fallback | `transcode.rs:70-105`, `health.rs`, `app.js:340` | 0.5 d |
| Supervisor split (alerts/backup/reconcile independent of tier/retention); never-connected alert; objects health line | `main.rs:306-375`, `health.rs:91-182` | 1 d |
| `detect_key` includes object fields; refresh handle camera; `record_audio` restarts the recorder | `detect.rs:57-63`, `recorder.rs reconcile` | 0.2 d |
| Webhook deliveries concurrent with bounded in-flight; shorter timeout | `notify.rs:165-191` | 0.2 d |
| Per-IP/per-user login limiter; `WatchdogSec`/`sd_notify`; `MemoryMax` | `api.rs:47-63`, `deploy/zmng.service`, `main.rs` | 0.5 d |
| Doctor: objects URL, MQTT, peers, transcode, mount points | `health.rs:270-382` | 0.5 d |
| `segment_file` streamed from disk; per-user in-flight media cap (S8) | `api.rs:689-720` | 0.5 d |

**Later**

| Item | Where |
|---|---|
| Module splits: `auth.rs`, `ffmpeg.rs`, `motion.rs`/`events.rs`, `supervisor.rs`, `status.rs`/`doctor.rs`/`backup.rs`, `mqtt.rs`, `hub.rs` | §4 |
| Detector pts/frame pairing off-by-one after a 300 ms stderr stall (drain to latest) | `detect.rs detect_from_recorder` |
| Stagger segment rotation; `fallocate`/`fadvise` | `recorder.rs` |
| Event burst grouping in the Events view (S7); peer events paging | `app.js` |
| PWA: PNG `apple-touch-icon` (iOS ignores SVG), 192/512 PNG in the manifest | `web/` |
| HLS `EXT-X-MAP` per segment (S9); `Accept-Ranges` on `video.mp4` | `video.rs:139-205`, `api.rs:636` |
| Fake RTSP server test for the recorder (review 07 §recorder) | `tests/` |
