# 05 — Design review of `docs/ARCHITECTURE.md` and the `zmng` implementation

*Reviewer's brief: senior-architect critique of the ZoneMinder replacement plan before the VM trial. Inputs: ARCHITECTURE.md, research 00–02, reviews 01–03, QUICK-WINS-PRODUCTION.md, and the code in `zmng/src` + `zmng/web/app.js` (read-only, as of 2026-09-27). Line references are to those files.*

**Overall verdict.** The diagnosis is right and the shape of the design (passthrough fMP4 segments, per-segment index, substream detection, browser-side decode, SQLite) is the correct one; it is the same shape moonfire, Frigate, mediamtx and UniFi converged on. The implementation is a credible 4.4 k-line prototype, not yet a system a volunteer can run. The problems are concentrated in (a) blocking I/O and panic handling inside the single tokio process, (b) the browser strategy — 23 full-resolution HEVC MSE tiles over HTTP/1.1 cannot work, and an unauthenticated go2rtc must not be exposed to guests, (c) several correctness bugs in the recorder/detector/retention paths that would corrupt the index or silently stop a camera, and (d) an operability layer (alerts, backups, HA tokens, disk-full behaviour) that does not exist yet. None of these invalidate the architecture; all of them are must-fix before the VM trial is meaningful. Whether to build zmng at all versus deploying Frigate is a real question; see §7.

---

## 1. Is the root-cause analysis (§1) correct and complete?

**Correct and well evidenced.** Decode-everything with `AnalysisSource` dead, unscaled zones, 243 M `Frames` rows against a 128 MB buffer pool, the no-LIMIT events query, per-viewer zms HEVC decode, and retention-as-emergent-property are all substantiated with file:line evidence in reviews 01–03 and match the live profile (NVDEC 100 %, mysqld 38 %, sda 3.7 k r/s). The "passthrough is already fine" conclusion is also right.

**Omissions and soft spots the plan should state explicitly, because the new design inherits them:**

1. **ext4 at 94–95 % for a year is a fragmentation problem the new design does not automatically fix.** Review 03 §5 makes the case (interleaved unlink holes, growing extent counts). zmng's default `reserve_bytes` is 20 GB (`db.rs:24`, `main.rs:41`), i.e. it will run the 11 TB disk to ~99.8 % of *available* space, which after ext4's 5 % root reserve is the same ~95 % steady state as today. Set the reserve as a percentage (≥ 8–10 %), `tune2fs -m 1` on both volumes, and consider `fallocate` of ~25 MB at segment open (the writer knows the expected size) with `ftruncate` at close so files land as one extent.
2. **Write amplification is small but the write *pattern* is synchronized.** All 23 recorders start together and close segments at ~60 s (`recorder.rs:408`), so every minute 23 files fsync within a second of each other (`recorder.rs:511`). On the RAID this is harmless; on the single SATA disk it is a periodic seek burst. Stagger rotation per camera (moonfire does).
3. **Page cache.** Write-once video that nobody reads back should not compete with the SQLite index and thumbnails for the 125 GB page cache; `posix_fadvise(DONTNEED)` after close costs one syscall. Not urgent, but it is the reason the ZM box shows 93 GB of cache.
4. **fsync policy is not analysed.** `fsync=true` per segment on the async runtime thread is the worst combination (see §2.9); losing 60 s of *index* on power loss is fine because `reindex` re-adopts files (`retention.rs:106`), so the honest position is "fsync for bounded dirty memory, not for durability", and it belongs on a blocking thread.
5. **SQLite WAL/checkpointing is actually the easy part** — a single connection means no reader can starve the checkpointer — but §1 blames MySQL's 38 % on inserts when the profile shows it is the *random reads* (34 GB of index outside a 128 MB pool). The equivalent hazard in zmng is `storage_used_bytes()` (`db.rs:269`): a `SUM(bytes)` over every segment of a volume, not covered by any index, called up to 50 times per volume per 30 s pass (`retention.rs:66-67`). At 20 days × 23 cameras × 1,440 segments ≈ 660 k rows that is a full scan every 30 s. Keep a running counter per storage (or a covering index `(storage_id, bytes)`).
6. **RTSP session budget is missing from §1/§7.** During the parallel run each camera serves: ZM main, ZM substream (opened as an audio source per review 01), go2rtc substream (on demand), zmng main (retina), zmng substream (ffmpeg). Hikvision's default limit is 6 concurrent live-view sessions per camera and some firmware caps the *main* stream lower. §7 says "Hikvision handles 6+"; that is the limit, not headroom. Verify on one camera before pointing zmng at all 23.
7. **GOP length is wrong in the plan.** The cameras are set to GOP 50 at 20 fps (profile 00), i.e. ~2.5–2.8 s fragments, not the "~1 s GOPs" the design assumes (ARCHITECTURE §3.1, §3.4). Live latency, seek granularity, thumbnail accuracy and motion-score alignment are all 2.5–3× worse than stated until the cameras are set to I-frame interval = fps (costs ~10–15 % bitrate on HEVC).

---

## 2. Design decisions (§3, §6), challenged one by one

### 2.1 Absolute epoch-based `tfdt` (64-bit, version 1)
Sound in principle and widely deployed (CMAF live encoders use epoch-based `tfdt`; MSE and HLS both mandate `tfdt` v1 support). 2026 ≈ 1.6e14 ticks fits in 48 bits; `timestampOffset` is a double in seconds (1.79e9 s) with ~0.25 µs precision, far below a 90 kHz tick. Pitfalls, in order of likelihood:
- **Live has no offset applied.** `live.mp4` sends no `X-First-Dts-Ms`, so `MsePlayer.baseSec = 0` (`app.js:65-72`) and the media timeline starts at 1.79e9 s. `wallMs()` happens to be correct by accident. Chrome tolerates huge `currentTime`, but Safari's controls and `buffered` handling with a 1.79e9 s start are untested territory. Parse the first `tfdt` client-side (or send the header) and always offset to ~0.
- **Progressive `<video src=video.mp4>` fallback** (`app.js:378`) hands an fMP4 whose timeline starts at 1.79e9 s to the native player *and* the endpoint has no `Accept-Ranges` (`api.rs:490-496`). Safari requires byte-range support for progressive MP4 and will refuse; iPhones must get the HLS playlist here, not the range MP4.
- `hvc1` vs `hev1`: Safari (MSE and native HLS) accepts only `hvc1`. retina appears to emit `hvc1`; confirm on the VM by inspecting an init segment (`mp4.rs:558` can print the fourcc).
- Clock steps: see §2.9 — the timestamp scheme is only as good as the wallclock anchoring.

