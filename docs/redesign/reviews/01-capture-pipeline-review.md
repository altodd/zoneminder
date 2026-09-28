# 01 — Capture / Analysis / Streaming pipeline review

Scope: the C++ core of the ZoneMinder 1.39.35 fork at `zoneminder/src/` (zmc, zms, zmu,
zm_rtsp_server). All paths below are relative to `zoneminder/`; line numbers are from the
tree as reviewed. Production facts (23 Hikvision cameras, HEVC 2688x1520 main @ ~18 fps,
H.264 640x360 sub @ 20 fps, one 4K, per-monitor config as listed in the task) are taken as
given and not re-measured.

---

## 0. Executive summary

1. **`AnalysisSource=Secondary` is dead configuration.** The value is read from the DB
   (`src/zm_monitor.cpp:404`) and never referenced again anywhere in `src/`. Every zmc
   decodes the **primary** HEVC 2688x1520 stream in full and runs motion detection on the
   **primary** Y plane. The 640x360 substream is only ever opened by `FfmpegCamera` as an
   *audio* source when the primary has no audio (`src/zm_ffmpeg_camera.cpp:819-845`), and
   handed to go2rtc/RTSP2Web as a live-view URL. This single fact explains NVDEC at 100 %:
   23 x 18 fps x 4.08 Mpx = ~1.7 Gpx/s of HEVC is being pushed through one Pascal NVDEC.

2. **`Decoding=Always` decodes every frame, and every decoded frame is copied ~5 times on
   the CPU.** Per frame: GPU->CPU transfer of a 6.1 MB NV12 surface into pageable memory
   (`src/zm_packet.cpp:145-162`), `sws_scale` NV12->YUV420P into a freshly `new`'d 6.1 MB
   `Image` (`src/zm_monitor.cpp:3670-3681`), timestamp overlay, then a 6.1 MB memcpy into
   the mmap ring (`src/zm_monitor.cpp:3738`, `src/zm_image.cpp:1173`). The analysis thread
   then **blends the 4 MB Y plane into the reference image on every frame at 18 fps**, not
   at the 5 fps analysis rate (`src/zm_monitor.cpp:2448-2455` sits outside the
   `motion_frame_skip` gate at `:2379`). That is ~800 MB/s of memory traffic per camera,
   ~18 GB/s across 23 processes, which is why per-byte CPU cost inflates.

3. **This fork does NOT convert to RGB32** (upstream does). `Colours=4` only sizes the
   shared-memory slot to the RGBA upper bound (`src/zm_monitor.cpp:996-1003`), which is why
   each mmap is 98 MB: two rings x 3 slots x 16,343,040 bytes (2688x1520x4), even though
   the bytes actually written are YUV420P (6.1 MB per slot).

4. **Zones are not scaled.** Pixel-unit polygons are used verbatim at the monitor's
   2688x1520 resolution (`src/zm_zone.cpp:1028-1034`, `:76-79`). A zone drawn at 1920x1080
   silently covers only the top-left 1920x1080 rectangle (~51 % of the frame), with no
   warning. Only percent-unit coordinates (containing a `.`) are rescaled (`:1020-1027`).

5. **Event playback of HEVC goes through zms software decode.** The event page only uses
   the native `<video>` path when `DefaultVideo` contains `h264`/`av1`
   (`web/skins/classic/views/event.php:154-155`); an `*.hevc.mp4` falls back to zms MJPEG
   (`:446-460`), which opens the mp4 with the first "auto" decoder = software `hevc`
   (`src/zm_ffmpeg.cpp:48`, `src/zm_ffmpeg_input.cpp:97-118`) with libavcodec's default
   single thread. Montage review at <1x speed spawns one zms per frame (`mode=single`).

6. **Frames/Stats rows are per-frame during any alarm/alert state** (`src/zm_event.cpp:660-666`),
   batched 100 at a time (`:41`, `:690-700`), plus a synchronous `UPDATE Events` per batch
   and 5 inserts per event open. Stats rows are per zone per analysed frame
   (`:549-568`). 243 M Frames rows is the expected outcome of this design at 23 cameras.

7. The pieces worth keeping are small and clean: the passthrough muxer with fragmented MP4
   + byte-range HLS index (`src/zm_videostore.cpp:608-611`, `:1426-1470`, `:1632-1652`),
   the per-monitor stream socket fan-out (`src/zm_stream_socket.cpp`), and the zone/blob
   algorithm (`src/zm_zone.cpp:215-836`). The architecture around them — one process per
   camera decoding everything into a JPEG-oriented shared-memory ring, analysis welded to
   the primary decode, per-frame DB rows, per-viewer zms decode — is the root cause and
   argues for a new core.

---

## 1. What the production configuration actually means in this code

