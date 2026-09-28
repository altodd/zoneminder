# ZoneMinder pain points and alternative NVR architectures: online research

Date: 2026-09-27
Scope: public internet research (GitHub, ZoneMinder forums via search excerpts, project docs, vendor docs).
Context: 23 Hikvision cameras, HEVC main streams 2688x1520 @ ~18-20 fps (one 4K), H.264 640x360 substreams,
ZoneMinder 1.38.4 / Ubuntu 24.04 / Quadro P2200 (NVDEC at 100 %) / 2x Xeon Silver 4116 / 125 GB RAM /
~17 TB of events on two 94 %-full volumes / MySQL `Frames` table of 243 M rows (34 GB).

Sourcing note: forums.zoneminder.com sits behind a Cloudflare challenge that blocks automated fetches, so
quotes from forum threads below come from search-engine excerpts of those threads (marked "excerpt") rather
than a full read. GitHub issues, PRs, source files, and project docs were read directly.

---

## 1. ZoneMinder pain points as documented by users and developers

### 1.1 The `Frames` table: why it exists, why it hurts

- **What it is.** `zm_event.cpp` (master) queues frames in `frame_data` and flushes with `WriteDbFrames()`:
  `#define MAX_DB_FRAMES 100`; a flush happens when `frame_data.size() >= MAX_DB_FRAMES`, on a BULK frame,
  or when `frame_data.size() > 5*fps`. The SQL is
  `INSERT INTO Frames (EventId, FrameId, Type, TimeStamp, Delta, Score, AudioLevel) VALUES ...`, optionally
  `INSERT INTO Stats (EventId, FrameId, MonitorId, ZoneId, PixelDiff, AlarmPixels, FilterPixels, BlobPixels ...)`
  per zone when `RECORD_EVENT_STATS` is on, plus periodic
  `UPDATE Events SET Length=..., Frames=%d, AlarmFrames=%d, TotScore=%d, AvgScore=%d, MaxScore=%d, MaxScoreFrameId=%d`
  and a final `EndDateTime=..., Length=..., Frames=..., AlarmFrames=..., DiskSpace=...` update.
  Source: https://raw.githubusercontent.com/ZoneMinder/zoneminder/master/src/zm_event.cpp
- **Why a row per frame.** Forum excerpt, "Fundamentally don't understand Frames database table":
  "Frames records are generated for the purpose of marking the timeline with detections when playing back
  event video, done for every frame so that the timeline can be marked in the specific place within the
  event video timeline." In Record/Mocord mode the `BULK_FRAME_INTERVAL` option groups non-alarm frames into
  one "bulk" row (default one per 100 frames); the docs say this "saves a lot of bandwidth and space" but
  "does not apply in motion detection mode".
  https://forums.zoneminder.com/viewtopic.php?t=33053 ,
  https://zoneminder.readthedocs.io/en/latest/userguide/options/options_config.html
- **Database write amplification.** "Very high disc write from MySQL destroying SSD": user sees ~500 KB/s of
  constant MySQL writes "nearly as much to the database as the video streams themselves"; the query log is
  `INSERT INTO Frames ...` + `UPDATE Events SET Length, Frames, AlarmFrames, TotScore, AvgScore, MaxScore ...` +
  `UPDATE`s to `Events_Hour/Day/Week/Month`. A related excerpt ("Performance tuning time") counts that one
  Frames insert per 100 frames "triggers 6 Update Queries from ZM, which then fire 6 x 13 Update Queries to
  MySQL, resulting in 78 Update Queries", each in its own READ COMMITTED transaction.
  https://forums.zoneminder.com/viewtopic.php?t=32120 , https://forums.zoneminder.com/viewtopic.php?t=26464
- **`Events_Hour/Day/Week/Month`** are summary tables populated by `zmstats.pl` so the console can show
  counts without scanning `Events`; they are themselves updated per-event (see the write-amplification above).
  https://forums.zoneminder.com/viewtopic.php?t=30070 , https://wiki.zoneminder.com/MySQL
- **Sizes seen in the wild:** >30 M Frames rows and multi-GB `.ibd` files are routinely reported; MySQL
  databases "grew > 26 GB"; users delete Frames "a little at a time while zoneminder is shut down" and run
  `OPTIMIZE TABLE`. https://forums.zoneminder.com/viewtopic.php?t=28465 ,
  https://forums.zoneminder.com/viewtopic.php?t=26970 , https://forums.zoneminder.com/viewtopic.php?t=26136
- **`Stats` table.** `RECORD_EVENT_STATS` writes one row per alarm frame per zone; docs: "if you are concerned
  about performance you can switch this off". Users ask for alarm-frames-only to keep the DB lean.
  https://zoneminder.readthedocs.io/en/stable/userguide/options/options_logging.html ,
  https://forums.zoneminder.com/viewtopic.php?p=136485
- **Developer acknowledgement (Sept 2026).** PR #5118 "perf: consolidate the Frames indexes" (schema 1.39.28,
  merged 2026-09-10) replaces `EventId_idx`, `Type`, `TimeStamp` with a single `(EventId, FrameId)` composite
  because "the old indexes cost insert time on the highest-insert-rate table in the schema, plus space, plus
  work on every DELETE ... WHERE EventId"; prev/next-frame lookups previously did "ref EventId_idx, rows 500,
  Using filesort". The FK `Frames_ibfk_1` (added 1.35.11, dropped 1.37.31) also made deletes slow.
  https://github.com/ZoneMinder/zoneminder/pull/5118
- **Takeaway:** even after the 1.39 index work the design still writes O(frames) rows and O(events x 6)
  updates; the fix is structural, not an index.

### 1.2 Event list, thumbnails, event playback

- **Slow first frame of event playback (structural).** Issue #5144 (2026-09-18): ZM writes event MP4s as
  `ftyp, moov, then one moof+mdat pair per keyframe, and an mfra at the end`. Because there is no `sidx`
  before the fragments, FFmpeg's demuxer "must read the entire file forward from byte 0 before presenting the
  first frame": a one-hour event took **326 s on a 10 Mbit/s link vs 0.49 s with an index** (3,595 fragments
  walked). Proposed fix: reserve space after `moov` and back-fill a `sidx` when the event closes.
  https://github.com/ZoneMinder/zoneminder/issues/5144
- Users independently report "up to 40 seconds to load an event" and that `movflags=faststart` or
  `frag_custom+dash+delay_moov` fix it at the price of not being able to view in-progress events.
  https://forums.zoneminder.com/viewtopic.php?t=30983 , https://forums.zoneminder.com/viewtopic.php?p=138132