### 2.2 No `sidx`
Correct. Nothing in the serving path hands a whole file to a player; the DB index replaces `sidx`. The only consumer that would want `sidx` is an external tool opening a raw segment (VLC/ffmpeg) — they scan `moof`s fine.

### 2.3 60 s segments vs 10 s (Frigate) vs 1 min (moonfire)
Keep 60 s. Fragments (GOPs) are the seek/serve unit, so segment length only sets retention granularity (60 s, fine), file count (33 k/day fleet-wide vs Frigate's ~200 k), and crash exposure of the *index* (≤ 60 s, recovered by reindex). Two consequences the plan does not draw: (a) thumbnails and any "is this time recorded" query lag up to 60 s + GOP — see §2.10; (b) segments should close on a staggered schedule (§1.2).

### 2.4 One fragment per GOP
Right, and it is what MSE wants. With the current 2.5 s camera GOP it is too coarse (§1.7). A `Recent` ring of three fragments (`recorder.rs:494`) is ~8 s of buffer at that GOP; fine.

### 2.5 32-byte-per-fragment index blob vs moonfire's 2 bytes/frame
No capability is lost: the per-sample durations and sizes are in each fragment's `trun` (`mp4.rs:269-273`), so frame-accurate operations can parse the one fragment they touch. What *is* currently keyframe-granular is `frame.jpg` (`api.rs:591` decodes only the fragment's first frame; with a 2.5 s GOP the "Full-res frame" link can be 2.5 s off). Fix by decoding N frames into the fragment (`-frames:v N` from `trun` durations) rather than by storing per-frame rows. The blob is uncompressed (62 MB/day fleet-wide, ~1.2 GB for 20 days on the SSD) — acceptable; varint-pack later if the SSD matters.

### 2.6 SQLite: one `Mutex<Connection>`, recorder inserts + detector updates + retention deletes + API reads
Throughput is not the problem (~23 inserts/min + a few event updates/s). **Latency and blocking are**: `parking_lot::Mutex` (`db.rs:123`) is taken synchronously from inside async tasks (`recorder.rs:518`, `detect.rs:374-410`, every API handler), so a slow statement — the `SUM(bytes)` scan above, an auto-checkpoint, or retention's burst of deletes (`retention.rs:50-83`, run on `spawn_blocking` but contending for the same mutex) — blocks tokio worker threads and every task queued behind them. WAL's reader/writer concurrency is unused because there is only one connection. Recommended shape: one writer connection owned by a dedicated thread fed by an mpsc channel (batch the 23 segment inserts per commit), a small pool of read connections used via `spawn_blocking`, `busy_timeout` stays. `PRAGMA synchronous=NORMAL` (`db.rs:227`) is right; note that a power loss loses the last checkpoint window and reindex will re-adopt those segments *without* motion scores (`mp4.rs:376`).

Other schema/DB findings:
- `delete_camera` (`db.rs:361`) will fail with a FK error while any segment/event references the camera (`foreign_keys=ON`, no `ON DELETE`), although the UI promises "Recordings are removed by retention" (`app.js:394`).
- No `UNIQUE(storage_id, path)` on `segment`; a manual `zmng reindex` while the service runs would adopt the open file and the recorder would insert it again at close (`retention.rs:105` warns but nothing prevents it).
- Keyset paging uses `id <` while sorting by `start_dts` (`db.rs:617-621`); ids and start times are only approximately monotonic across cameras (pre-roll differs), so pages can skip/duplicate at boundaries. Page on `(start_dts, id)`.
- There is no migration mechanism beyond `CREATE TABLE IF NOT EXISTS` (`db.rs:230-234`). Add one before the VM accumulates data you care about.

### 2.7 Detector reads the substream via an ffmpeg child and stamps frames with wallclock at read time
`dts = now - 250 ms` (`detect.rs:333`) versus the recorder's `received_wall`-anchored clock (`recorder.rs:365-372`). The real offset is camera encode + network + ffmpeg's `fps` filter buffering + decode + pipe, and it varies (roughly 200–800 ms). Consequences are bounded — pre-roll (5 s) absorbs it for events, and the timeline bar is 1 s-bucketed — but the peak-frame thumbnail can land in the neighbouring fragment. The clean fix also fixes two security/ops problems at once (§5, §3): pull the substream with retina in-process and pipe Annex-B into `ffmpeg -f h264 -i pipe:0`; then detector frames carry the camera's RTP clock mapped through the same anchor as the recorder, credentials leave the command line, and the same pull can feed a substream recorder.

### 2.8 Motion scores flow into the index only at fragment close (race)
`flush_gop` computes `motion.max_in(dts, dts+duration)` at the moment the *next* keyframe arrives (`recorder.rs:483`), i.e. exactly at the fragment's end, when the detector has not yet delivered the last ~0.3–0.8 s of that interval. Every fragment systematically under-reports its tail. Trivial fix: the blob is only serialized in `close_segment` (`recorder.rs:518`), so recompute every fragment's score from the ring at segment close (the ring holds 4,096 samples ≈ 13 min at 5 fps, `recorder.rs:66`). Also consider storing the per-camera *substream* fragments' scores instead once §2.7 is done.