| Setting | Effect in code | Reference |
|---|---|---|
| `Capturing=Always` | main thread loops `PreCapture/Capture/PostCapture` per monitor | `src/zmc.cpp:333-393` |
| `Analysing=Always` | `shared_data->analysing` set; analysis thread always started | `src/zm_monitor.cpp:1191`, `:4468-4474` |
| `AnalysisSource=Secondary` | **no effect** — loaded at `:404`, never read | `grep -rn analysis_source src/` |
| `AnalysisImage=YChannel` | motion detection on `in_frame->data[0]` wrapper, no copy | `src/zm_packet.cpp:306-307`, `src/zm_monitor.cpp:2382-2390` |
| `AnalysisFPSLimit=5` | `motion_frame_skip = capture_fps/5`; gates **only** `DetectMotion`, not blend or decode | `src/zm_monitor.cpp:2353-2358`, `:2379` |
| `Recording=Always` | an Event always exists; `openEvent` at `:2678-2693` | |
| `RecordingSource=Primary` | loaded `:410`; VideoStore is built from `GetVideoStream()` (primary) `src/zm_event.cpp:857-864` | |
| `VideoWriter=2 (passthrough)` | `keep_keyframes` in packetqueue; packet copied to muxer untouched | `src/zm_monitor.cpp:601`, `src/zm_videostore.cpp:1426-1470` |
| `Decoding=Always` | every video packet sent to the decoder | `src/zm_monitor.cpp:3510-3511` |
| `DecoderHWAccelName=cuda` | `av_hwdevice_ctx_create(CUDA)` + `get_format` callback; decoder is ffmpeg `hevc` with hwaccel | `src/zm_ffmpeg_camera.cpp:671-773` |
| `Colours=4` | only feeds `Camera::ImageSize()`; SHM slot forced to RGBA bound anyway | `src/zm_monitor.cpp:996-1003` |
| `ImageBufferCount=3` | 3 capture slots + 3 analysis slots in mmap | `src/zm_monitor.cpp:1008-1012` |
| `MaxImageBufferCount=2000` | packetqueue GOP purge threshold | `src/zm_monitor.cpp:600`, `src/zm_packetqueue.cpp:151-180` |
| `PreEventCount=5` | `pre_event_video_packet_count=max(5, alarm_frame_count)` | `src/zm_monitor.cpp:599` |
| `StreamChannel=CameraDirectSecondary` | browser asks go2rtc for `<id>_CameraDirectSecondary` = the camera's own substream URL | `web/js/MonitorStream.js:505-525`, `src/zm_monitor_go2rtc.cpp:286-295` |
| Zone `Area` at 1080p | polygon used unscaled on a 2688x1520 mask | `src/zm_zone.cpp:76-79`, `:1028-1034` |

The 98,063,928-byte mmap is reproduced by the `connect()` formula: 2 rings x 3 x
16,343,040 = 98,058,240 bytes of image slots plus ~5.7 KB of header
(`SharedData`=888, `TriggerData`=560, `VideoStoreData`=4128, 3 x `timeval`, per-slot
pixfmt arrays, alignment slack; `src/zm_monitor.cpp:1004-1020`). The identical size on all
23 files, including the 4K unit, implies the 4K monitor's `Width/Height` are also
2688x1520 in the DB — which turns its decode into a real bicubic downscale of an 8 Mpx
NV12 frame every frame (`src/zm_monitor.cpp:3250-3256`, `:3691-3693`).

---

## 2. Q1 — What is decoded per frame from the primary stream, and why

**Pipeline per video packet (Decoding=Always):**

1. Main thread: `Monitor::Capture()` reads one AVPacket from the primary RTSP context
   (`src/zm_ffmpeg_camera.cpp:294`), wraps it in a `ZMPacket`, forwards it to the stream
   socket (`src/zm_monitor.cpp:3158-3160`, zero-copy), bumps `image_count`, and
   `packetqueue.queuePacket()` (`:3217-3220`). Nothing is decoded here.

2. Decoder thread (`src/zm_decoder_thread.cpp:31-44` -> `Monitor::Decode()`):
   - `should_decode` is true for every packet because `decoding == DECODING_ALWAYS`
     (`src/zm_monitor.cpp:3509-3515`). `avcodec_send_packet` at `:3550`.
   - `receive_frame()` gets a CUDA frame and **immediately** calls
     `av_hwframe_transfer_data()` into a freshly allocated software frame
     (`src/zm_packet.cpp:145-162`). For 8-bit HEVC on CUDA the transfer format is NV12:
     2688 x 1520 x 1.5 = **6,128,640 bytes** per frame, copied device->host into pageable
     memory (a driver-staged copy; the CPU does the second half).
   - Phase 3 (`src/zm_monitor.cpp:3622-3682`): NV12 is *not* in the pass-through list
     (`:3654-3660`: YUV420P/YUVJ420P/YUV422P/YUVJ422P/GRAY8/RGB24/RGB32), so the code
     picks YUV420P (`:3665-3667`, mapped by `src/zm_pixformat.h:29-31`), allocates a new
     6,128,640-byte `Image` (`:3670` -> `src/zm_image.cpp:210 AllocImgBuffer`), builds a
     `SwsContext` NV12->YUV420P with `SWS_BICUBIC` (`:3250-3256`) and runs `sws_scale`
     (`src/zm_image.cpp:442`). Same-size NV12->YUV420P is swscale's unscaled
     plane-deinterleave path; it is a copy, not arithmetic, but it is 6 MB read + 6 MB
     written.
   - Phase 4 (`:3688-3698`): `get_y_image()` wraps `in_frame->data[0]` with the real stride
     (`src/zm_packet.cpp:306-307`) — no copy, but this **pins the 6 MB NV12 `in_frame` for
     the life of the packet**; nothing ever releases `in_frame` before `~ZMPacket`
     (`src/zm_packet.cpp:92-96`; the only reset site is the V4L2 synth at
     `src/zm_monitor.cpp:3767`; the old `out_frame = nullptr` is commented out at `:2743`).
   - Phase 5 (`:3705-3748`): orientation (no-op), privacy mask (none), **timestamp
     `Annotate`** on the YUV420P image (`:3730-3732`, 1-byte-per-pixel path
     `src/zm_image.cpp:2685-2700`, cheap), then `WriteShmFrame()` = `Image::Assign` =
     `fptr_imgbufcpy` of 6,128,640 bytes into the mmap slot (`:3738`,
     `src/zm_image.cpp:1173`), timestamp, `last_write_index` publish.