- **Thumbnails are generated by spawning ffmpeg with a seek** per image, e.g.
  `ffmpeg -ss 87.52 -i .../5643528-video.mp4 -frames:v 1 .../01145-capture.jpg` (excerpt, "Issue with ZM
  1.36.x - Event thumbnails not generating properly"). https://forums.zoneminder.com/viewtopic.php?t=33981
- **Frames view of long events** "can kill either the server or the browser while trying to show thousands
  of frame images" (issue #1988, open, Stale). https://github.com/ZoneMinder/ZoneMinder/issues/1988
- **MJPEG event playback burns CPU:** viewing a recording as MJPEG makes `nph-zms` "consume one Xeon core".
  https://forums.zoneminder.com/viewtopic.php?t=33038
- **Event list pagination** is capped at "up to 200 and then all" rows per page; the console's
  `Events_*` counts and the `Events` list are separate queries. https://forums.zoneminder.com/viewtopic.php?t=31167
- **DiskSpace.** `Events.DiskSpace` is NULL until an "Update Disk Space" filter (`DiskSpace IS NULL`) walks
  the directories; users report "null used by events", wrong totals, and that PurgeWhenFull must be limited
  ("limit results to 10") because a large backlog "can hard spike the CPU usage for a long time".
  https://forums.zoneminder.com/viewtopic.php?t=30009 , https://forums.zoneminder.com/viewtopic.php?t=32637 ,
  https://wiki.zoneminder.com/PurgeWhenFull , https://zoneminder.readthedocs.io/en/stable/userguide/filterevents.html
- **Mocord chunking.** Continuous recording is a chain of fixed-length events (default 600 s), so cross-event
  playback needs UI glue; the developers' `EVENT_CLOSE_MODE` (time/idle/alarm) only tweaks the boundaries.
  https://forums.zoneminder.com/viewtopic.php?t=27786

### 1.3 Timeline and Montage Review

- Issue #4036 "Feature Todo list: Montage Review" (2024-05-23): Montage Review "constantly grab[s] images one
  at a time and writes them to a canvas" and "cannot handle an mjpeg stream"; proposes replacing it with the
  montage layout or merging the timeline into the events review. Open, no PR merged.
  https://github.com/ZoneMinder/zoneminder/issues/4036
- "Upgrading Montage Review page" (2024-06, excerpt): iconnor: "Then we request that frame from zms";
  the design "requests images at specific times one after another, and the latency/turnaround time to spawn
  processes, seek in video storage, find frames, and output them is very slow". A separate `view=timeline`
  implementation exists. https://forums.zoneminder.com/viewtopic.php?t=33288
- "Time line performance" (excerpt): the optimized timeline still has "slow queries of 80-90 ms" and iconnor
  proposed merging Watch/Events/Montage/Montage Review into a single page as "very global and lengthy work".
  https://forums.zoneminder.com/viewtopic.php?t=29925
- Montage Review workarounds: enabling Apache deflate for `application/json` because frame data is sent as a
  huge JSON blob; "code to load frame data in chunks due to it being too large".
  https://forums.zoneminder.com/viewtopic.php?t=33679
- Montage Review delays "on the order of 10 minutes before the next chunk is added" (excerpt).
  https://forums.zoneminder.com/viewtopic.php?t=30795

### 1.4 Decoding, CPU, hwaccel, 4K

- **Architecture (from `zm_monitor.cpp`).** One `zmc` process per monitor with a capture thread, a decoder
  thread and an analysis thread coordinated through a `packetqueue` and an mmap'd shared-memory ring of
  decoded RGB images. `Analyse()` returns early "if (!packet->decoded)", i.e. analysis waits for the decoder.
  Shared memory size = `SharedData + TriggerData + zones + VideoStoreData + image_buffer_count*(timeval +
  image_size + image_size)` where `image_size = av_image_get_buffer_size(AV_PIX_FMT_RGBA, w, h, 32)`; the
  queue is `packetqueue.setMaxVideoPackets(max_image_buffer_count)`.
  https://raw.githubusercontent.com/ZoneMinder/zoneminder/master/src/zm_monitor.cpp
- **Decoding modes** (`None`, `On demand`, `KeyFrames`, `KeyFrames + On demand`, `Always`) were introduced
  in 1.37. Docs: None = "live view and thumbnails will not be available"; KeyFrames = "viewing frame rate will
  be very low depending on the keyframe interval"; and the caveat "GPU support has not yet been fully
  implemented in ZoneMinder."
  https://zoneminder.readthedocs.io/en/latest/userguide/definemonitor/definemonitor_source.html
- **Effect of decode modes.** "Keyframes Only + On Demand" drops CPU "from around 30 % to 1-2 % per camera"
  but makes montage "extremely slow, with images disappearing one by one and the page never finishing its
  load" (1-2 minutes to highlight a console row, 2-3 minutes to open the page) because on-demand decoding
  has to spin up per viewer. https://forums.zoneminder.com/viewtopic.php?t=32598
- **Why 4K needs huge buffers.** For a 4K/20 fps/H.265 camera with 50-frame GOP ZM demands
  `ImageBufferCount 101`, i.e. `3840x2160x3x101 = 2513 MB` of RGB shared memory, because analysis is
  behind capture and "ZoneMinder needs to keep everything since the last keyframe".
  https://forums.zoneminder.com/viewtopic.php?t=30671 ,
  https://wiki.zoneminder.com/Math_for_Memory_-_knowing_how_much_memory_you_need_and_how_to_optimize
- **Queue overflows at exactly our resolution.** Issue #4182: H.264 2688x1520 @ 20 fps, GOP 40:
  "You have set the max video packets in the queue to 421. The queue is full. Either Analysis is not keeping
  up or your camera's keyframe interval 41 is larger than this setting." Issue #3414 (4K, Mocord): zmc RAM
  climbs per event; closed "not planned". https://github.com/ZoneMinder/zoneminder/issues/4182 ,
  https://github.com/ZoneMinder/zoneminder/issues/3414 , https://forums.zoneminder.com/viewtopic.php?t=30846
- **CPU advice from the developers (excerpts).** "You don't have enough cpu to decode the 4k stream";
  "1 thread per motion detection works best ... a 6 core Xeon with 12 threads can handle 6 monitors doing
  motion detection"; "At the end of the day, you are still decoding a high res stream. That's a lot of work."
  https://forums.zoneminder.com/viewtopic.php?t=32300 , https://forums.zoneminder.com/viewtopic.php?t=18837 ,
  https://forums.zoneminder.com/viewtopic.php?t=33327
- **CUDA does not remove the CPU cost.** With `DecoderHWAccelName=cuda` users still see "CPU with
  approximately 30 % load, with only small GPU usage around 5 %" because the decoded NV12 surface is copied
  back and converted to RGB for the shared-memory ring and then analysed on the CPU; the wiki says GPUs are
  "only partially supported ... the CPU gets used anyways".
  https://forums.zoneminder.com/viewtopic.php?t=32597 , https://wiki.zoneminder.com/GPU_passthrough_to_virtual_machines
- **Community "very low CPU" recipe** (t=32093): substream monitor at low res/fps in Modect; main-stream
  monitor in Nodect linked to it, Video Writer = Camera Passthrough, Save JPEGs off, Analysis and Decoding
  off. That is exactly the Frigate/Blue Iris pattern, bolted on with two monitors per camera.
  https://forums.zoneminder.com/viewtopic.php?t=32093 , https://forums.zoneminder.com/viewtopic.php?t=33671
- **1.36.34** set "ffmpeg decoder thread_count to 2", which "approximately halves software 1080p H.264
  decode time"; **1.38.1** added "decoder optimizations" (queue flushing) and fixed "race conditions in
  analysis, decoder, and event threads". https://github.com/ZoneMinder/zoneminder/releases
- **Live view is JPEG.** `zms` reads the RGB frame from shared memory and JPEG-encodes it per viewer; montage
  "can result in CPU load reaching 60 % and higher". Per-viewer JPEG encode of 4 MP frames is the reason the
  UI is forced to the 640x360 substream. https://forums.zoneminder.com/viewtopic.php?t=31002 ,
  https://forums.zoneminder.com/viewtopic.php?t=29512

### 1.5 H.265 live view and the streaming add-ons

- Forum consensus (excerpts): "H.265 live view does not exist in ZoneMinder; live view will be by MJPEG
  unless Janus is set up", and Janus is "only for H.264 at this time". iconnor: he has "a proof of concept
  for decoding H.265 in JavaScript, however no one is funding this work".
  https://forums.zoneminder.com/viewtopic.php?t=32370 , https://forums.zoneminder.com/viewtopic.php?t=31787
- Viewing tab (1.38 docs): RTSP2Web ("experimental", HLS/MSE/WebRTC), Janus (WebRTC, "h.264 video"),
  go2rtc; event view falls back to "MJPEG for h.265 content requiring conversion since browsers cannot
  natively handle it". https://zoneminder.readthedocs.io/en/latest/userguide/definemonitor/definemonitor_viewing.html
- 1.38.0 "Seek and Destroy" (2026-02-01) adds "Janus WebRTC Gateway support", "Go2RTC protocol support",
  "RTSP2Web streaming with WebRTC/MSE/HLS", hardware-accelerated *encoding* (`EncoderHWAccelName`), and
  splits Function into Capturing/Analysing/Recording. Users confirm "Go2RTC is capable of serving up h265
  streams to Firefox directly". https://github.com/ZoneMinder/zoneminder/releases/tag/1.38.0 ,
  https://forums.zoneminder.com/viewtopic.php?t=34262 , https://forums.zoneminder.com/viewtopic.php?t=34044
- These are bolt-ons: the camera stream is pulled a second time by go2rtc/RTSP2Web/Janus (or re-served by
  ZM's built-in RTSP server) and is unrelated to the recorded events, timeline, or Montage Review, which
  remain JPEG/zms based. https://forums.zoneminder.com/viewtopic.php?t=33431 ,
  https://forums.zoneminder.com/viewtopic.php?p=140032

### 1.6 Multi-server, storage, misc

- Multi-server = shared MySQL + one shared events folder over NFS; every monitor is pinned to a server.
  Event counts across servers are historically wrong. https://zoneminder.readthedocs.io/en/latest/installationguide/multiserver.html ,
  https://forums.zoneminder.com/viewtopic.php?t=27631 , https://github.com/ZoneMinder/zoneminder/issues/2762
- "Isn't it about time?" (2025): users note "Frigate etc assume the presence of a decent GPU"; iconnor on
  why a rewrite has not shipped: "Because we know it has issues that we would like to resolve before release.
  Also, I am overworked and under paid." https://forums.zoneminder.com/viewtopic.php?p=137600
- Release cadence: 1.36.33 (Feb 2023) -> 1.36.35 (Oct 2024) -> 1.38.0 (Feb 2026) -> 1.38.4 (Aug 2026);
  1.39.x is the development branch. https://github.com/ZoneMinder/zoneminder/releases

### 1.7 What ZM 1.37/1.38/1.39 actually changed (summary)

| Area | Change | Still true after change |
|---|---|---|
| Decoder | 1.37: separate decoder thread, Decoding modes, `SourceSecondPath` substream analysis (experimental) | analysis waits on decode; RGB ring in shm; per-viewer JPEG |
| Frames | 100-row batched inserts (`MAX_DB_FRAMES`); 1.37.31 dropped FK; 1.39.28 composite index | one row per frame in Modect; 6 Events updates per flush |
| Streaming | 1.37 Janus; 1.38 go2rtc + RTSP2Web (MSE/HLS/WebRTC), HW encode | not used for events/timeline/montage review |
| Storage | fragmented MP4 passthrough (PR #1882 era), `EVENT_CLOSE_MODE` | no `sidx` (issue #5144), 10-min event chunks |
| UI | 1.38 grid layouts, Server_Stats | Montage Review canvas/JPEG design (issue #4036) |

---

## 2. How the alternatives architect the same problems

### 2.1 Frigate (Python + go2rtc + ffmpeg + SQLite + nginx-vod-module)

- **Roles.** Each camera has `detect`, `record`, `audio` roles; docs recommend "a lower resolution stream for
  object detection, but create recordings from a higher resolution stream" and "define a `detect` role for a
  low resolution stream to minimize resource usage from the required stream decoding".
  https://docs.frigate.video/configuration/cameras/
- **Recording.** Segments "are written in 10 second chunks", "copied directly from the camera stream without
  re-encoding", cached in `/tmp/cache` as `{camera}@{timestamp}.mp4`, validated, then moved with the
  `faststart` flag to `/media/frigate/recordings/YYYY-MM-DD/HH/<camera>/MM.SS.mp4` (UTC). Retention modes
  `all` / `motion` / `active_objects`; deletion is by free space ("when approximately one hour of recording
  space remains, old recordings delete automatically").
  https://docs.frigate.video/configuration/record/ , https://deepwiki.com/blakeblackshear/frigate/7.1-recording-system
- **Index.** SQLite `/config/frigate.db`; `Recordings(id, camera, path, start_time, end_time, duration,
  motion, objects, regions, dBFS, segment_size)`, plus `ReviewSegment`, `Previews`, `Timeline`. One row per
  10 s segment, never per frame. https://deepwiki.com/blakeblackshear/frigate/7-storage-and-recording
- **Playback.** Recorded video is served as HLS by nginx-vod-module: `/vod/...` "fetches a small JSON mapping
  from the Frigate API and generates segments straight from the recording files"; fMP4 is used "for non-AVC
  video". Maintainer: "Previews are saved as low fps, low resolution files, and your high res recordings are
  copied directly with the original codec, bitrate, and frame rate". Known limit: hours with >1000 segments
  can break VOD. https://github.com/blakeblackshear/frigate/discussions/18948 ,
  https://github.com/blakeblackshear/frigate/discussions/19467
- **Review/timeline.** Review items group overlapping detections into alerts/detections; the Review page is
  a vertical timeline plus a grid of previews. Previews are "bandwidth-optimized, low frame rate, low
  resolution videos" (180 px high, frames only saved when there is motion, generated hourly by an
  `FFMpegConverter` thread with the concat demuxer) so "an entire hour" is "smaller than a single snapshot";
  when scrubbing stops "Frigate loads the higher frame rate, full resolution recording".
  https://docs.frigate.video/configuration/review/ , https://github.com/blakeblackshear/frigate/discussions/13345
- **Live view.** jsmpeg (<=10 fps, 720p, no go2rtc), MSE and WebRTC via bundled go2rtc at native resolution;
  "smart streaming where camera images update once per minute when no detectable activity is occurring";
  docs warn "some browsers may not support H.265" and recommend H.264 as "the most compatible video codec".
  https://docs.frigate.video/configuration/live/
- **GPU use.** Decode via `preset-nvidia`, `preset-vaapi`, `preset-intel-qsv-*`, `preset-rkmpp`, Jetson;
  detection via Coral, Hailo, OpenVINO (CPU/iGPU/NPU), ONNX ("Nvidia GPUs will automatically be detected and
  used with the ONNX detector in the -tensorrt Frigate image"; the old TensorRT detector "has been removed"),
  ROCm, Apple NPU. Recommended model YOLOv9 at 320x320; Frigate "crop[s] a region of motion from the full
  frame and zoom[s] into it before running detection". Published inference: Coral ~10 ms, RTX 3070 6-8 ms
  (YOLOv9-t), RTX 5060 Ti 5-7 ms, Intel Arc A750 ~4 ms, Intel NPU ~6 ms, Hailo-8 ~7 ms. Coral "no longer
  recommended for new installs". https://docs.frigate.video/configuration/hardware_acceleration_video/ ,
  https://docs.frigate.video/configuration/object_detectors/ , https://docs.frigate.video/frigate/hardware/ ,
  https://github.com/blakeblackshear/frigate/discussions/19730
- **Known gap for our use-case:** "there's not a sensible way to have low resolution 24/7 recording with
  only events saved in high resolution" (users add the camera twice).
  https://github.com/blakeblackshear/frigate/discussions/16082

### 2.2 moonfire-nvr (Rust, single process)

- "Saves H.264-over-RTSP streams from IP cameras to disk into a hybrid format: video frames in a directory on
  spinning disk, other data in a SQLite3 database on flash"; "handles six 1080p/30fps streams on a Raspberry
  Pi 2, using less than 10 % of the machine's total CPU". H.264 only; no motion detection yet.
  https://github.com/scottlamb/moonfire-nvr/blob/master/README.md
- **Storage.** 1-minute "recordings"; each sample file is "a concatenation of the compressed, timestamped
  video samples ... as received from the camera" (just the `mdat` payload). Rotation times are staggered
  across cameras to avoid seek storms. Rationale for 1 minute: ~4 KB metadata per recording at 30 fps,
  <1 % disk time for 16 streams, small internal fragmentation, reasonable retention granularity.
  https://github.com/scottlamb/moonfire-nvr/blob/master/design/schema.md
- **Index.** SQLite `recording(start_time_90k, duration_90k, sample_file_uuid, sample_file_blake3,
  sample_file_size, video_samples, video_sample_entry_id, video_index BLOB)`; `video_index` stores per frame
  two zig-zag varints (duration delta with key-frame bit; size delta vs previous frame of same type) -
  "approximately 2 bytes per frame". "Putting the metadata on flash means metadata operations can be fast
  (sub-millisecond random access, with parallelism) and do not take precious disk time fraction away from
  accessing sample data." Same doc.
- **Playback.** Builds unfragmented MP4 (for `<video>`/download) or fragmented MP4 (for MSE/DASH) for
  arbitrary time ranges on the fly from the index, served with HTTP range requests; the moov `stbl` is
  synthesised from `video_index`. No per-frame DB rows, no thumbnails stored. Same doc.
- Verification of a 5.9 TB / 500,000-file dataset: 19 s presence check, 120 s size check.

### 2.3 UniFi Protect (why it feels fast on a tiny box)

- Hardware: UNVR = quad-core ARM Cortex-A57 1.7 GHz, 4 GB; UNVR Instant = Cortex-A55, 4 GB, "15 HD or 6 4K
  cameras"; UNVR Pro 8 GB. No discrete GPU. https://techspecs.ui.com/unifi/cameras-nvrs ,
  https://lazyadmin.nl/home-network/unifi-protect-review/
- **Live video is the camera's own bitstream** delivered as fMP4 over a WebSocket
  (`/proxy/protect/api/ws/livestream`) and played with MSE; "true access to the camera's encoded video
  datastream (H.264, HEVC, or AV1), not the RTSP URLs"; identical subscriptions "share one underlying
  WebSocket, and a late joiner is replayed the cached init segment"; latency "1-3 seconds".
  https://github.com/hjdhjd/unifi-protect , https://github.com/awestley/MMM-UniFiProtect
- **Codec choice is a browser decision.** "Standard" = H.264 ("plays in all major browsers"), "Enhanced" =
  H.265 ("30-50 % storage savings", "requires specific client support"; "Auto" can throw "codec not supported").
  Ubiquiti moves the decode burden to the client and offers H.264 to keep universal playback; there is no
  server transcode. https://doitforme.solutions/blog/unifi-protect-codec-encoding-standard-vs-enhanced-h-264-vs-h-265/
- **Timeline** is built from motion/smart-detection metadata pushed over a realtime WebSocket ("instantaneous
  updates to Protect-related events"); scrubbing uses low-rate thumbnails and the recorded segments.
  https://github.com/hjdhjd/unifi-protect
- Net: passthrough recording + browser-native decode + metadata-only timeline lets a 4-core ARM box serve
  dozens of cameras; the price is H.264 (bigger files) or HEVC (client support).

### 2.4 Scrypted NVR (TypeScript/Node plugin)

- "24/7 recording and smart detections" with desktop/mobile apps; officially supports "local RTSP Cameras"
  with "H.264 video codec" and ONVIF; "Scrypted waits for the camera to report motion to trigger video
  analysis" (camera motion sensor drives detection). Records main and sub streams and spreads them across
  disks (1 TB minimum each); deletes at 15 % free, stops at 10 %.
  https://docs.scrypted.app/scrypted-nvr/ , https://docs.scrypted.app/scrypted-nvr/camera-recording.html ,
  https://docs.scrypted.app/scrypted-nvr/recording-storage.html
- Hardware guidance: Intel N100 = 12 cameras ("fantastic iGPU for accelerated transcode and detection"),
  Mac mini 16 GB / Core Ultra 125H = 20+ cameras using Apple Neural Engine / Intel NPU.
  https://docs.scrypted.app/server-hardware.html
- Live view via WebRTC (Scrypted's WebRTC plugin) with H.264; H.265 is not the supported path.

### 2.5 Viseron (Python, Docker)

- v3: "24/7 recordings" with a storage component that "retain[s] data based on time and consumed space" and
  tiers to move recordings by age/size; "FFmpeg writes small 5 second segments of the stream to disk" (enables
  lookback and continuous recording); PostgreSQL backend; timeline view shows recordings plus motion/object/
  face events; go2rtc for live (RTSP + WebRTC); detectors YOLOv7/v4/v3, EdgeTPU, etc.
  https://github.com/roflcoopter/viseron/discussions/721 , https://viseron.netlify.app/docs/documentation/configuration/recordings ,
  https://community.home-assistant.io/t/viseron-v3-6-0-self-hosted-local-only-nvr-and-ai-computer-vision-software/223152

### 2.6 Shinobi (Node.js)

- Node.js 16 + FFmpeg + MariaDB (or SQLite); "Direct saving to MP4"/WebM via ffmpeg (copy or encode);
  live view over WebSocket (Base64/MJPEG), HLS, and "Streams by WebSocket"; Power Viewer timeline draws
  motion-level bars; detector plugins TensorFlow/Coral, YOLO, Deepstack.
  https://docs.shinobi.video/installation , https://wiki.archlinux.org/title/Shinobi

### 2.7 Blue Iris (Windows, C++)

- **Direct-to-disc** "images from the camera are sent directly to the storage drive without first re-encoding
  ... spares the CPU from having to redraw every frame"; the `.bvr` container carries "metadata for video
  overlays ... motion detection rectangles, as well as camera state flags"; "for timeline playback to work
  properly, BVR must be used". https://www.houselogix.com/docs/blue-iris/BlueIris/recording.htm ,
  https://ipcamtalk.com/threads/5-2-8-may-20-2020-direct-to-disc-bvr-recording-will-now-include-metadata-for-video-overlays.48168/
- **Sub streams**: motion detection and grid view use the sub stream; "Direct-to-disc must be enabled in
  order for sub streams to work at all"; "Recorded sub streams are used during multiple-camera timeline
  playback in order to keep CPU usage from ballooning"; solo/maximize switches to the main stream. Reported
  effect: "reduce CPU usage by 5x to 20x" (95-100 % -> ~18 %).
  https://ipcamtalk.com/wiki/sub-stream-guide/ , https://ipcamtalk.com/threads/using-sub-streams-dropped-cpu-utilization-down-dramatically.50896/

### 2.8 Nx Witness / DW Spectrum (C++ media server)

- Archive layout `<storage>/HD Witness Media/<server GUID>/<hi|low_quality>/<camera ID>/<yyyy>/<mm>/<dd>/<hh>/
  <epochtime>_<duration>.mkv` - both streams recorded, hourly folders, files named by start time and
  duration; "gap-free timelines"; writes spread across all drives at equal fill rates; archive can be
  re-indexed from the files. Metadata lives in SQLite: `ecs.sqlite` (site), `mserver.sqlite` (local events/
  bookmarks), and an Object Detection Database (recommended on SSD).
  https://support.networkoptix.com/hc/en-us/articles/205501378-How-does-Nx-Witness-manage-storage ,
  https://support.networkoptix.com/hc/en-us/articles/115013837268-Nx-Mediaserver-Database-Files ,
  https://support.networkoptix.com/hc/en-us/articles/217862448-Replacing-a-Camera-and-Recovering-Archive-Footage-in-Nx-Witness
- Timeline is driven by motion metadata (the "Metadata Driven Backup" feature in v5 selects by motion/
  bookmark/object metadata). https://www.networkoptix.com/blog/metadata-driven-archive-backup-in-v5
- The desktop client decodes natively (H.264/H.265/AV1 with GPU); the web client uses HLS/WebRTC with
  server-side transcoding to H.264 when needed.

### 2.9 Agent DVR (iSpy, .NET)

- Timeline: "Recordings are color-coded based on peak motion levels ... Agent DVR will display thumbnails for
  recordings when space allows"; click a bar to seek; motion search needs a tracking detector or AI with
  area data. Detection via built-in models or CodeProject.AI/DeepStack. Recording via ffmpeg to MP4.
  https://www.ispyconnect.com/docs/agent/timeline , https://www.ispyconnect.com/features

### 2.10 Kerberos.io Agent (Go)

- Go backend (`gortsplib` ingest) + React UI; "Ability to create fragmented recordings, and streaming through
  HLS fMP4"; one fragmented MP4 per recording (free-box placeholder up front, back-filled on close), keyframe
  aligned, GOP-aware pre-recording; multi-stream "simultaneous H265 recording + H264 livestream"; WebRTC live
  is "H264/PCM only"; H265 "lacks WebRTC compatibility". Detection is offloaded to Kerberos Hub/Vault.
  https://github.com/kerberos-io/agent , https://deepwiki.com/kerberos-io/agent/2.2-video-capture-and-recording-pipeline

### 2.11 go2rtc (Go)

- Zero-dependency Go binary; outputs RTSP, WebRTC, MSE (fMP4 over WebSocket), HLS, MP4, MJPEG; codec filter
  syntax `?video=h264,h265&audio=aac`; optional ffmpeg transcoding with hardware acceleration
  (`#video=h264#hardware`). Support matrix (README): Chrome 136+/Android Chrome 136+ "H264, H265* ... via
  WebRTC" and "H264, H265* ... via MSE"; Safari desktop/iOS 17.1+ H264/H265 via WebRTC, HLS and MSE;
  Firefox "H264 PCMU, PCMA OPUS" only; iPhone Safari 14+ HLS only. "HLS is the worst technology for live
  streaming." https://github.com/AlexxIT/go2rtc , https://raw.githubusercontent.com/AlexxIT/go2rtc/master/README.md
- go2rtc uses "a custom Apple-specific payload format" rather than RFC 7798 to get H.265 WebRTC into Safari;
  PR #1644 added `isH265Supported` so the web player only prefers `hvc1.` in MSE when the browser can decode it.
  https://github.com/AlexxIT/Blog/issues/5 , https://github.com/AlexxIT/go2rtc/pull/1644

### 2.12 Comparison table

| System | Language | On-disk format | Index | Full-res live in browser | Timeline speed trick | GPU use |
|---|---|---|---|---|---|---|
| ZoneMinder | C++/Perl/PHP | fMP4 per event (no sidx), JPEGs optional | MySQL: row per frame (+Stats) | JPEG via zms; go2rtc/Janus/RTSP2Web bolt-on | none (canvas of JPEGs) | decode only, CPU still busy |
| Frigate | Python/Go/C | 10 s MP4 segments, copy | SQLite row per segment | go2rtc MSE/WebRTC | hourly 180 px motion-only previews | decode (ffmpeg), detect (ONNX/OpenVINO/TensorRT) |
| moonfire | Rust | 1-min raw sample files | SQLite + 2 B/frame varint index | fMP4 to MSE | on-the-fly MP4 from index | none |
| UniFi Protect | C++ (closed) | camera bitstream | proprietary | fMP4/WebSocket -> MSE, H.264 or HEVC | motion metadata + thumbnails | none (cameras do analytics) |
| Blue Iris | C++ | BVR direct-to-disc (main+sub) | proprietary | main on solo, sub in grid | sub stream timeline | Intel/NVIDIA decode |
| Nx Witness | C++ | hourly MKV per stream, hi+low | SQLite | HLS/WebRTC (web), native (client) | motion metadata | client GPU |
| Kerberos | Go | fMP4 per recording | files + Hub | HLS fMP4 / WebRTC H.264 | - | optional |

---

## 3. Browser video decoding facts (as of 2026)

### 3.1 HEVC by API and browser

| Browser | MSE (`hvc1`) | WebRTC | WebCodecs | HLS | Notes |
|---|---|---|---|---|---|
| Safari macOS/iOS | yes (macOS 11+/iOS 17.1+ MSE; hvc1 tag required) | yes (Safari 18+; go2rtc uses Apple payload) | yes (16.4+) | yes | universal hardware decode on Apple silicon / 2017+ Intel |
| Chrome Windows | yes since 107 (Oct 2022) if GPU has HEVC decoder (D3D11VA) | 136+ (Mar 2025) receive | 107+ (8-bit), 108+ (10-bit) | via MSE | no software HEVC decoder; ~81 % of Windows Chrome sessions can decode |
| Chrome macOS | yes 107+ (VideoToolbox) | 136+ | 107+ | via MSE | ~99 % |
| Chrome Android | MSE 112+ | 136+ | 112+ | yes | device dependent (~86 %) |
| Chrome Linux | 108+ VAAPI only (Intel GPUs) | 136+ w/ hw | no | via MSE | fragile |
| Edge Windows | as Chrome, but only 56 % of sessions decode | no send; receive as Chromium | as Chrome | | |
| Firefox | Win 134+, macOS 136+, Linux/Android 137+ (hardware only) | no | no ("does not support decoding H.265 streams using WebCodecs API yet") | | |

Sources: https://github.com/StaZhu/enable-chromium-hevc-hardware-decoding ,
https://chromestatus.com/feature/5153479456456704 , https://groups.google.com/a/chromium.org/g/blink-dev/c/3h8lL8a377c ,
https://webcodecsfundamentals.org/datasets/codec-analysis-2026/ , https://webcodecsfundamentals.org/codecs/hevc.html ,
https://bugzilla.mozilla.org/show_bug.cgi?id=1852263 , https://support.cs.inc/s/article/Enabling-HEVC-H-265-Video-Playback-in-Browsers ,
https://learn.microsoft.com/en-us/answers/questions/5875799/edge-webrtc-h265-support , https://caniuse.com/webcodecs ,
https://www.ffmpeg-micro.com/blog/your-hevc-mp4-won-t-play-on-apple-the-ffmpeg-hvc1-tag-fix

- 2026 field data (1.1 M sessions): HEVC decode is "universal on Safari and nearly absent on Edge and
  Firefox", 81 % on Chrome/Windows; "AV1 + HEVC covers 99.73 % of sessions for decode" while AV1 alone is
  ~91.5 % (Safari macOS only ~24 %). Practical rule: HEVC works on Apple devices and Chrome on hardware with an
  HEVC decoder; H.264 is the only codec that works everywhere. https://webcodecsfundamentals.org/datasets/codec-analysis-2026/
- WebCodecs (Chrome 94+, Edge 94+, Firefox 130+, Safari 16.4+/26) lets a page decode `EncodedVideoChunk`s
  and paint `VideoFrame`s to canvas with lower latency than MSE and without MSE's "stop on underrun"
  behaviour; HEVC via WebCodecs is hardware-only and absent in Firefox. WASM HEVC decoders exist as a
  fallback for low resolutions. https://github.com/w3c/webcodecs/blob/main/explainer.md ,
  https://dev.to/mrujjwalg/play-hevc-av1-and-mkv-in-the-browser-with-webcodecs-no-server-transcoding-57 ,
  https://dev.to/privaloops/playing-hevc-in-a-browser-without-plugin-an-h265-decoder-in-webassembly-4ag0
- **go2rtc's position** (README): MSE H.265 on "modern Chrome and Safari", WebRTC H.265 on Chrome 136+ and
  Safari 18+, Firefox H.264 only; falls back per codec filter and can transcode with ffmpeg on demand.
  https://github.com/AlexxIT/go2rtc

### 3.2 Hikvision: H.264 at 4 MP / 8 MP and the bitrate penalty

- Hikvision 8 MP models (e.g. DS-2CD2087G2-L(U)) list main stream "H.265/H.264/H.264+/H.265+" at
  3840x2160 @ 20 fps (60 Hz), bit rate 32 Kbps-16 Mbps; sub stream H.265/H.264/MJPEG; third stream
  H.265/H.264. So H.264 main streams at 4 MP and 8 MP are available on our cameras.
  https://www.hikvision.com/en/products/IP-Products/Network-Cameras/Pro-Series-EasyIP-/ds-2cd2087g2-l-u-/ ,
  https://assets.hikvision.com/prd/public/all/doc/sm000058396/DS-2CD2087G2-LUK-D_Datasheet_V5.7.2_20220906.pdf
- Typical bitrates at 15 fps (industry table): H.264 3 MP 4.4 / 5 MP 7 / 8 MP 11.5 Mbps; H.265 3 MP 2.5 /
  5 MP 4.2 / 8 MP 6.7 Mbps; H.265+ roughly a further 50-67 % below H.265 (Hikvision claims H.265+ "operates
  at 32 % of the HEVC's bitrate" and "83.7 % over H.264"). Hikvision's calculator suggests 2048 kbps max for a
  4 MP ColorVu at 25 fps with H.265+. https://mammothsecurity.com/blog/what-is-the-storage-difference-between-h264-h265-and-h265 ,
  https://www.hikvision.com/en/core-technologies/storage-and-bandwidth/h-265-plus/ ,
  https://ipcamtalk.com/threads/hikvision-bitrate-settings-4k-8mp-h-265-im-confused.31471/ ,
  https://www.use-ip.co.uk/forum/threads/how-much-bandwidth-and-storage-does-hikvisions-h-265-save.1941/
- Caveats: H.265+/H.264+ are proprietary long-GOP VBR modes that "massively reduce" static-scene bitrate but
  produce very long keyframe intervals (bad for seek granularity and packet-queue sizing) and some players
  choke on them; CBR/VBR at a fixed GOP is safer for an NVR. UniFi's own numbers for HEVC vs H.264 are
  "30-50 %" savings. https://ipcamtalk.com/threads/cbr-vs-vbr-recorded-video-quality.33123/ ,
  https://doitforme.solutions/blog/unifi-protect-codec-encoding-standard-vs-enhanced-h-264-vs-h-265/
- Back-of-envelope for our site: 22x4 MP + 1x8 MP at ~18 fps, H.265 ~3.5 Mbps / H.264 ~6.5 Mbps average ->
  ~0.85 TB/day (HEVC) vs ~1.6 TB/day (H.264); 17 TB is ~20 days vs ~11 days of continuous retention.

---

## 4. GPU capabilities: Quadro P2200 today, cheap upgrades

### 4.1 Quadro P2200 (Pascal GP106, 5 GB GDDR5X, 75 W)

- "One dedicated H.264 and HEVC encode engine and a dedicated decode engine" -> **1 NVENC + 1 NVDEC**;
  decode H.264 8/10-bit, HEVC 4:2:0 8-bit (no HEVC 4:4:4, no VP9 10-bit, no AV1); HEVC decode up to 4K60
  10/12-bit per datasheet. https://developer.nvidia.com/video-encode-and-decode-gpu-support-matrix-new ,
  https://www.nvidia.com/content/dam/en-zz/Solutions/design-visualization/quadro-product-literature/quadro-p2200-datasheet-letter-974207-r4-web.pdf ,
  https://www.leadtek.com/eng/products/workstation_graphics(2)/nvidia_quadro_p2200(20836)/detail
- **NVDEC throughput per engine (NVIDIA Video Codec SDK 13 app note, 1080p fps):** Pascal H.264 694 /
  HEVC 810 / HEVC-10 789; Turing 771 / 1316 / 1158; Ampere 748 / 1415 / 1299; Ada 903 / 1641 / 1520;
  Blackwell 2172 / 1872 / 1818. "If a Quadro or Tesla GPU has 2 NVDECs, multiply ... by the number of NVDECs."
  https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvdec-application-note/index.html
- **Our load in 1080p-equivalents:** 22 x 2688x1520 (1.97x 1080p) x 19 fps ~ 823 fps + 1 x 4K (4x) x 20 fps
  = 80 fps -> ~900 1080p-fps of HEVC against a single Pascal NVDEC rated ~810. That is exactly why NVDEC is
  pinned at 100 % and frames spill to the CPU. Decoding only the 23 x 640x360 substreams is ~25 1080p-fps
  (3 % of one NVDEC, or ~1-2 % of a CPU core each in software).
- **NVENC:** Pascal 1080p H.264 ~667 fps, HEVC ~539 fps (P1); "on non-qualified GPUs, the number of
  concurrent encode sessions is limited to 8 per system" (raised from 3/5 in 2024); Quadro is *qualified*
  -> sessions limited only by engine throughput. Thus the P2200 can serve ~8-10 concurrent 4 MP@15 fps
  transcodes-to-H.264 for browsers that cannot decode HEVC, but not 23 continuous ones.
  https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvenc-application-note/index.html ,
  https://videocardz.com/newz/nvdia-geforce-gpus-now-support-up-to-8-concurrent-nvenc-encoding-sessions
- **AI inference:** TensorRT now requires "compute capability SM 7.5 or higher" - **Pascal (SM 6.1) is not
  supported** by current TensorRT; CUDA 13.0 "removed" Maxwell/Pascal/Volta offline compilation. ONNX Runtime
  with the CUDA EP on CUDA 12.x still runs on Pascal, and Frigate users report a GTX 1050 Ti doing YOLO-NAS-S
  at ~23 ms, a Quadro P600 yolov4-tiny-416 at ~10 ms. So the P2200 can run a small YOLO (roughly 30-60 fps of
  320x320 inferences, enough for 23 cameras at 1-2 detections/s each) but only on legacy CUDA 12 stacks with
  no forward path. https://docs.nvidia.com/deeplearning/tensorrt/latest/getting-started/support-matrix.html ,
  https://github.com/NVIDIA/TensorRT/issues/3826 , https://github.com/blakeblackshear/frigate/discussions/19730 ,
  https://michalasobczak.pl/ai-ml/2025/02/google-coral-tpu-and-tensorrt/

### 4.2 Cheap upgrade options

| Card | Decode engines / codecs | Detection | Notes / source |
|---|---|---|---|
| Intel Arc A310 (~$100, 50-75 W) | 2 media engines; AVC/HEVC/VP9/AV1 8K60 12-bit decode; AV1 encode | OpenVINO GPU (~4 ms MobileNet on A750 class) | Frigate report: "4 UHD HEVC streams are barely noticeable (~1 %)", ~0.5 GB VRAM per UHD stream -> VRAM (4 GB) is the limit. https://github.com/blakeblackshear/frigate/discussions/17727 , https://www.intel.com/content/www/us/en/support/articles/000093450/graphics/intel-arc-dedicated-graphics-family.html |
| Intel Arc Pro B50 ($350, 70 W, 16 GB) | dual media engines "each with encoder and decoder for up to two 8K 10-bit workloads"; HEVC 8/10/12-bit, AV1 | OpenVINO; needs OpenVINO 2025.4+/new NEO drivers; Frigate 0.17 works via VAAPI | https://www.pugetsystems.com/labs/articles/intel-arc-pro-b50-review/ , https://github.com/blakeblackshear/frigate/discussions/21257 |
| NVIDIA RTX A2000 (Ampere, 70 W) | 1 NVDEC (Ampere ~1415 HEVC 1080p-fps), no AV1 decode; Quadro = unlimited NVENC | TensorRT SM 8.6, ONNX CUDA | ~1.75x the P2200 decode; supported by TensorRT 10. https://developer.nvidia.com/video-encode-and-decode-gpu-support-matrix-new |
| NVIDIA RTX 4060 / 5060 (Ada / Blackwell) | 1 NVDEC each (Ada ~1641, Blackwell ~1872 HEVC fps; 5060 "double the H.264 decode", 4:2:2, MV-HEVC); AV1 decode; GeForce = 8 NVENC sessions | TensorRT SM 8.9 / 12.0; Frigate "RTX 5060 Ti 5-7 ms YOLOv9" | dual NVDEC only on 4070 Ti+ / 5070 Ti+ class. https://www.techpowerup.com/review/zotac-geforce-rtx-5060-solo-8-gb/2.html , https://docs.frigate.video/frigate/hardware/ |
| Coral TPU (USB/M.2) | none (decode not helped) | ~8-10 ms SSD-MobileDet, single model instance | Frigate: "no longer recommended for new installs"; superseded by Hailo-8/8L (~7 ms YOLOv6n) and Intel NPU (~6 ms). https://docs.frigate.video/frigate/hardware/ , https://hometechops.com/cameras/coral-vs-hailo-vs-intel-npu |

- Rule of thumb from the numbers above: decoding **all 23 HEVC main streams** continuously needs ~900
  1080p-fps of NVDEC = an Ada/Blackwell-class single engine or two Arc media engines; decoding **only
  substreams** needs almost nothing; and no cheap GPU is needed for detection if it runs at 320x320 on
  motion-cropped substream frames (Frigate's design).

---

## 5. Rewrites, ports and storage proposals around ZoneMinder

- **zm-api (Rust, Axum + SeaORM + Tokio)** by Steve Gilvarry: replaces the Perl/PHP API "as a single,
  well-tested Rust service - with live streaming (HLS fMP4 and WebRTC), fine-grained access control, and
  OpenAPI docs"; optional "takeover mode" re-implements `zmdc.pl`, `zmwatch.pl`, `zmstats.pl`, `zmaudit.pl`,
  `zmtelemetry.pl`, "each independently switchable with all defaulting off"; explicitly "does not touch
  capture or recording daemons (zmc/zma)". ~62 % coverage, 1,100+ tests, Ubuntu 24.04/Fedora 41 packages.
  https://github.com/SteveGilvarry/zm-api
- **zmNinjaNg** (React/TypeScript/Capacitor/Electron, ~3,000 commits) is a client rewrite; the author
  "don't[s] plan to support zmNinjaNg with any urgency" and "ZoneMinder does plan to offer limited support".
  https://github.com/pliablepixels/zmNinjaNG , https://forums.zoneminder.com/viewtopic.php?t=34288
- **zmeventnotification / mlapi / pyzm**: `zmeventnotification` was "archived by the owner on May 19, 2026";
  "The built-in server mode of pyzm replaces mlapi"; next-gen zmES 7 / pyZM 2 were opened for testing Feb 2026
  under the ZoneMinder org; pliablepixels "has run out of time to actively maintain these projects".
  https://github.com/pliablepixels/zmeventnotification , https://github.com/pliablepixels/mlapi ,
  https://forums.zoneminder.com/viewtopic.php?t=34288
- **fMP4 / continuous storage in ZM itself:** MP4 event storage was a bounty in 2013 (#39 "Store events in
  mp4 container [$1,280]", #452), landed as "Feature h264 videostorage" (PR #1882, passthrough into MP4
  per event), later switched to fragmented MP4 so in-progress events are viewable; the missing `sidx`
  (issue #5144) and 10-minute Mocord chunking are the residue. No proposal for a continuous, event-independent
  segment store was found in the ZM tracker. https://github.com/ZoneMinder/zoneminder/issues/39 ,
  https://github.com/ZoneMinder/zoneminder/issues/452 , https://github.com/ZoneMinder/zoneminder/pull/1882/files ,
  https://github.com/ZoneMinder/zoneminder/issues/5144
- **Frigate-with-ZoneMinder hybrid** (Frigate discussion #16082): users keep ZM for low-res 24/7 and Frigate
  for high-res events; the maintainers say Frigate has no first-class "low-res continuous + high-res events"
  mode. https://github.com/blakeblackshear/frigate/discussions/16082
- No project named "zm-ng"/"zoneminder-ng" or a Go/Rust port of `zmc` was found; the closest full-stack
  Rust NVR is moonfire-nvr (recorder only) and the closest Go one is Kerberos Agent (recorder + motion).

---

## Implications for our redesign

1. **Never write per-frame rows to a relational database.** ZM's `Frames` (243 M rows here) exists only to
   paint alarm markers on a timeline; moonfire stores the same information at ~2 bytes/frame in a per-minute
   blob and Frigate stores one row per 10 s segment. A per-segment row plus a compact keyframe/score index
   blob removes the 78-updates-per-insert amplification, the SSD-killing write load, the slow deletes, and
   the 34 GB table. (Sources: 1.1, 2.1, 2.2)
2. **Record passthrough, continuously, into fixed-length fMP4/CMAF segments with a keyframe index; make
   "events" metadata, not files.** Every fast system (Frigate 10 s, moonfire 60 s, Nx hourly MKV, UniFi,
   Kerberos, Blue Iris BVR) stores the camera bitstream unmodified and treats motion/objects as a time-range
   overlay. This kills Mocord's 10-minute event boundaries, `DiskSpace IS NULL` walks, PurgeWhenFull CPU
   spikes (retention becomes "delete oldest segments until free >= X"), and the no-`sidx` first-frame stall.
   Segment length 10-60 s; write a `sidx`/side index at close; stagger rotation across cameras (moonfire).
3. **Retention by free space, oldest-first, per volume, in the recorder** (Frigate: delete when ~1 h of
   space remains; Scrypted: start at 15 % free), not by a Perl filter that scans `Events`.
4. **Decode only the substream for motion, and only at ~5 fps.** 23 x 640x360 costs ~1-2 % of a core each
   in software (or 3 % of one NVDEC); ZM's design forces full-res decode into an RGB ring (2.5 GB for one 4K
   camera) and CUDA "still uses the CPU". Blue Iris and Frigate both report 5-20x CPU reduction from this.
5. **Never decode the main stream on the server except on demand** (thumbnail, export transcode, a browser
   that cannot decode HEVC). A single Pascal NVDEC (~810 HEVC 1080p-fps) cannot even keep up with our 23
   main streams (~900 1080p-fps), which is why it is pinned at 100 % today.
6. **Use the GPU/NPU for object detection on motion-cropped substream frames, not for decoding.** Frigate's
   320x320 YOLOv9 on a motion crop needs 5-10 ms per inference on any modern accelerator; 23 cameras at 1-2
   inferences/s is trivial. Note the P2200 is off the TensorRT support list (SM 7.5+ required) and CUDA 13
   dropped Pascal, so plan on ONNX Runtime/CUDA 12 as a stop-gap and an Arc/Hailo/RTX-class device later.
7. **Deliver live full-resolution video as the camera's bitstream over fMP4/WebSocket (MSE) or WebRTC,
   never as server-side JPEG.** UniFi Protect serves dozens of cameras from a 4-core ARM box this way;
   go2rtc gives us WebRTC + MSE + HLS with codec negotiation for free (Go, zero deps).
8. **Treat HEVC in the browser as "usually" not "always": negotiate per client.** In 2026 HEVC decodes on
   Safari (universal), Chrome/Edge on Windows/macOS/Android with a hardware decoder (Chrome MSE 107+,
   WebRTC 136+), Firefox 134-137+ hardware-only; it is absent on Firefox WebCodecs/WebRTC and on Linux
   Chrome without VAAPI. Order of preference: WebRTC/MSE HEVC passthrough -> WebCodecs HEVC (canvas) ->
   substream H.264 -> on-demand NVENC transcode to H.264 (P2200 has unlimited sessions but ~8-10 x 4 MP
   worth of throughput) -> WASM decode for tiny views.
9. **Consider switching the main streams to H.264 for universal playback only if storage allows.** Our
   Hikvisions support H.264 at 4 MP/8 MP; the cost is ~1.9x bitrate (approx. 1.6 vs 0.85 TB/day, ~11 vs
   ~20 days on 17 TB). A middle path is HEVC main streams plus H.264 substreams at a higher resolution
   (e.g. 1280x720 third stream) for non-HEVC clients.
10. **Do not use H.265+/H.264+ smart codecs for the recorded stream.** They inflate GOP length (breaks
    seek granularity and pre-buffer sizing, cf. ZM's "keyframe interval larger than this setting" errors)
    and some decoders mis-handle them; use plain HEVC VBR with a fixed 1-2 s GOP. (3.2, 1.4)
11. **Build the timeline from metadata only and make scrubbing use tiny previews.** Frigate's hourly
    180-px motion-only preview MP4 is "smaller than a single snapshot"; Agent DVR and Blue Iris draw
    motion-level bars and only show thumbnails when zoomed; UniFi/Nx use motion metadata. Full-res playback
    starts only when scrubbing stops. Never fetch per-frame JSON or per-frame JPEGs for a timeline (ZM Montage
    Review's fatal design, issue #4036).
12. **Pre-generate thumbnails/previews at ingest from the substream, asynchronously, and store them next to
    the segment**, never by spawning `ffmpeg -ss` against a 4 MP fMP4 on page load (ZM). A 5 fps substream
    keyframe or a JPEG every N seconds costs almost nothing while the stream is already decoded for motion.
13. **Serve recordings with range-capable, index-first containers.** Either moonfire-style on-the-fly MP4
    from a frame index, or Frigate-style HLS/fMP4 playlists from segment files. Both give sub-second first
    frame for any time range and let a browser seek without downloading the file (contrast ZM's 326 s vs
    0.49 s measurement).
14. **Multi-camera review must play substreams** (Blue Iris records both streams and uses the sub for grid
    timeline playback; Nx records hi and low quality; Scrypted and UniFi keep both). Record the 640x360
    substream too (cheap: ~0.3-0.5 Mbps each) so 23-up review never decodes 4 MP.
15. **One process (or a small set of Rust/Go services) with async I/O, not one C++ process per camera plus
    Perl daemons plus PHP.** moonfire runs 6 x 1080p on a Pi 2 at <10 % CPU; go2rtc and Kerberos are
    single Go binaries; zm-api shows the ZM community itself is moving the API/daemons to Rust while leaving
    `zmc` untouched, which is the part that hurts us most.
16. **Metadata on flash, video on spinning disk, staggered writes.** moonfire's explicit rationale
    (sub-ms index access, no seek storms) and Nx's equal-fill multi-drive writer apply directly to our two
    94 %-full volumes: keep SQLite/Postgres + previews on SSD, spread segments across both volumes by fill
    ratio, and let the recorder own free-space management.
17. **Keep camera-side analytics as a first-class trigger.** Scrypted waits for the camera's motion/ONVIF
    events before running analysis; Hikvision AcuSense cameras emit ONVIF/ISAPI events. Using these to gate
    server-side detection cuts substream inference further and gives "line crossing"/"intrusion" semantics
    for free.
18. **Design the API around time ranges, not events.** UniFi, Nx, Frigate and moonfire all expose
    "camera x [t0,t1)" -> video/previews/markers; events are queries over that. This makes export, review,
    retention and multi-server trivial and avoids ZM's `Events` + `Frames` + `Events_Hour/Day/...` triad.
19. **Budget GPU for the future, not the P2200.** For continuous main-stream decode of 23 HEVC cameras
    (only needed if we insist on server-side analytics at full resolution) an Arc A310/B50 (two media
    engines, HEVC/AV1) or an RTX A2000/4060-class NVDEC suffices; for detection any of Arc/OpenVINO,
    Hailo-8, Intel NPU, or an RTX card beats a Coral. The redesign should work with zero GPU (substream-only
    decode, CPU YOLO-tiny) and scale up when a GPU is present, exactly like Frigate.
20. **Sunset the JPEG path entirely.** No `Save JPEGs`, no `zms` MJPEG, no RGB shared-memory ring. Every
    consumer (live, review, thumbnails, detection) works from either the compressed bitstream or the decoded
    substream; this is the single largest architectural difference between ZoneMinder and every faster NVR
    surveyed here.