### 2.9 Recorder correctness (found while checking the above)
- **Write failures are ignored and indexed.** `flush_gop` logs `write failed` and then advances `pos` and pushes the `FragEntry` anyway (`recorder.rs:474-486`). On ENOSPC/EIO the segment row will reference bytes that do not exist. A write failure must drop the fragment, close and discard the segment, and fail the session.
- **A panicking camera task is never restarted.** `tokio::spawn(run_camera)` (`recorder.rs:166`) is fire-and-forget; a panic ends the task but the `CamHandle` stays in the hub, so `reconcile` sees it as running forever (`recorder.rs:140-152`). Same for detectors (`detect.rs:94`). Wrap sessions in `catch_unwind` or supervise the `JoinHandle`, and have reconcile treat `now - last_frame_dts > N s` as dead.
- **Blocking file I/O and fsync on the runtime.** `write_all` (`recorder.rs:474`) and `sync_data` (`recorder.rs:511`) run on tokio workers. With 48 threads it will mostly work; under a slow-disk stall it will freeze unrelated cameras and the HTTP API. Use a per-camera writer thread (std) or `spawn_blocking` for close/fsync.
- **Backward clock steps freeze the index.** Re-anchoring sets `dts = wall_now` (`recorder.rs:385`) but the monotonic guard turns every subsequent frame into `last_dts + 1` (`recorder.rs:390-392`) until wallclock catches up, producing 1-tick durations and a segment that never reaches 60 s (`recorder.rs:408`). On a re-anchor, close the current segment and let the new one start at the new time; adopt moonfire's monotonic-clock-plus-offset model (its `design/time.md`). The unit file also lacks `After=time-sync.target`, and VMs routinely boot with a wrong clock.
- **File name collisions.** Segments are named `HHMMSS.mp4` and opened with `truncate(true)` (`recorder.rs:443-448`); a reconnect within the same second silently overwrites an already indexed file. Use `create_new` and include milliseconds or a sequence.
- `RecvError::Lagged` on the live broadcast just continues (`video.rs:204`), which appends a fragment with a gap; acceptable for live but the client should be told to re-seek to the live edge.

### 2.10 Thumbnail generation waits up to 2 minutes
`make_thumbnail` polls the index every 20 s, six times (`detect.rs:459-463`). For HA notifications ("someone at the door", with picture) a 1–2 minute delay defeats the purpose. The fragment bytes are already on disk (`write_all` completed before the entry was pushed) and the recorder knows their offsets; publish the open segment's path + `Vec<FragEntry>` on `CamHandle` (next to `recent`) and read the fragment from the open file immediately. `Recent` (3 fragments) is too short for the peak of a typical event.