3. Analysis thread (`src/zm_monitor.cpp:2144`): waits for `packet->decoded` (`:2340-2347`),
   then on **every** video frame: `ref_image.Blend(*y_image, ref_blend_perc)`
   (`:2448-2455`; SSE2 `sse2_fastblend` over the 4,085,760-byte Y plane, reading two
   planes and writing one, `src/zm_image.cpp:2323`); on every `(motion_frame_skip+1)`-th
   frame additionally `DetectMotion()` (`:2379-2394`) = `Image::Delta` over the Y plane
   (`:4001`, `src/zm_image.cpp:2437-2519`, SSE2 gray8) + per-zone `CheckAlarms`
   (`src/zm_zone.cpp:215-836`), and if the frame scores, a full 6.1 MB copy of
   `packet->image` into `packet->analysis_image` (`:2400-2401`).

4. Event thread (`src/zm_event.cpp:903-944`): waits for `analyzed`, `writePacket()` the
   original compressed packet (passthrough), `AddFrame()`, then `delete packet->image`
   (`:923-929`) — but not `in_frame`.

**Is there RGB32 conversion?** Not in this fork. Upstream 1.36/1.37 converts to
`camera->Colours()` (RGB32 here); this fork's "no-conversion" SHM (`src/zm_monitor.cpp:3276-3303`)
writes whatever format the decoder produced and records it per slot
(`image_pixelformats[index]`). Readers adopt it in `ReadShmFrame()` (`:3305-3345`).

**Per-frame byte budget at 2688x1520** (decoder thread, per frame):

| Step | Bytes | Where |
|---|---|---|
| NVDEC surface -> host NV12 | 6,128,640 written (driver copies through pinned staging) | `src/zm_packet.cpp:148` |
| `new Image` YUV420P (fresh 6 MB allocation, page-faulted) | 6,128,640 | `src/zm_monitor.cpp:3670` |
| `sws_scale` NV12->YUV420P | 6.1 MB read + 6.1 MB write | `src/zm_image.cpp:442` |
| `Annotate` | ~few KB | `src/zm_monitor.cpp:3731` |
| memcpy into mmap slot | 6.1 MB read + 6.1 MB write | `src/zm_image.cpp:1173` |
| **Total decoder-thread traffic** | **~31 MB/frame, ~550 MB/s at 18 fps** | |
| Analysis: blend Y plane every frame | 12.3 MB/frame, 220 MB/s at 18 fps | `src/zm_monitor.cpp:2452` |
| Analysis: delta + zone pass at 5 fps | ~12 MB + zone extent x3, ~80 MB/s | `:4001`, `src/zm_zone.cpp:299` |
| Would-be RGB32 (upstream behaviour) | 16,343,040 per frame (2688x1520x4); sws would do real YUV->RGB math | |

Aggregate over 23 cameras: roughly 18-20 GB/s of memory traffic in the decoder and
analysis threads alone. On a two-channel DDR4 host that is a large fraction of practical
bandwidth, so each memcpy/sws call runs slower than its single-process benchmark and the
per-thread CPU % inflates accordingly.

---

## 3. Q2 — What the other `Decoding` modes change

Gate logic is in `Monitor::Decode()`:

```cpp
// src/zm_monitor.cpp:3510-3515
bool should_decode = !already_decoded && (
  (decoding == DECODING_ALWAYS) ||
  ((decoding == DECODING_ONDEMAND) && (hasViewers() || shared_data->last_write_index == image_buffer_count)) ||
  ((decoding == DECODING_KEYFRAMES) && (packet->keyframe || decoder_requires_next_packet)) ||
  ((decoding == DECODING_KEYFRAMESONDEMAND) && (hasViewers() || packet->keyframe || decoder_requires_next_packet))
);
```

`hasViewers()` = a zms set `last_viewed_time` within 10 s (`src/zm_monitor.h:956-964`,
set from `src/zm_monitorstream.cpp:508/650/722`). go2rtc/WebRTC viewers do **not** touch
it, so with `StreamChannel=CameraDirectSecondary` live viewing never counts as "viewers".

| Mode | Decoded | Live via zms | Console thumbnail / `zmu -i` / `mode=single` | snapshot.jpg / alarm.jpg | Motion detection | Frames rows |
|---|---|---|---|---|---|---|
| `Always` | every frame | full rate | yes | yes | at AnalysisFPSLimit | yes |
| `OnDemand` | first frame, then only while a zms viewer is attached | full rate while viewed | stale until viewed | first frame only; later frames have no image | **effectively off** (needs `packet->image`, `:2362`) | yes |
| `KeyFrames` | I-frames only (+ the follow-up packet the hwaccel needs, `:3555-3558`) | ~1 frame per GOP | yes, refreshed per GOP | yes (event start is a keyframe, `src/zm_packetqueue.cpp:695-698`) | **1 frame per GOP only** | yes |
| `KeyFrames+OnDemand` | I-frames always, all frames while a zms viewer is attached | full rate while viewed | yes | yes | 1/GOP unless viewed | yes |
| `None` | nothing; decoder thread not created (`:4450-4459`) | none (`sendTextFrame`) | blank/stale slot (`Capture` publishes an index without an image, `:3147-3152`) | none (`AddFrame` skips when `!packet->image`, `src/zm_event.cpp:610`) | off | **still written** (`src/zm_event.cpp:482-484` calls `AddFrame` for every video packet) |

`Analysis` images (the annotated ring, `WriteAlarmImage`) are only produced when a zms
analysis viewer is attached (`src/zm_monitor.cpp:2443-2446`), in every mode.

zmwatch is unaffected by `None`: `Decode()` and `Capture()` keep `last_write_time` fresh
(`:3149`, `:3613-3615`), and zmwatch keys on heartbeat/last-write time
(`scripts/zmwatch.pl.in:164-170`, `:182-193`).

