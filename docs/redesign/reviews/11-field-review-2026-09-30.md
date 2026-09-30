# Field review of the side-by-side test — 2026-09-30

zmng has run next to ZoneMinder on securityserver since 2026-09-28 21:45 ET (Front Door = ZM 1,
Playground = ZM 9). This review covers the live UI (browsed at 1440×900 and phone width), the server,
detection, and zmNinjaNg, after two days of operation. Everything under "Fixed" is deployed on the
test instance (binary sha256 `f294ddec…af45d1`, 2026-09-30 18:42 ET) and covered by tests.

## How it has been running (40 h)

| | Front Door | Playground |
|---|---|---|
| Recording | 18 fps HEVC 2688×1520 + 640×360 H.264 sub, **0 reconnects** | same, **0 reconnects** |
| Motion events / 24 h | 66 | 34–38 |
| ZoneMinder, same camera, 72 h | alarms in 78 of ~430 ten-minute sections | alarms in **4** sections |
| People (after this review's detector) | 87 events | 62 events |
| Vehicles | 0 | 46 |

Storage behaves as designed: the 40 GB RAID tier sits at its cap (holds 1.0 day), tiering moves
~190 MB every ~3.5 min to the USB tier (79.5 / 150 GB, ~6 days), backlog 0. ZoneMinder monitors 1/9
stayed Connected at ~18 fps throughout.

## Problems found, root causes, fixes

1. **Intermittent HTTP 500 on live snapshots** (the "response failed" lines in the journal; a black
   Front Door tile on Live). Ubuntu's ffmpeg links a library that starts an OpenMP pool of one thread per
   core in every process: each snapshot ffmpeg ran ~130 threads and burned ~4.6 s of CPU (mostly thread
   start-up) for one 640 px JPEG, and the two detector ffmpegs alone held 258 threads. Concurrent
   snapshots then hit the unit's `TasksMax=512` → `ff_frame_thread_encoder_init failed … Resource
   temporarily unavailable`. **Fixed:** every ffmpeg child gets `OMP_NUM_THREADS=1` plus explicit
   `-threads/-filter_threads 1` (`thumbs::ffmpeg_command`). zmng tasks 308 → 58, detector ffmpeg
   129 → 4 threads, snapshot CPU 4.6 s → 0.35 s. Live JPEGs are also cached per keyframe and decoded once
   per key (`api::live_jpeg`): 16 concurrent snapshot requests → all 200 in 0.27 s.
2. **"People" found nothing, although event 229 (Front Door 17:08:52) plainly shows two people.**
   Object detection was never running: the test `zmng.toml` had no `[objects]` section and no detector
   was installed, so every event stayed `motion`. Not a ZoneMinder-protection choice, just not set up.
   **Fixed:** `zmng-detector` (YOLO11s on onnxruntime **CPU**, nice 19, 1 GB cap, localhost only — the
   GPU is ZoneMinder's) and a second detection pass on every finished event's 640 px main-stream
   thumbnail. A backfill labelled 156 of 229 past events; event 229 is `person 0.91`. The UI hides object
   filters when no detector is configured, and Status reports "detector failing" when it stops answering.
   The kind filter now matches any label in the event (a person-and-car event is found under Vehicles).
3. **Review: unticking Playground did nothing until Apply.** **Fixed:** cameras and groups update the
   view immediately and keep the playhead; dates and the new *Show* filter still wait for Apply (the
   button is highlighted while there are unapplied edits).
4. **Review: the "only cameras with motion in range" checkbox sat above its label.** Global CSS gave
   every `<input>` `width:100%`, checkboxes included. **Fixed** (also the full-width selects that stacked
   the Events and Live toolbars one per line).
5. **Admin layout.** A 4-column auto-fit grid left the right quarter empty and clipped the Storage table
   under the Browser card (Free/Cap/Reserve/Tiering and the storage **Edit** buttons were unreachable).
   **Fixed:** full-width sections, scrollable tables, Users + Object detection side by side, diagnostics
   folded away. The camera editor is sectioned (General, Streams, Retention, Motion, Objects, ONVIF),
   masks RTSP passwords behind "show", and closes on Esc.
6. **zmNinjaNg: slow camera loading, streams labelled "MJPEG".** The app picks go2rtc only when a
   monitor has `Go2RTCEnabled` and `ZM_GO2RTC_PATH` is set; we advertised neither, so MJPEG was the only
   path. Its snapshot tiles (`nph-zms?mode=single`) decoded the 4 MP HEVC main stream every 3 s per tile
   (with the thread problem above on top), only 3 MJPEG streams were allowed, and the app's stream "quit"
   (`command=17`) was acknowledged but never stopped anything. **Fixed:** zmng speaks the MSE subset of
   go2rtc's websocket protocol at `/zm/go2rtc/ws` and advertises it; live video in the app is now MSE
   from the recorder's own fMP4 (substream for tiles; main HEVC only if the phone lists `hvc1`), first
   picture in ~0.12 s, no transcoding. Snapshot tiles and MJPEG use the substream; MJPEG pool 3 → 8;
   quit ends the stream; `Capturing/Analysing/AnalysisFPS` reflect the real state.
7. **Stray "null" above the Status heading** (`replaceChildren(null)`). **Fixed.**
8. **Videos started paused after a reload/bookmark.** `muted` set as an attribute by script does not
   mute (only the HTML parser copies it), so autoplay was refused without a click. **Fixed** in `h()`.
9. **After an upgrade browsers kept the old UI for hours** (static files had no `Cache-Control`, so
   heuristic caching applied). **Fixed:** `no-cache` (revalidate, 304 when unchanged). The first load
   after this deploy may still need one hard refresh (Cmd/Ctrl+Shift+R).
10. Timeline ticks read "05:15" (12-hour clock without AM/PM, UTC-aligned for 3 h+ steps). **Fixed:**
    "5:15 PM", local-time aligned, spaced by pixels. Transcode now keeps VFR (`-fps_mode passthrough`,
    TEST-DEPLOY finding 4); `frame.jpg` answers for the open segment (finding 5); Add camera defaults to
    the primary volume (finding 2, UI only — the CLI still defaults to storage 1).

New: a camera-groups manager (Admin → Camera groups: a cameras × groups grid saved per click, create /
rename / remove), previous/next event buttons and keys (N/P) in Review, colour-coded event kinds on
the timeline and event cards.

## Found on production ZoneMinder (not changed — Aaron's call)

- **Motion alarms fell on several monitors after the 2026-09-27 zone rescale** (`phase0-zones.sql`).
  All "All" zones now require 8 % of a 4 MP frame (`MinAlarmPixels` ≈ 326 k of 4.08 M) — a person across
  the playground is well under 1 %. Alarm sections, 3 days before vs the last 72 h: Kitchen Door 54 → 10,
  BackDoor (17) 65 → 12, Courtyard 77 → 15, Upstairs Hall 42 → 13, Playground 10 → 4 (zmng saw 62
  people events there). Weekday mix differs, so the drop is indicative, but for wide outdoor views 8 %
  is too coarse: consider 1–2 % (percent units) on Playground, Kitchen Door, BackDoor, Courtyard,
  Upstairs Hall and Field.
- **Installing any apt package restarts ZoneMinder.** `needrestart` restarts every service still on a
  library replaced by the nightly upgrade. The `python3.12-venv` install for the detector restarted
  apache2, mysql and zoneminder at 18:38:36 (≈22 s gap on all 23 monitors). Runbook §4c now says to use
  `NEEDRESTART_MODE=l` outside the maintenance window.

## Suggested next steps (not done)

- **Detection quality:** run the live pass at 640×360 (`detect_width/height`) on the object cameras
  (sharper frames for the motion-gated pass; watch for extra motion noise), per-camera zones/masks for
  the Playground's trees and road, and a per-camera minimum confidence (the two weakest Front Door
  "person" labels are 0.56–0.58).
- **Notifications on people only:** `require_object` is available per camera; pair it with the ntfy/HA
  webhook so a phone alert means "person at the Front Door", not "shadow moved".
- **GPU detector after cut-over:** once ZoneMinder no longer saturates NVDEC, `onnxruntime-gpu` on the
  P2200 is ~25 ms per frame and makes 23 cameras cheap.
- **Page cache inside the unit's memory cap:** the old process ended at its 4 GB `MemoryMax`, almost all
  page cache from writing ~65 GB/day (RSS ~0.4 GB). Harmless so far (no stalls, 0 reconnects), but
  dropping closed segments from cache (`posix_fadvise(DONTNEED)` after fsync and after tiering copies)
  or a `MemoryHigh` below the max would keep reclaim out of the recorder's write path at 23 cameras.
- **Groups beyond selection:** per-group notification rules and viewer permissions by group (the CBA
  users are scoped by a ZoneMinder group today).
- zmNinjaNg: a native HLS answer for older iPhones without ManagedMediaSource; push delivery (FCM relay).

## Review-page filter ideas (kept out of the way of an average user)

Implemented: **Show** = all selected cameras / cameras with motion / with people / with vehicles / with
animals (object options appear only when a detector is configured), plus previous/next event.
Easy further adds, each one control:
1. **Time-of-day preset** in the Range menu: "last night (10 PM–6 AM)", "this morning", "Sunday
   services" — most church reviews are "what happened overnight / during service".
2. **Minimum event length** (hide blips under ~3 s) — cuts trees and headlight sweeps without touching
   detection settings.
3. **Only archived (starred) events** as a Show option — for incident follow-up.
4. **Speed through quiet time**: play at 1× during events and 8× between them (a toggle next to the speed
   menu) — the single biggest time-saver for reviewing a night.
5. **Saved views**: remember a named combination of group + Show (e.g. "Outside people") as a chip.