### 2.11 Event state machine
- Pre-roll by subtraction is fine because recording is continuous.
- **The 10-minute cap is measured from `peak_dts`, not from the event start** (`detect.rs:402-403`): an event whose peak keeps rising never ends; one that peaked early ends 10 min after the peak. Use the start.
- On a cap-close the state resets to default so `consecutive` restarts from 0 and the next event begins with a fresh 5 s pre-roll overlapping the previous post-roll; harmless but untidy.
- **Event volume.** With one full-frame zone per camera and a parking lot, "one event per activity burst" means hundreds of events per camera per day (ZM's 2,660/day fleet-wide are 10-minute sections, not bursts). Review 03 proposed Frigate-style review items (merge bursts with gaps < N s, per-camera per-severity); ARCHITECTURE dropped them. Add them, or the events list will be technically instant and practically unreadable.
- Only `kind='motion'`; no camera-side ONVIF/ISAPI events, which the Hikvisions emit and which would gate detection for free (research 01 §17).

### 2.12 Retention: whole segments, oldest-first per volume
- **Fairness**: oldest-first across cameras is fair in *time*, which is what users expect; bitrate only changes how much each camera contributes. Fine. But with `retention_days` defaulting to 30 (`db.rs:34`) and ~17 days of capacity, the age rule never fires and effective retention is again emergent — surface the effective depth (oldest segment per camera) in the UI and alert when it falls below a target.
- **Static camera→volume assignment** (`camera.storage_id`) means the two volumes drain at different rates unless bitrates are balanced by hand. The schema already supports per-segment `storage_id`; place each new segment on the volume with the most headroom and the "never copy" property still holds.
- **Archived events lose their footage** (`retention.rs:87-96` only spares the row). Pinning segments is the wrong tool: pinned 60 s files accumulate outside the byte budget and the oldest-first loop would have to skip them forever. Export-on-archive instead: when a user archives an event, copy its fragments (init + range) into `thumb_dir/archive/<event>.mp4` (typically 5–100 MB) and serve archived events from there. Simple, honest, and it survives volume loss.
- **Unlink failure deletes the row anyway** (`retention.rs:25-30`): on EROFS/EIO the file becomes an orphan the index no longer knows about. Keep the row (or a `garbage` table like moonfire) and retry.
- **Disk-full path.** If free < reserve and no segments remain to delete (other data on the volume, or a foreign process filling it), the loop only warns (`retention.rs:76`) and recording proceeds to ENOSPC, which then hits the bug in §2.9. There must be a floor below which the recorder pauses writing to that volume and raises an alert.

### 2.13 One process vs process-per-camera
One tokio process is the right call (moonfire, go2rtc, mediamtx) *provided* blocking work leaves the runtime and panics are supervised; today neither holds (§2.9). Decoders/encoders stay in child processes — correct. A CUDA context for phase-2 detection should live in its own process for the same reason.

### 2.14 Single node
Appropriate for a church. Nothing in the schema prevents a later federating UI. Do not spend time on it.

---

## 3. Browser strategy

1. **The 23-tile full-resolution HEVC MSE grid cannot work on client hardware.** The default live mode is `mse` when `hevcMse` is true (`app.js:116`, `:137`), so a staff Mac would try to decode 23 × 2688×1520 × 18 fps ≈ 1.7 Gpx/s — 3–4× what a laptop's HEVC decoder does. Grid tiles must be substream (research 02 appendix D says exactly this); main-stream HEVC is for the single-camera view only.
2. **HTTP/1.1 connection limit.** Live uses one long-lived `fetch` per tile (`app.js:61`) plus `/api/stats` polling. Browsers cap HTTP/1.1 at 6 connections per origin; without TLS there is no HTTP/2 (axum does not do h2c by default). Over Tailscale on port 80 the grid will stall at 6 tiles. Either move live to WebSockets (separate limit) or terminate TLS/HTTP/2 in front (Caddy on the LAN; `tailscale serve` for the tailnet, which also gives guests HTTPS for free).
3. **Probe, don't trust `isTypeSupported`.** `support.hevcMse` (`app.js:47`) is exactly the check the plan says is insufficient (ARCHITECTURE §7; StaZhu's Windows/D3D11 caveat). Do the real probe once per device: append the camera's init + one fragment to a hidden `SourceBuffer`, require `readyState ≥ 2` within ~1.5 s, persist the result, and offer a manual override. Chrome on Linux and Edge without the HEVC extension will land on the fallback path either way.
4. **Mobile Safari.** iPhones expose `ManagedMediaSource` (iOS 17.1+), not `window.MediaSource`; `MsePlayer` never uses it, so iPhones fall to native HLS for review (good) and to the broken progressive-MP4 path for events (§2.1). There is no *live* HLS playlist at all (`video_hls` is VOD-only, `video.rs:129-174`), so iPhone live is snapshots at 2 s. Add a sliding live playlist from the open segment (needs the open-segment index from §2.10) or route iPhone live through go2rtc/WebRTC.
5. **HLS playlist hygiene.** A new `EXT-X-MAP` URI on every 60 s segment (`video.rs:150-153`) is technically a new initialization section per segment; Apple's validator flags that without a discontinuity. Serve the init from one stable URL per `sample_entry` (`/api/sample-entries/{id}/init.mp4`) and emit `EXT-X-MAP` once. Confirm cookies are sent by the iOS media stack for `/api/segments/{id}/file.mp4` (they normally are for same-origin HttpOnly cookies; Bearer never is).
6. **go2rtc as a separate process.** Acceptable for WebRTC *only if* zmng proxies it. Today the UI embeds `http://<host>:1984/stream.html` in an iframe (`app.js:146`) using ZM's stream names (`<id>_CameraDirectSecondary`), which (a) requires opening 1984 to guests in the Tailscale ACL, (b) exposes go2rtc's unauthenticated API including `/api/streams` with RTSP credentials — contradicting ARCHITECTURE §3.6 — and (c) breaks at cut-over when ZM's go2rtc.yaml goes. Bind go2rtc to 127.0.0.1, have zmng write its config and proxy `/api/ws` (WebSocket) and `/api/webrtc` with zmng auth, or drop it and do MSE-over-WebSocket in zmng (WebRTC is only needed for sub-second latency and two-way audio, neither a stated requirement).
7. **Substream recording should be phase 1, not deferred.** One retina pull of the substream per camera, shared by (i) the detector (§2.7), (ii) a second segment writer, and (iii) grid live via the same live path, buys: a working 23-tile grid without go2rtc, playback on every browser (H.264 640×360 decodes everywhere, including Linux Chrome and Firefox), Blue Iris-style multi-camera synchronized review (which the plan otherwise cannot do at all), and a scrub source. Cost: ~124 GB/day (~13 % of capacity) and ~300 lines (the writer is codec-agnostic already). It also cuts the per-camera RTSP session count.
8. **Server-side JPEG paths are back-door ZM patterns.** `snapshot.jpg` every 2 s per tile (`app.js:176`) is 11.5 ffmpeg spawns/s per viewer of 4 MP HEVC I-frame decodes; the review strip fires up to 24 `frame.jpg` per timeline refresh and `refresh()` runs on every wheel tick (`app.js:283`, `:292`). Add a global semaphore (4–8 concurrent decodes), single-flight + 1 s cache for snapshots, and canonicalize `frame.jpg?t=` to the fragment's `dts` so `max-age` caching actually hits. With substream recording most of these become substream decodes.
9. **MSE quota.** The review player fetches 10-minute ranges (`CHUNK`, `app.js:220`) ≈ 220 MB of 4 MP HEVC; Chrome's SourceBuffer quota is ~150 MB. `pump()` catches `QuotaExceededError`, logs it and drops the chunk (`app.js:89`), after which the remaining appends are mid-box garbage. Handle quota by pausing appends and `remove()`-ing behind the playhead, and fetch 1–2 minute windows chained on `ended`.

**Target client matrix** (what the UI should select per view once the above is done; "probe" = real init+fragment append test, persisted per device):

| Client | Grid live | Single live | Review / events | Notes |
|---|---|---|---|---|
| Safari macOS | sub H.264 MSE (or WebRTC) | main HEVC MSE (`hvc1`) | main HEVC MSE, HLS fallback | HEVC universal on Apple silicon and T2 Macs |
| Chrome/Edge Windows with HEVC ext. + GPU | sub MSE | main HEVC MSE after probe | same | probe mandatory (StaZhu caveat) |
| Chrome Linux, Firefox, Chrome without HEVC | sub MSE | sub MSE; phase-3 NVENC H.264 of main | sub recording (S1) or NVENC | today: snapshots only |
| iPhone/iPad Safari ≥ 17.1 | sub via `ManagedMediaSource` or live HLS | main via HLS (`hvc1`) | HLS VOD playlist | `window.MediaSource` absent on iPhone |
| iPhone Safari < 17.1 | live HLS | HLS | HLS VOD | no MSE at all |
| Android Chrome | sub MSE | main HEVC MSE if SoC decodes it (probe) | MSE / HLS via hls.js | many SoCs decode HEVC; probe |
| Home Assistant | go2rtc/WebRTC card or `live.mp4` | — | `snapshot.jpg` + events via MQTT | needs long-lived token (S2) |