**Bottom line:** no `Decoding` value gives "decode the substream". `KeyFrames` is the only
mode that keeps thumbnails, snapshots and event-start images working while cutting NVDEC
load by the GOP length, but it reduces motion detection to one sample per GOP.

---

## 4. Q3 — The secondary stream, and zone scaling

**Capture.** `FfmpegCamera` opens exactly one video input (`mFormatContext`,
`src/zm_ffmpeg_camera.cpp:508-548`). The `SecondPath` is opened via `FFmpeg_Input` **only
if the primary has no audio stream** (`:819-845`), and then only its audio stream id/codec
are taken (`:839-841`). `Capture()` reads from `mSecondFormatContext` solely to interleave
audio (`:258-287`). If the primary contains a second video stream, ZM picks the one whose
dimensions match the monitor (`:570-579`) and discards the rest. There is no second video
decoder, no second decoder thread, no hwaccel for it. `RecordingSource` (`:410`) is loaded
and, like `AnalysisSource`, unused in the C++ core.

**The analysis image is therefore the primary's Y plane at 2688x1520**, wrapped from the
NV12 frame at `src/zm_packet.cpp:306-307`. If the decoded frame's size differs from the
monitor's `camera_width/height` (the 4K unit configured as 2688x1520), the Y plane is
`Scale()`d with swscale first (`src/zm_monitor.cpp:3691-3693`, `src/zm_image.cpp:3497-3540`)
— an extra full-frame scale on top of the NV12->YUV420P scale in phase 3.

**Zone scaling.** `Zone::Load()` (`src/zm_zone.cpp:954-1073`):

```cpp
// src/zm_zone.cpp:1020-1034
if (strchr(Coords, '.')) {
  if (!ParsePercentagePolygon(Coords, monitor->Width(), monitor->Height(), polygon)) { ... }
} else {
  if (!ParsePolygonString(Coords, polygon)) { ... }   // raw pixels, no scaling
}
```

`Zone::Setup()` rasterises the polygon into a `monitor->Width() x monitor->Height()` mask
(`:76-79`) and builds per-row ranges (`:81-97`). `CheckAlarms()` clamps `hi_x/hi_y` to the
delta image only if they exceed it (`:277-286`). A pixel-unit zone drawn at 1920x1080 on a
2688x1520 monitor is therefore **neither clipped nor scaled**: it is applied as the
top-left 1920x1080 rectangle — 71 % of the width, 71 % of the height, ~51 % of the area —
and the right/bottom bands of every frame are never analysed. `MinAlarmPixels` &c. are
absolute pixel counts (`:1049-1054`), so thresholds tuned at 1080p are also off by the
pixel-density ratio. Percent-unit zones (this fork's `ParsePercentagePolygon`,
`:875-915`) are scaled correctly and their thresholds converted from % of polygon area
(`:1040-1047`).

---

## 5. Q4 — Shared-memory layout and its readers

Layout from `Monitor::connect()` (`src/zm_monitor.cpp:1004-1020`, pointers `:1121-1126`,
`:1181-1186`):

```
+0        SharedData            888 B   (static_assert, src/zm_monitor.h:314)
+888      TriggerData           560 B   (src/zm_monitor.h:331-339)
+1448     zone_scores           4 B x zone_count
+1452     VideoStoreData        4128 B  (current_event, event_file[4096], recording timeval)
+5580     shared_timestamps     16 B x ImageBufferCount
+...      [align to 64]
          capture image ring    ImageBufferCount x slot     slot = av_image_get_buffer_size(RGBA,W,H,32) = 16,343,040
          analysis image ring   ImageBufferCount x slot     (annotated/alarm images, this fork)
          image_pixelformats            ImageBufferCount x 4 B  (per-slot AVPixelFormat)
          analysis_image_pixelformats   ImageBufferCount x 4 B
```

Notable:
- The slot is always the RGBA upper bound regardless of what is written
  (`:996-1003`), so 62 % of every slot is unused when the payload is YUV420P.
- The per-slot `AVPixelFormat` arrays are how zms learns what bytes are in the slot
  (`ReadShmFrame`, `:3305-3345`; `GetAlarmImage`, `:1442-1475`).
- `SharedData` also carries `last_viewed_time` / `last_analysis_viewed_time` (viewer
  presence for OnDemand), `stream_socket_path` (`src/zm_monitor.h:273`), audio level
  fields, and the analysis-ring index.

Readers: **zms** (`MonitorStream`, `src/zm_monitorstream.cpp:818-866`), **zmu**
(`-i/-f/-a`, `src/zmu.cpp:603-657` via `Monitor::GetImage`), the Perl `Memory.pm` and PHP
`Monitor.php` readers (offsets asserted at `src/zm_monitor.h:304-322`), and zmwatch through
Perl. `zma` no longer exists as a live consumer: analysis runs in-process in zmc. The
`zma` binary in this fork (`src/zma.cpp:20-60`, `src/CMakeLists.txt:222`) is an offline
event re-analysis tool (`zma -e <event_id>`) that decodes the stored mp4 with
`FFmpeg_Input` and re-runs `AnalyseFrame` (`:486-577`); it only attaches to SHM to load
zones (`:477`). Linked monitors read each other's `SharedData` through `MonitorLink`
(`src/zm_monitor.h:351-402`).

---

## 6. Q5 — Database writes per event/frame

**Event open** (`Event::Event`, `src/zm_event.cpp:47-194`): 1 synchronous
`INSERT INTO Events` retried until it succeeds (`:155-157`), then 4 queued inserts into
`Events_Hour/Day/Week/Month` (`:160-175`) and 1 `Event_Summaries` upsert (`:182-191`);
`Event::Run` then does `UPDATE Events SET DefaultVideo` (`:892-893`) and, if the storage
path fails, further updates.

**Per video packet** (`AddFrame`, `:581-718`): a `Frame` object is queued when

```cpp
// src/zm_event.cpp:660-666
bool db_frame = ( frame_type == BULK ) or ( frame_type == ALARM ) or ( frames == 1 )
             or ( score > max_score )
             or ( monitor_state == Monitor::ALERT ) or ( monitor_state == Monitor::ALARM )
             or ( monitor_state == Monitor::PREALARM );
```

So: idle = 1 row per `BULK_FRAME_INTERVAL` (default 100, `ConfigData.pm.in:2592`) plus
score maxima; **any alarm/alert = one row per captured frame (18/s)**, not per analysed
frame. Rows are flushed by `WriteDbFrames()` (`:531-579`) as one multi-row
`INSERT INTO Frames (EventId, FrameId, Type, TimeStamp, Delta, Score, AudioLevel)` when 100
are queued, or 5 s x fps, or on a BULK frame, or when snapshot/alarm images were written
(`:690-700`). Each flush is accompanied by an `UPDATE Events SET Length, Frames, AlarmFrames,
TotScore, AvgScore, MaxScore, MaxScoreFrameId` (`:702-712`). `Stats` rows are written in
the same flush, one per zone per frame that carries `zone_stats` (`:549-568`) — `zone_stats`
are attached on every frame where `DetectMotion` ran, scored or not
(`src/zm_monitor.cpp:2404-2408`), so ~5 rows/s/camera/zone while in alarm/alert with
`RECORD_EVENT_STATS=1` (default, `ConfigData.pm.in:1624`). All of these go through the
single `dbQueue` thread (`src/zm_db.cpp:371`).

**Event close** (`~Event`, `:196-357`): `videoStore->finalize()`, m3u8 rewrite, hard-link
`incomplete.*` -> final name, flush remaining frames, a `readdir`+`stat` of the event
directory to sum size (`:303-319`), one synchronous `UPDATE Events` (`:326-342`), one queued
`UPDATE Storage`. It runs on a detached `close_event_thread` (`src/zm_monitor.cpp:3953`).
With `SectionLength=600` and 23 cameras that is an event open+close every ~26 s fleet-wide.

**Memory vs PreEventCount / MaxImageBufferCount.** The packetqueue keeps at least
`max(PreEventCount, AlarmFrameCount)` video packets and, with passthrough, never trims past
the last keyframe before the slowest iterator (`src/zm_packetqueue.cpp:251-268`, `:326-409`).
So the steady-state depth is 1-2 GOPs. `MaxImageBufferCount=2000` is only the emergency
purge threshold (`:151-180`, drops whole GOPs — including from the recording if the event
iterator is behind). The real memory cost is not the compressed packets (~40 KB each) but
what hangs off each `ZMPacket` while it is queued: the 6.1 MB NV12 `in_frame` (never
released early) plus the 6.1 MB `image` until the event thread deletes it. Two Hikvision
GOPs (~50 frames each at I-interval 50) is ~600 MB-1.2 GB of RSS per zmc if decoding is
keeping up, and up to 2000 x 6 MB if it is not.

---

## 7. Q6 — Event playback (zms `EventStream`)

Decision in `web/skins/classic/views/event.php`:

```php
// :154-155
$video_tag = ($codec == 'MP4') || ($codec == 'MP4HLS') ||
  ((false !== strpos($Event->DefaultVideo(), 'h264') || false !== strpos($Event->DefaultVideo(), 'av1')) && ($codec === 'auto'));
```

`$codec` defaults to the monitor's `DefaultCodec`, which defaults to `auto`
(`:96-104`, `web/includes/Monitor.php:342`). Event files are named
`<id>-video.<codec>.mp4` (`src/zm_event.cpp:876`), so for HEVC the name is
`*.hevc.mp4` and `$video_tag` is **false**. The page then falls through to the zms MJPEG
path (`:446-460`): `?view=nph-zms&source=event&mode=jpeg`.

In zms, `EventStream::loadEventData` opens the mp4 with `FFmpeg_Input`
(`src/zm_eventstream.cpp:410-419`), which picks the first matching entry of `dec_codecs`
for `"auto"` — `hevc` **software** (`src/zm_ffmpeg.cpp:48`; `hevc_cuvid` is at `:50` and
the VAAPI/QSV entries carry a hwdevice type, CUDA entries do not) — and never sets
`thread_count` (`src/zm_ffmpeg_input.cpp:97-118`), i.e. libavcodec's default single
thread. Each frame is then `get_frame(stream, offset)` (seek + decode, `:255-310`),
converted to an `Image` at monitor size (`src/zm_eventstream.cpp:970-975`), scaled,
JPEG-encoded (`:1025`) and written to the socket. One 2688x1520 HEVC single-threaded
software decode per frame per viewer is 30-80 ms on a modern core; real-time 18 fps
playback is not achievable, and every open event page costs ~1 core.

When `$video_tag` is true (`DefaultCodec=MP4`/`MP4HLS`, or an H.264 file), the browser
gets `<video>` with the fork's byte-range HLS playlist (`view=view_hls`) or the mp4 via
`view=view_video` (`:355-383`). `view_video.php` serves the file with a PHP
`fread(16 KB)`/`echo` loop honouring `Range` (`web/views/view_video.php:128-157`) — no
sendfile, but no decode.

Montage review / timeline: at >= 1x speed it uses zms EventStreams (same MJPEG path); at
< 1x, paused or live it issues one `mode=single` zms request per monitor per tick
(`web/skins/classic/views/js/montagereview.js:373-382`, `:826-834`, `:992-995`) — each a
fresh process, mp4 open, seek, software HEVC decode, JPEG.

---

## 8. Q7 — Live streaming paths