---

## 4. Migration plan (§5)

### 4.1 Adopting ZoneMinder event MP4s as legacy segments
Feasible, but not as "no rewriting":
- ZM files rebase timestamps to ~0 per file. Serving them under absolute time via the range endpoint means **patching each `tfdt` (+ StartDateTime×90k) as the `moof` is copied** — an 8-byte in-place edit at a known offset; `RangeItem::Frag` needs a `tfdt_delta`. HLS byte-range playlists cannot patch, so legacy files get one `EXT-X-DISCONTINUITY` per file plus `PROGRAM-DATE-TIME`, which hls.js and Safari handle.
- **Timescale must be checked per file.** If ffmpeg chose a track timescale other than 90000 the patch becomes a rescale of `tfdt` *and* every `trun` duration; store the timescale per legacy segment and refuse the byte-patch path when it differs.
- **Audio.** Review 01 notes ZM opens the substream as an audio source when the main stream has none; legacy files may carry two tracks. `plan_range` regenerates the init from `sample_entry` (`video.rs:60-75`), which would drop the audio `trak` and desynchronize the `traf`s. Legacy segments must use the file's own `ftyp+moov` bytes as init (`init_len` already exists) and a two-track init cannot be spliced with native single-track segments without a discontinuity.
- **`hev1` vs `hvc1`.** Verify with `ffprobe` on a real file; if `hev1`, Safari will not play it natively and a fourcc patch of the `stsd` entry is a gamble because the in-band parameter sets remain.
- **Scanning.** `adopt_file` reads whole files into memory (`retention.rs:142`); 60 k × 240 MB = 14 TB of reads is not a plan. The importer needs a seeking box walker (or the `mfra`/`tfra` trailer, or the fork's `index.m3u8`) as review 03 §7 describes. The same seeking scanner should back `reindex` for large files.
- Frames→events coalescing per review 03 §7 is correct; events with `AlarmFrames=0` produce nothing; thumbnails from `snapshot.jpg`.

### 4.2 Parallel run
- RTSP session budget (§1.6) is the real risk; test the per-camera limit first.
- zmng writing into the RAID while ZM's `PurgeWhenFull`/`AutoMove` are active means ZM will delete its own events to make room for zmng's. Stop `MoveToDiskWhenFull` (quick win D14) *before* the parallel run, and give zmng an explicit `max_bytes` on that volume.
- Two systems, two clocks: both anchor to the server's clock, fine; but ZM's `StartDateTime` is 1 s resolution and local time — the importer must convert using the server's zone (`Event.pm` rule), including DST transitions.

### 4.3 Cut-over risks
- **Tailscale ACL**: today 80 + 10001–10023; zmng listens on 8080 (`config.rs:41`). Either `AmbientCapabilities=CAP_NET_BIND_SERVICE` and listen on 80/443, or a reverse proxy; do not open 1984 (§3.6). `tailscale serve` is the cleanest guest path (HTTPS, HTTP/2, no ACL port games).
- **Guest accounts**: ZM password hashes cannot be imported into argon2; every user gets a reset. Plan the communication.
- **Home Assistant**: the ZM HA integration dies; HA needs `snapshot.jpg` (works, but only with a session token that expires in 14 days — there is no long-lived API token table), a motion/occupancy signal (nothing yet: no MQTT, no webhook), and a stream (go2rtc or `live.mp4`). Long-lived tokens + MQTT publish of `camera/<id>/state` and `event` are a cut-over prerequisite, not a "small addition".
- Viewers can archive and annotate events for their cameras (`api.rs:438-449`); the guest group should probably be read-only. Add an `operator` role or per-user flags.

### 4.4 Cut-over checklist (what "after parallel run" should actually mean)
1. Per-camera RTSP session count verified with all consumers attached; GOP set to fps; H.265+ off.
2. zmng has recorded all 23 cameras for ≥ 7 days with zero unsupervised camera stalls (M2) and no write errors (M1); effective retention per volume matches the model.
3. HEVC MSE probe results collected from every staff machine type; iPhone playback (live + review + events) verified by a real guest.
4. Guest group re-created in zmng with per-camera assignments; each guest has logged in once over Tailscale via `tailscale serve`/HTTPS before ZM is stopped.
5. HA switched to zmng tokens/MQTT and validated (snapshot, motion sensor, stream) while ZM still runs.
6. Backup timer running and one restore rehearsed on the VM.
7. Alerts firing to a real phone (unplug one camera; fill a test volume).
8. Legacy import done idempotently against read-only MySQL; a sample of old events plays in Safari and Chrome.
9. Only then: stop `zmc`/`zma`/`zmfilter`, hand both volumes to zmng (`add-storage` with percentage reserves), update the Tailscale ACL, keep MySQL read-only for 30 days as a fallback.

### 4.5 Missing from the plan
- **Backups of the SQLite index**: the segments can be rebuilt by reindex (without motion scores), but events, notes, archives, users, ACLs, camera config and zones exist only in `zmng.db`. Nightly `VACUUM INTO` to the *other* volume plus an off-box copy; the DB contains RTSP credentials, so protect the copies.
- **Monitoring/alerting**: `CamStatus` has everything needed (`recorder.rs:41-53`) but nothing acts on it. Needed: "camera X no frames for N min", "detector down", "volume headroom < X / effective retention < Y days", "service restarted", delivered via ntfy/email/HA webhook; a `/metrics` endpoint; systemd `WatchdogSec` + `sd_notify`.
- **Disk-full behaviour**: §2.12 — pause writes on a volume below a hard floor, alert, and never index a failed write.
- **Time sync**: chrony on server and cameras; `After=time-sync.target` in the unit; refuse to start recording if the clock is before the build date.
- **Log rotation**: journald handles it; set `SystemMaxUse` and make the `tracing` filter default `info,retina=warn` less chatty for `ffmpeg` stderr (currently `debug`, `detect.rs:312`).
- **Upgrade path**: schema migrations (§2.6), a `zmng check-config`, and a way to run the binary once against a copy of the DB.

---

## 5. Security review

- **Auth model**: argon2 defaults (`api.rs:37-51`), 32-byte random tokens, HttpOnly cookie `SameSite=Lax`, Bearer for automation, per-camera fail-closed visibility (`api.rs:113-127`) — sound. `visible_cameras()` hits the DB on every request; cache per session.
- **No login rate limiting** (`api.rs:148-168`): argon2 at ~50–100 ms per attempt on 48 threads is both a brute-force and a CPU-DoS surface for anyone on the tailnet. Add per-IP and per-username throttling with a lockout, and log failures.
- **Setup window**: while `user_count()==0` *every* request is admin (`api.rs:83-88`); the first LAN host to reach the VM owns it. Restrict `/api/setup` to loopback, or require the CLI `add-user` (which exists, `main.rs:114`).
- **CSRF**: good by construction — explicit `SameSite=Lax`, JSON-only bodies (a form cannot send `application/json`; cross-origin fetch would preflight and there is no CORS layer), no state-changing GETs. Keep it that way; if a CORS layer is ever added for HA, restrict origins.
- **RTSP credentials**: plaintext in `camera.main_url` (acceptable at rest if the DB is 0600 — add `UMask=0077` to the unit — and backups are protected), returned only to admins (`api.rs:230-234`). **But the detector passes the full URL on ffmpeg's command line** (`detect.rs:294`), visible in `ps`, `/proc/*/cmdline` and crash logs to any local user. Fix via §2.7 (retina pull, pipe to ffmpeg), or at minimum an ffconcat file with 0600 permissions. `last_error` strings are currently clean because the recorder strips credentials before DESCRIBE (`recorder.rs:255-257`); keep that invariant under test.
- **Path handling**: media by id, paths from the DB joined to storage roots, `ServeDir` for the SPA — no traversal surface found. `update_camera` interpolates only allow-listed column names (`db.rs:331-341`). Fine.
- **DoS**: `video.mp4` allows 6 h (~8 GB) per request with no per-user concurrency cap; `segment_file` reads the whole requested range into memory (`api.rs:527`; a legacy 240 MB file with no `Range` header is 240 MB of RAM per request); `frame.jpg`/`snapshot.jpg` spawn unbounded ffmpeg processes (§3.8). Add a per-user in-flight limit, stream `segment_file` from disk, and a decode semaphore. `Content-Length` is pre-computed for `video.mp4` (`api.rs:492`), so a retention delete mid-stream produces a truncated body; use chunked transfer or defer retention of segments served in the last minute.
- **TLS**: none in zmng; `secure_cookies=false` by default. On the LAN this is cleartext credentials. Decide now: Caddy (or nginx) in front on the LAN with a local CA or a real cert, `tailscale serve` for guests; then set `secure_cookies=true`. This also solves HTTP/2 (§3.2).
- **Session lifetime** 14 days (`config.rs:48`) on shared church PCs is long; add idle expiry and a "sign out everywhere".

---

## 6. Operability for a volunteer-run system

What exists: TOML for process settings, DB for cameras/users/storage, a CLI for bootstrap (`main.rs:27-69`), a hardened systemd unit, reindex on start, `tracing` to journald, an admin page with per-camera status and a browser capability line (`app.js:385-410`).

What is missing, in priority order:
1. **Health dashboard and alerts** (§4.4): a single page a volunteer can read — per camera: recording yes/no since when, fps/bitrate, detector fps, last event; per volume: used/free/effective days; service uptime; last errors. Plus push notifications for camera-down > N min and low headroom.
2. **Backup/restore** as first-class CLI verbs with a systemd timer, and a documented restore drill.
3. **Disk usage forecast**: daily ingest per camera is known from segment rows; show "at current rates you keep ~16 days; adding camera X costs Y".
4. **Upgrade path**: versioned schema migrations, `zmng --version` in the health JSON, a release checklist, and a rollback story (keep the previous binary; the DB is forward-only).
5. **Camera onboarding**: zone drawing is "paste JSON" (`app.js:414`); a volunteer needs a canvas over a snapshot. ONVIF discovery is nice-to-have.
6. **Bus factor**: the whole thing is one person's Rust. Whatever is chosen (§7), write the operator runbook before cut-over.

---

## 7. Alternatives not taken — an honest verdict

**Frigate (with `-tensorrt` or the ONNX/CUDA detector).** Gives, today: passthrough recording (10 s segments, `hvc1` forcing), substream motion + object detection, review items, hourly previews, export, HA integration (MQTT + official integration), go2rtc restream with one pull per stream, camera roles for per-camera access, a mature mobile-capable UI, and a community. It does *not* give: two-volume no-copy retention (single media path — solvable with LVM/mergerfs at the cost of losing the "important cameras on the RAID" distinction), import of the 14 TB of ZM events (they would stay browsable only in a read-only ZM until they age out), or SQLite-simple ops (it is Docker + Python + nginx). Detection on a Pascal card: TensorRT 10 dropped SM 6.1, but Frigate's TensorRT images and the ONNX detector with the CUDA EP have run on GTX 10-series; treat it as "verify on the VM in an afternoon", and note research 01 §6 already recommends a ~$100 Arc A310 for detection regardless.

**moonfire-nvr + Frigate** means two ingest paths, two UIs and two retention systems for the same cameras — not recommended. **mediamtx as ingest/record** is a solid RTSP/WebRTC hub and could replace go2rtc, but its playback (no Range, age-only cleaner, no motion index) means you would still write everything zmng writes except the RTSP client.

**Verdict.** Building zmng is justified only if *all* of these hold: (1) the owner wants to own a Rust service for years and treats the must-fix list below as a few weeks of work, not a weekend; (2) the two-volume policy and/or the legacy import are genuine requirements rather than preferences; (3) full-resolution HEVC in the browser without go2rtc matters more than Frigate's object-detection UX. If any of those is soft, deploy Frigate: it is the 80 % solution with a support forum. The cheapest way to decide is empirically — run Frigate on 3 cameras and zmng on 3 cameras on the VM for the same week, with the same guest, staff and HA workflows, and compare. Either way, phase 0 (quick wins) should happen now; it is independent of the choice.

---

## 8. Prioritized changes

### Must-fix before the VM trial (≈ 2–3 weeks of focused work)
| # | Change | Why | Effort |
|---|---|---|---|
| M1 | Fail the segment/session on write error; never index a failed fragment (`recorder.rs:474-486`) | Index corruption on ENOSPC/EIO | 0.5 d |
| M2 | Supervise camera/detector tasks: `catch_unwind` + `JoinHandle`, and restart when `last_frame_dts` is stale (`recorder.rs:166`, `detect.rs:94`) | A panic silently stops a camera forever | 1 d |
| M3 | Move file writes/fsync and SQLite off the tokio workers: per-camera writer thread, single DB writer thread + read pool (`recorder.rs:474,511,518`, `db.rs:238`) | Latency/stalls across cameras and API | 3 d |
| M4 | Running `used_bytes` counter per storage (or covering index) (`db.rs:269`, `retention.rs:67`) | Full table scan every 30 s | 0.5 d |
| M5 | Motion scores recomputed at segment close (`recorder.rs:483`, `:518`); 10-min cap from event start (`detect.rs:403`) | Systematic under-reporting; runaway events | 0.5 d |
| M6 | Substream via retina in-process, piped to ffmpeg; feeds detector now, recorder next (`detect.rs:290-303`) | Credentials off `ps`, timestamp alignment, one fewer RTSP session | 2–3 d |
| M7 | Grid tiles = substream (go2rtc-proxied or substream recording); main HEVC only in single view; real MSE probe with persisted result (`app.js:116,137,47`) | 23 × 4 MP HEVC decodes is impossible client-side; `isTypeSupported` lies | 2 d |
| M8 | Live over WebSocket *or* TLS/HTTP/2 in front (Caddy / `tailscale serve`); `secure_cookies=true` | 6-connection HTTP/1.1 limit kills the grid; cleartext auth on LAN | 1–2 d |
| M9 | go2rtc bound to loopback and proxied with zmng auth, or removed (`app.js:146`) | Unauthenticated credential leak to guests | 1 d |
| M10 | MSE quota handling + 1–2 min chained ranges (`app.js:89,220`); iPhone event playback via HLS, `ManagedMediaSource` support (`app.js:378`) | Playback breaks after ~150 MB; iPhones cannot play events | 1.5 d |
| M11 | Login throttling; `/api/setup` loopback-only; `UMask=0077`; `After=time-sync.target`; `WatchdogSec` | Basic hardening before guests touch it | 1 d |
| M12 | Thumbnails from the open segment (publish open-segment index on `CamHandle`) (`detect.rs:434-465`) | 2-minute thumbnails make notifications useless | 1 d |
| M13 | Backward clock step: close segment on re-anchor, monotonic+offset clock (`recorder.rs:380-393`) | Frozen timestamps after an NTP step | 1 d |
| M14 | `create_new` segment files with ms/seq in the name (`recorder.rs:448`); `UNIQUE(storage_id,path)` | Silent overwrite of indexed files | 0.5 d |

### Should-fix before cut-over (≈ 3–4 weeks)
| # | Change | Why | Effort |
|---|---|---|---|
| S1 | Substream recording (second writer per camera) + multi-camera synchronized review view | Grid, mobile, weak clients, Blue Iris-style review | 1–2 w |
| S2 | Long-lived API tokens; MQTT/webhook publish of camera state and events; documented HA recipes | HA integration otherwise breaks every 14 days and has no motion signal | 3 d |
| S3 | Alerts (camera down N min, detector down, low headroom, restarts) via ntfy/email/HA; `/metrics`; health page | Volunteers will not read journald | 1 w |
| S4 | Backups: `VACUUM INTO` timer to the other volume + off-box; restore runbook | The DB is the only copy of events/users/config | 1 d |
| S5 | Retention: percentage reserve, hard floor that pauses writes, garbage table on unlink failure, placement by headroom across volumes, effective-days display | Steady state at 99 % full and silent orphans | 3 d |
| S6 | Export-on-archive for archived events | "Archived" currently means metadata only | 1 d |
| S7 | Review items (burst grouping) + per-day counts in the events UI | Hundreds of raw motion events per camera per day | 3 d |
| S8 | Decode semaphore + single-flight snapshot cache + canonical `frame.jpg` URLs; stream `segment_file` from disk; per-user in-flight limits | Server-side JPEG storms and memory blow-ups | 2 d |
| S9 | Stable init URL per `sample_entry` in HLS; live HLS playlist for iOS; verify `hvc1` and cookie behaviour on real iPhones | iOS correctness | 2 d |
| S10 | Schema migrations; `delete_camera` semantics; `(start_dts,id)` keyset paging; `operator` role | Data model hygiene before real data | 2 d |
| S11 | Legacy import: seeking scanner, per-file timescale check, own-init for two-track files, `tfdt` patching in `RangeItem::Frag`, discontinuity playlists | Only way the 14 TB stays useful | 1–2 w |
| S12 | Camera GOP = fps, H.265+ off, per-camera RTSP session test; stagger segment rotation | Latency, alignment, disk pattern | 0.5 d + camera visits |

### Nice-to-have
- `fadvise(DONTNEED)` after close; `fallocate` at open; `tune2fs -m 1`.
- Varint-packed index blobs; frame-accurate `frame.jpg` from `trun` durations.
- ONVIF/ISAPI camera events as detector gates; zone editor over a snapshot; disk-usage forecast; PWA shell for phones; audio passthrough.
- Prometheus/Grafana dashboard; `zmng doctor` (config, permissions, clock, ffmpeg presence, camera reachability).

---

## Appendix — code observations by file (for whoever picks up the must-fix list)

`zmng/src/recorder.rs`
- `:166` fire-and-forget `tokio::spawn(run_camera)`; no `JoinHandle`, no `catch_unwind` (M2).
- `:305` 20 s no-packet timeout is good; `:274` `enforce_timestamps_with_max_jump_secs(10)` will end the session on Hikvision RTP jumps — expected, but expose `reconnects` in the health page.
- `:365-372` anchor = first frame's `received_wall`; `:380-389` re-anchor at keyframe after 30 s of >1 s drift; `:390-392` monotonic guard turns a backward step into 1-tick durations (M13).
- `:397` per-sample duration clamped to [1 tick, 2 s]; a forward step > 2 s therefore silently shortens the last frame — acceptable, but log it.
- `:408` segment closes at first keyframe ≥ 60 s; not staggered across cameras (S12).
- `:443-448` `HHMMSS.mp4` + `truncate(true)` (M14).
- `:474-486` write error logged, entry still indexed (M1); `:483` motion score computed at the instant the fragment ends (M5).
- `:511,518` `sync_data` and SQLite insert on the async runtime (M3).
- `:494` `Recent` keeps 3 fragments; sufficient for live start, insufficient for thumbnails (M12).

`zmng/src/mp4.rs`
- `:257` `tfhd` default-base-is-moof, `:261-263` `tfdt` v1 64-bit, `:266-273` `trun` with per-sample duration/size/flags — correct CMAF-style fragments.
- `:105-111` brands `iso5/iso6/mp41/dash`; fine for MSE/HLS.
- `:350-386` scanner tolerates a truncated trailing fragment and sets `motion: 0` on adoption.
- `:558-570` sample-entry extraction — use it to assert `hvc1` on the VM.

`zmng/src/db.rs`
- `:123` single `parking_lot::Mutex<Connection>`; `:227` WAL + `synchronous=NORMAL` + `busy_timeout=5000`.
- `:269-277` `SUM(bytes)` per storage, no covering index (M4).
- `:361-366` `delete_camera` fails on FK while segments exist (S10).
- `:596-628` keyset paging on `id` while ordering by `start_dts` (S10).
- `:64-81` no `UNIQUE(storage_id,path)`; no `pinned`/`garbage` concepts.

`zmng/src/detect.rs`
- `:290-303` ffmpeg spawned with the RTSP URL (credentials) as an argument (M6); `:312` ffmpeg stderr at `debug`.
- `:333` `dts = now - 250 ms` wallclock stamping (§2.7).
- `:372-385` event start after 2 consecutive motion frames, pre-roll by subtraction; `:402-403` 10-min cap measured from `peak_dts` (M5).
- `:434-465` thumbnail waits for the segment to be indexed, up to 6 × 20 s (M12).
- `:131-216` detector core is small, deterministic and unit-tested — keep.

`zmng/src/video.rs`
- `:46-88` `plan_range` regenerates the init from `sample_entry` — wrong for legacy two-track files (S11).
- `:91-121` blocking reads on `spawn_blocking`, 8-item channel — fine; note the reader keeps going until the consumer drops.
- `:129-174` HLS: `EXT-X-MAP` per segment (S9), discontinuity on > 0.5 s gaps, `PROGRAM-DATE-TIME` present — good.
- `:177-210` live: init + last fragment, `Lagged` skipped silently.

`zmng/src/retention.rs`
- `:23-35` unlink failure other than `NotFound` still deletes the row (S5).
- `:47-61` per-camera age loop is index-backed (`segment_cam_end`); `:64-83` space loop calls `storage_used_bytes` per round (M4).
- `:87-96` orphan events deleted unless archived; archived events keep metadata only (S6).
- `:106-158` reindex reads whole files into memory (S11 scanner) and runs before recorders start — correct ordering.

`zmng/src/api.rs`
- `:74-99` auth middleware; `:83-88` no-users → everyone is admin (M11).
- `:148-168` login without throttling; `:161-164` cookie flags correct, `Secure` off by default.
- `:226-236` credentials stripped for non-admins — correct.
- `:476-497` `video.mp4`: 6 h cap, `Content-Length` pre-computed, no `Accept-Ranges` (§2.1, §5).
- `:511-541` `segment_file`: whole range read into memory (S8).
- `:565-603` `snapshot.jpg`/`frame.jpg`: one ffmpeg per request, no semaphore or cache (S8).
- `:438-449` viewers may archive/annotate (S10 role).

`zmng/web/app.js`
- `:47` `isTypeSupported` only (M7); `:56-109` `MsePlayer`: `timestampOffset = -baseSec` (`:72`), quota errors swallowed (`:89`), live `baseSec = 0` (`:65`).
- `:116,137` default grid mode is full-res MSE for every tile (M7); `:146` go2rtc iframe on port 1984 with ZM stream names (M9).
- `:220-234` 10-minute range fetch per seek (M10); `:230` native HLS only when MSE is absent; `:378` progressive `video.mp4` fallback (M10).
- `:283,292` thumbnail strip re-fetched on every wheel tick (S8).

`zmng/deploy/zmng.service`, `config.rs`
- Good hardening baseline; add `UMask=0077`, `After=time-sync.target`, `WatchdogSec`, `AmbientCapabilities=CAP_NET_BIND_SERVICE` if listening on 80/443, `MemoryMax`, `IOSchedulingClass=best-effort`.
- `config.rs:48` 14-day sessions; `:50` `secure_cookies=false`; `:44` `segment_secs=60` (keep).