1. **go2rtc / RTSP2Web / Janus (the `<video>` path).** When any of them is enabled,
   `getStreamHTML` emits a `<video>` element (`web/includes/Monitor.php:1297-1301`) and
   `MonitorStream.js` maps `StreamChannel` to a go2rtc stream name
   (`web/js/MonitorStream.js:505-525`). zmc registers those names via the Poller thread
   every 10 s (`src/zm_monitor.cpp:2103-2136`, `src/zm_monitor_go2rtc.cpp:215-302`):
   `<id>` = primary URL (or the ZM RTSP restream), `<id>_h264` = an ffmpeg transcode
   source inside go2rtc (`:250-259`), `<id>_CameraDirectPrimary`, `<id>_CameraDirectSecondary`
   = the camera URLs with credentials. **zmc's decode plays no part**; the browser gets
   WebRTC/MSE/HLS straight from go2rtc's own RTSP pull of the camera. Full-res HEVC live
   view therefore requires only (a) a browser that decodes HEVC in MSE/WebRTC (Safari,
   Chromium with platform HEVC, Edge) or (b) go2rtc's `_h264` transcode source, which
   costs a software or NVENC transcode per camera inside go2rtc.

2. **zms MJPEG from shared memory** (`MonitorStream::runStream`,
   `src/zm_monitorstream.cpp:818-866`): reads `ReadShmFrame(last_write_index)`, applies
   `prepareImage()` (`src/zm_stream.cpp:173-305`) which copies into a static `Image` and
   `Image::Scale()`s with swscale (`:289-293`, `src/zm_image.cpp:3497-3540`, one cached
   context per thread), then `EncodeJpeg` (`:442`) — libjpeg, on the YUV420P bytes. Every
   viewer is a process doing a full-frame scale + JPEG at up to capture rate. This path
   is also what the console thumbnails, `mode=single`, `zmu -i` and the analysis overlay
   use.

3. **Stream socket** (fork addition): every packet is fanned out zero-copy to
   `PATH_SOCKS/stream_<id>.sock` consumers (`src/zm_monitor.cpp:3158-3165`,
   `src/zm_stream_socket.cpp:299-374`). The only in-tree consumer is the
   `zm_rtsp_server` binary (`src/zm_rtsp_server.cpp:88-138`), which is what the
   `_ZoneMinderPrimary` go2rtc source points at when `RTSPServer` is enabled.

4. **MPEG (`STREAM_MPEG`)** re-encodes from SHM per viewer (`src/zm_monitorstream.cpp:415-433`).

---

## 9. Q8 — Where the GPU is used

- Only NVDEC, via libavcodec's `hevc` decoder + CUDA hwaccel
  (`src/zm_ffmpeg_camera.cpp:726-747`). There is no `hevc_cuvid`/`cuvid` parser path in
  zmc (those `dec_codecs` entries have `AV_HWDEVICE_TYPE_NONE`, `src/zm_ffmpeg.cpp:50-51`,
  and are only reachable by naming them in `Decoder`).
- **No GPU-side scaling or colour conversion.** The surface is transferred to host
  memory the instant it is received (`src/zm_packet.cpp:145-162`, comment: "Release GPU
  surface immediately... exhaust the surface pool"). All subsequent work — NV12->YUV420P,
  scaling, blend, delta, JPEG — is CPU. There is no NPP/CUDA filter, no `scale_cuda`, no
  `hwdownload` of a reduced-size frame.
- **No "keep frames on GPU" path** and no shared decoder across monitors: each zmc owns a
  CUDA context and decoder session (the 148 MiB/zmc is the context + NVDEC surface pool).
- The encoder side (`enc_codecs`, `hevc_nvenc`/`h264_nvenc`) is only used for
  `VideoWriter=Encode`, which is off here.

---

## 10. Q9 — Threads per zmc, and which one is hot

Threads created per monitor process (grep of `std::thread(` sites):

| Thread | Created at | Work |
|---|---|---|
| main (capture) | `src/zmc.cpp:333-438` | `av_read_frame`, RTP depacketise, packetqueue push, stream-socket fan-out, `CheckAction`, FPS/`Monitor_Status` |
| decoder | `src/zm_monitor.cpp:4450-4459` -> `src/zm_decoder_thread.cpp:31` | everything in section 2 step 2 |
| analysis | `:4462-4474` -> `src/zm_analysis_thread.cpp:32` | blend every frame, delta+zones at 5 fps, state machine, event open/close decisions |
| event writer | `src/zm_event.cpp:193` (`Event::Run`) | passthrough mux, `AddFrame`, image frees |
| close_event (transient) | `src/zm_monitor.cpp:3953` | event finalisation/DB |
| dbQueue | `src/zm_db.cpp:371` | serialised MySQL writes |
| StreamSocket | `src/zm_stream_socket.cpp:90` | accept/serve consumers |
| Poller | `src/zm_poll_thread.cpp:9` (only with go2rtc/RTSP2Web/Janus/ONVIF/Amcrest) | curl every 10 s |
| ONVIF | `src/zm_monitor_onvif.cpp:227` (if enabled) | event subscription |
| libavcodec workers | `thread_count=2`, `src/zm_ffmpeg_camera.cpp:631-632` | hevc bitstream parsing / hwaccel submission |
| libavformat UDP receiver (rtpUni) | inside ffmpeg | socket read into circular buffer |

Threads are not named (no `pthread_setname_np` in `src/`), so in `top -H` they appear as
`zmc`. Mapping the observed profile to this list:

- **~0-10 % main thread**: consistent with demux + queue + zero-copy fan-out.
- **~0-20 % second thread = analysis**: 18 x 12.3 MB SSE2 blend/s (~220 MB/s) + 5
  delta/zone passes/s over 4 Mpx (`std_alarmedpixels` is a scalar per-row loop,
  `src/zm_zone.cpp:1120-1139`; the blob pass is scalar too) is in the 10-20 % range on
  a contended memory bus.
- **~70-90 % hot thread = decoder**. It is the only thread that, per frame, (a) parses
  HEVC and submits to NVDEC, (b) blocks/spins in `avcodec_receive_frame` ->
  `cuvidMapVideoFrame` while a saturated NVDEC works through a 23-camera backlog,
  (c) does the D2H copy into pageable memory (driver-side memcpy on this thread),
  (d) `sws_scale`s 6 MB, (e) allocates and page-faults a fresh 6 MB `Image` every frame
  (`new Image` at `:3670`; freed by the event thread ~1-2 GOPs later, so glibc keeps
  mmap/munmap-ing large chunks: ~1,500 page faults + kernel zeroing per allocation), (f)
  memcpys 6 MB into the mmap. Items (c)-(f) alone are ~31 MB/frame x 18 = 550 MB/s of
  traffic on a bus shared with 22 other processes doing the same; (b) shows up as CPU
  if the driver's wait is a spin/yield loop, which is common when the engine is
  oversubscribed. The `packetqueue` mutex and `condition.notify_all()` on every packet
  (`src/zm_packetqueue.cpp:189`, ~70 wakeups/s including audio, x3 waiting threads) and
  the per-packet `ZMPacket` mutex/trylock dance (`:513-553`) add context switches but are
  not the bulk of the cycles.

The best single explanation for "~1 core per camera" is therefore: **full-rate 4 Mpx
decode that nobody consumes at full rate**, feeding a 6 MB-per-frame copy chain on the
CPU and a 4 MB-per-frame blend, multiplied by 23 processes fighting for memory bandwidth
and one NVDEC. Two cheap confirmations on the production host, if wanted:
`pidstat -t -u -p <pid> 1` (usr vs sys split of the hot TID — high `%system` = page
faults/driver copies; high `%user` = sws/memcpy/blend) and
`perf top -t <tid>` (expect `__memmove_avx_unaligned_erms`, `sws_*`/`nv12ToPlanar`,
`clear_page_erms`, `libcuda`/`libnvcuvid` symbols).

---

## 11. Q10 — Low-risk changes on the current ZoneMinder

Ordered by impact / risk. Code evidence and what each gives up.

1. **Split each camera into two monitors** (the only way to get "detect on substream,
   record primary" in this code):
   - *Recording monitor*: Path = main stream, `VideoWriter=passthrough`,
     `Recording=Always`, `Analysing=None`, `Decoding=KeyFrames`, `SaveJPEGs=0`,
     `Colours` irrelevant. Cost: one NVDEC decode + one 6 MB copy chain per GOP
     (`:3513`); thumbnails, `snapshot.jpg`, `alarm.jpg` and `mode=single` keep working
     (section 3). Frames rows drop to 1 per 100 frames (`src/zm_event.cpp:592-600`).
   - *Detection monitor*: Path = substream (640x360), `Decoding=Always`,
     `Analysing=Always`, `AnalysisImage=YChannel`, `Recording=None` (or `OnMotion` for
     short low-res clips), `AnalysisFPSLimit=5`, zones **redrawn** at 640x360 or entered
     as percent coordinates (`src/zm_zone.cpp:1020-1027`). Software H.264 decode of
     640x360@20 is ~1-2 % of a core; leave `DecoderHWAccelName` empty to free NVDEC.
     Per-frame copies shrink from 6.1 MB to 345 KB; the mmap shrinks from 98 MB to ~5.5 MB.
   - To have alarms mark the recording monitor's events, set the recording monitor's
     `LinkedMonitors` to the detection monitor: link scores are folded into `score` in
     `Analyse()` regardless of `Analysing` (`src/zm_monitor.cpp:2321-2333`) — verify
     `Ready()`/state behaviour on a test pair before rolling out.
   - Trade-off: two `Monitor_Status` rows, two go2rtc registrations, two event lists
     (filter on the recording monitor), ONVIF/PTZ attach to one of them.

2. **If the split is not possible now: `Decoding=KeyFrames+OnDemand` plus
   `Analysing=None`** on the existing monitors (NVDEC and CPU drop by the GOP length;
   live view is already go2rtc) — accepting the loss of motion detection. With
   `Analysing=Always` it is *not* a useful mode: detection would see one frame per GOP.

3. **`DefaultCodec=MP4HLS` (or `MP4`) on every monitor** (`web/includes/Monitor.php:342`)
   so HEVC events play in `<video>` via the fork's byte-range HLS
   (`web/skins/classic/views/event.php:355-383`) instead of zms software decode. Requires
   HEVC-capable browsers; otherwise use go2rtc's `_h264` for live and consider recording
   the H.264 substream as a second track for playback.

4. **`ZM_RECORD_EVENT_STATS=0`** (`ConfigData.pm.in:1624`): removes all `Stats` inserts
   (`src/zm_event.cpp:549`). Lost: per-zone stats in the frame view.

5. **`ZM_BULK_FRAME_INTERVAL`** 100 -> 1000 (`:2592`): 10x fewer idle Frames rows and
   `UPDATE Events` flushes. Lost: timeline/montage granularity between alarms
   (`montagereview.php:205-207` comment acknowledges this).

6. **`ZM_TIMESTAMP_ON_CAPTURE=0`** (`:859`): drops the per-frame `Annotate` in the decoder
   thread (`src/zm_monitor.cpp:3730-3732`); zms annotates on send instead
   (`src/zm_monitorstream.cpp:406-408`). Small but free. Equivalent: empty `LabelFormat`.

7. **`MaxImageBufferCount`**: 2000 x (packet + 6 MB `in_frame` + 6 MB `image`) is the
   worst-case RSS when decode/analysis falls behind. Setting it to ~3 GOPs (e.g. 150 at
   I-interval 50) bounds memory but makes the GOP purge (`src/zm_packetqueue.cpp:151-180`)
   drop recording data sooner when behind. Keep it high only if RAM allows.

8. **`Colours`, `ImageBufferCount`**: no CPU effect in this fork (`connect()` sizes RGBA
   regardless, `:996-1003`; the ring is never converted). `ImageBufferCount=3` is already
   minimal. Do not bother.

9. **Fix the zones regardless**: redraw or convert to percent units; today the outer ~49 %
   of each 2688x1520 frame is never analysed (section 4).

10. **4K camera**: set its monitor `Width/Height` to the real 3840x2160 or move it to the
    two-monitor pattern; as configured it pays a bicubic 8 Mpx -> 4 Mpx scale per frame
    on top of everything else (`:3250-3256`, `:3691-3693`).

Expected effect of (1)+(3)+(4)+(5): NVDEC from 100 % to ~2-4 % (one 4 Mpx decode per GOP
per camera), zmc CPU from ~1 core to ~0.05-0.1 core per camera pair, Frames growth cut by
one to two orders of magnitude, event playback off the CPU.

---

## 12. Q11 — What to keep, what to replace

**Worth keeping / wrapping (small, sound, tested):**

- `VideoStore` passthrough: `av_packet_ref` + timestamp rebase +
  `av_interleaved_write_frame` into fragmented MP4 with
  `frag_keyframe+empty_moov+default_base_moof` (`src/zm_videostore.cpp:608-611`,
  `:1426-1470`), fragment bookkeeping per keyframe (`:1632-1652`) and a byte-range
  `EXT-X-MAP` HLS playlist (`:1761+`). This is exactly the CMAF-style segment recorder a
  new core needs; it could be lifted almost verbatim, or its behaviour reproduced in
  Rust/Go with the ffmpeg muxer.
- `PacketQueue` semantics (`src/zm_packetqueue.cpp`): keyframe-anchored retention,
  pre-event walk-back to a keyframe (`:640-706`), multi-iterator design. The idea is right;
  the implementation (per-packet mutexes, `notify_all` per packet, iterator
  invalidation caveats noted in comments) should be re-done as a bounded ring of GOPs.
- `StreamSocket` (`src/zm_stream_socket.cpp`): zero-copy per-monitor packet fan-out
  with HELLO/generation semantics. A new core wants precisely this as its internal bus.
- `Zone` motion algorithm (`src/zm_zone.cpp:215-836`, `:1120-1186`) and the SIMD
  delta/blend kernels (`src/zm_image.cpp:3889+`, `:4742+`): compact, deterministic,
  and cheap when run at 640x360. Port as a library function
  `score(y_plane, ref, zones) -> stats`; drop the `Image` class around it.
- FFmpeg camera open/hwaccel negotiation (`src/zm_ffmpeg_camera.cpp:431-872`) as a
  reference for option handling (rtsp_transport, reorder, thread_count, hwaccel list).

**Architectural root causes (argue for a new core):**

1. **Decode-everything-into-shared-memory.** The mmap image ring is the universal
   currency: live view, thumbnails, snapshots, `zmu`, analysis overlay and motion
   detection all assume a full-resolution raster of every frame exists in `/dev/shm`.
   With passthrough recording and go2rtc live view, nothing needs that raster at full
   rate, yet `Decoding=Always` is the only mode in which detection works. The fix is not a
   flag; the consumers must be redesigned around "decode on demand from the recording"
   (thumbnails) and "detect on a small stream".
2. **Analysis welded to the primary decode.** `AnalysisSource`/`RecordingSource` exist in
   the schema and UI but not in the engine (`src/zm_monitor.cpp:404`, `:410`). Building
   a second decoder into `FfmpegCamera` is possible but every downstream assumption
   (zones in monitor pixels, `packet->image` sharing, SHM slot geometry, event snapshot
   from the analysed image) would have to be untangled.
3. **One process, one CUDA context, one decoder session per camera**, no cross-camera
   scheduling, no GPU-side downscale. 23 contexts x 148 MiB and a single saturated
   engine with no back-pressure other than "Decoding is not keeping up" warnings
   (`:3745-3748`) and GOP purges.
4. **Per-frame relational rows** (`Frames`, `Stats`) as the event index, written from the
   hot path through one DB thread, plus `UPDATE Events` per batch. A segment/keyframe index
   plus a sparse detections table is what playback and timeline actually need.
5. **JPEG-oriented streaming and playback**: a process per viewer that scales and
   JPEG-encodes from SHM, and re-decodes the recording in software for playback,
   thumbnails and montage review. Browser-native fMP4/HLS (already produced by the fork's
   muxer) makes zms redundant for playback; go2rtc already makes it redundant for live.
6. Memory lifecycle: per-frame 6 MB allocations, `in_frame` retained for the queue
   lifetime, SHM slots sized 2.7x larger than their payload.

**Shape of the replacement** (consistent with the above): per-camera capture task that
demuxes once and fans out packets (the stream socket idea); a segment recorder writing
fMP4/CMAF with a keyframe index (the `VideoStore` behaviour); a detection task that decodes
the **substream** (software, or one shared NVDEC session with GPU-side NV12 downscale)
at 5 fps and runs the ported zone scorer or an ML detector, emitting sparse detection
records; thumbnails and scrubbing images produced lazily from the nearest keyframe of the
recording and cached; live view delegated to go2rtc/WebRTC; playback via HTTP range /
HLS straight from the segments, with an H.264 substream track or on-demand transcode as
the HEVC-in-browser fallback.
