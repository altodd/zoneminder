# 02 — How the best NVRs are built internally (reference architectures)

Research date: 2026-09-27. Sources are primary (source code, design docs, vendor docs) wherever possible; every claim carries a URL. Numbers marked *(est.)* are my own arithmetic from cited inputs.

Target recap: 23 Hikvision cameras (22 × HEVC 2688×1520 @ 18–20 fps, 1 × HEVC 3840×2160; H.264 640×360 substreams), Ubuntu 24.04, 2 × Xeon Silver 4116 (24c/48t), 125 GB RAM, Quadro P2200 (1 × NVDEC, 1 × NVENC, 5 GB), 17 TB across a 6.3 TB RAID volume and an 11 TB single disk. Requirements: continuous native-resolution recording with zero re-encode, motion/object detection on the substream, instant timeline + thumbnails, full-res live + playback in Chrome/Safari/Firefox on desktop and iOS/Android, scrubbing, export, per-camera permissions (guest users through Tailscale), automation API (Home Assistant).

---

## 1. moonfire-nvr (Rust) — the "index + raw sample files, synthesize MP4 on demand" design

Repo: https://github.com/scottlamb/moonfire-nvr — actively maintained (v0.7.32, 2026-08-14, retina 0.4.20: https://raw.githubusercontent.com/scottlamb/moonfire-nvr/master/CHANGELOG.md).

### 1.1 Claims and numbers
- README: "six 1080p/30fps streams on a Raspberry Pi 2 using less than 10% of the machine's total CPU". No decode, no re-encode; MP4 files are constructed on the fly from an index. https://raw.githubusercontent.com/scottlamb/moonfire-nvr/master/README.md
- design/schema.md: DB is ~4 KB/min of 30 fps video (~2 bytes per frame of index) → ~1 GB for six camera-months; sample-file verification of 0.5 M files / 5.9 TB: `readdir` presence check 19 s, `fstat` size check 120 s, full hash read ~8 h. https://raw.githubusercontent.com/scottlamb/moonfire-nvr/master/design/schema.md

### 1.2 Storage model (design/schema.md + server/db/schema.sql)
- **Recording** = ~1 minute of one RTSP session, split on a key frame, "never more than 5 minutes" (`wall_duration_90k < 5*60*90000` check). Rationale in the design doc: metadata size vs disk seeks vs .mp4 overhead vs crash-loss tolerance.
- **Sample file** = raw concatenation of the compressed samples exactly as they would sit in an `mdat` box; not self-contained; named by recording id inside a **sample file directory** (one per disk, has a 512-byte `meta` file with a `DirMeta` protobuf). SQLite lives on flash, sample files on spinning disk.
- **Run** = all recordings from one RTSP session (`run_offset = 0` marks the start; reconnect starts a new run). Glossary: https://raw.githubusercontent.com/scottlamb/moonfire-nvr/master/design/glossary.md
- Key tables (schema.sql, verbatim excerpts):
  - `stream(id, camera_id, sample_file_dir_id, type in ('main','sub','ext'), config, cum_recordings, cum_media_duration_90k, cum_runs)` — `cum_recordings` is the counter that assigns the next recording id.
  - `recording(composite_id PRIMARY KEY, open_id, stream_id, run_offset, flags, sample_file_bytes, start_time_90k, prev_media_duration_90k, prev_runs, wall_duration_90k, media_duration_delta_90k, video_samples, video_sync_samples, video_sample_entry_id, end_reason)` with `check (composite_id >> 32 = stream_id)` — i.e. **composite_id = stream_id << 32 | recording_index**. Flag bit 1 = "trailing zero" (last frame of a run has duration 0 because the next frame never arrived).
  - Covering index `recording_cover(stream_id, start_time_90k, open_id, wall_duration_90k, media_duration_delta_90k, video_samples, video_sync_samples, video_sample_entry_id, sample_file_bytes, run_offset, flags)` — listing a day never touches the row.
  - `recording_playback(composite_id, video_index blob)` — kept in a separate table so serving a byte range only loads the index of the recordings that range touches.
  - `recording_integrity(composite_id, local_time_delta_90k, local_time_since_open_90k, wall_time_delta_90k, sample_file_blake3)`.
  - `video_sample_entry(id, width, height, rfc6381_codec text e.g. "avc1.4d001f", data blob (the avcC/hvcC sample entry box), pasp_h_spacing, pasp_v_spacing)`.
  - `garbage(sample_file_dir_id, composite_id)` — files to unlink; `open(id, uuid, start_time_90k, end_time_90k, boot_uuid)` — every read-write open gets an **open id** used to disambiguate uncommitted recordings and to detect a directory that was written by a different DB.
  - `signal`, `signal_type`, `signal_change(time_90k PK, changes blob of varints)` — the (still draft) non-video timeseries meant to drive a scrub-bar overlay (motion, alarm state). https://raw.githubusercontent.com/scottlamb/moonfire-nvr/master/design/signal.md
  - `user(..., password_hash PHC/scrypt, permissions blob)`, `user_session(session_id_hash, seed, flags, domain, revocation_*, permissions)`.
- **video_index encoding** (design/schema.md): two varints per sample — (1) zig-zag duration delta with the low bit = key-frame flag, (2) delta of byte size vs the previous frame *of the same type* (key or non-key). It represents `stts`, `stsz`, `stss` in ~2 bytes/sample. Worked example: frames (dur 10/9/11/10/10, key 1/0/0/0/1, bytes 1000/10/15/12/1050) encode to `29 d0 0f | 02 14 | 08 0a | 02 05 | 01 64`.
- **Durability invariants** (design/schema.md): create = write with `O_EXCL`, fsync file, fsync dir, insert row; delete = replace row with `garbage` row, unlink, fsync dir, delete garbage row; startup = lock DB, unlink everything in `garbage`, unlink any file whose id ≥ `cum_recordings`, fsync. Row commits are batched by a periodic flush ("flush_if_sec"), which is why `open_id` exists.
- **Retention**: per-stream `retainBytes` (surfaced in `GET /api/` as `streams[].retainBytes`) inside a per-disk sample-file dir; deletion is oldest-first per stream, via the `garbage` table. No motion-based retention yet (design doc lists "extend retention of motion events" at 1-minute granularity as future).
- **Time**: 90 kHz everywhere; "wall duration" (corrected to NVR clock) vs "media duration" (camera RTP clock); tolerates ≤500 ppm camera drift; per-recording `local_time_delta_90k` corrects the next recording. https://raw.githubusercontent.com/scottlamb/moonfire-nvr/master/design/time.md

### 1.3 Serving (ref/api.md + server/src/mp4.rs)
- `GET /api/cameras/<uuid>/<stream>/recordings?startTime90k&endTime90k&split90k` → `{recordings:[{startId,endId,runStartId,firstUncommitted,growing,openId,startTime90k,endTime90k,videoSampleEntryId,videoSamples,sampleFileBytes,hasTrailingZero,endReason}], videoSampleEntries:{id:{width,height,pixelHSpacing,...}}}`. Adjacent recordings of one run are coalesced into one row.
- `GET .../view.mp4?s=START_ID[-END_ID][@OPEN_ID][.[REL_START]-[REL_END]]&ts=true` — returns a **virtual, unfragmented MP4 with etag and HTTP Range support**, moov before mdat, boxes `ftyp moov(mvhd trak(tkhd edts/elst mdia(mdhd minf(vmhd dinf stbl(stsd stts stsc stsz co64 stss))))) [subtitle trak] mdat` (mp4.rs header comment). Clipping to a non-keyframe start is done with an **edit list**, never by re-encoding. `ts=true` adds a timestamp subtitle track. https://raw.githubusercontent.com/scottlamb/moonfire-nvr/master/ref/api.md
- `GET .../view.m4s` — same selector, but a fragmented media segment for MSE; `GET /api/init/<id>.mp4` — init segment for a `video_sample_entry` (ftyp major brand `iso5`; the code comments note Safari insists on a compatible brand).
- `GET .../live.m4s` — **WebSocket**; each binary message = HTTP-style headers (`X-Video-Sample-Entry-Id`, `X-Recording-Id: <open>.<id>`, `X-Recording-Start`, `X-Prev-Media-Duration`, `X-Media-Time-Range`) + an fMP4 media segment; first message starts on an IDR; `{"type":"drop","frames":N}` text messages when the client falls behind (server jumps to the next key frame). They moved from `multipart/mixed` to WebSocket because Chrome caps HTTP/1.1 at 6 connections but allows ~256 WebSockets → all cameras can stream at once.
- Documented limitations: 404 if the oldest recording is deleted between list and view; connection dropped if a recording is deleted mid-request; cannot append after a trailing-zero frame (#178).
- Auth: `POST /api/login` → `s` cookie (HttpOnly), CSRF token, per-user and per-session `permissions` protobuf (viewVideo, readCameraConfigs, ...). User management at `/api/users/`.

### 1.4 H.265, motion, thumbnails, UI
- H.265 recording landed in v0.7.19 (2025-01-28, issue #33), fixes in v0.7.20; "Browser support may vary" (CHANGELOG). retina depacketizes H.265 per RFC 7798 (https://raw.githubusercontent.com/scottlamb/retina/main/README.md).
- No motion detection (signals API is a draft; README lists "video analytics" as aspiration). No timeline thumbnails, "basic web interface (no scrub bar)", experimental live view, no config UI, no TLS (reverse proxy). React 19/MUI 7 UI as of v0.7.28.
- Retina: RTSP/1.0 client, TCP interleaved + experimental UDP (no reorder buffer), digest auth, H.264/H.265/MJPEG/AAC/G.711, ONVIF metadata; no server, no backchannel, no ONVIF replay, "clean stable API" still open (#47).

**Take-aways for us**: (a) a per-frame index in SQLite + raw sample files is the cheapest possible write path and gives byte-range-seekable MP4 without touching video; (b) the wall/media time model and the open-id crash-recovery protocol are worth copying verbatim; (c) the missing pieces (motion, thumbnails, timeline, retention by event) are exactly the pieces we must add.

---

## 2. Frigate — the "10-second passthrough segments + nginx-vod HLS packaging + review items + previews" design

Repo: https://github.com/blakeblackshear/frigate (dev branch inspected 2026-09-27).

### 2.1 Recording path (`record` role)
- ffmpeg pulls the `record` input and writes **10 s MP4 segments with `-c copy`** into a tmpfs cache; `PRESETS_RECORD_OUTPUT["preset-record-generic"] = ["-f","segment","-segment_time","10","-segment_format","mp4","-reset_timestamps","1","-strftime","1","-c","copy","-an"]` (https://raw.githubusercontent.com/blakeblackshear/frigate/dev/frigate/ffmpeg_presets.py). Optional `force_record_hvc1` rewrites the HEVC tag for Apple players.
- Constants (`frigate/const.py`): `CACHE_DIR="/tmp/cache"`, `RECORD_DIR=/media/frigate/recordings`, `MAX_SEGMENT_DURATION=600`, `MAX_SEGMENTS_IN_CACHE=6`, cache names `{camera}@{%Y%m%d%H%M%S%z}.mp4` (sub-stream recordings `{camera}@sub@...`). Final path `recordings/YYYY-MM-DD/HH/<camera>/MM.SS.mp4` (UTC). https://raw.githubusercontent.com/blakeblackshear/frigate/dev/docs/docs/configuration/record.md
- `RecordingMaintainer` (`frigate/record/maintainer.py`): every cycle lists the cache, keeps the newest file per stream "in use", **ffprobes each closed segment** (`get_video_properties`: duration, codec, audio) and then runs a second ffprobe `-show_entries packet=pts_time,flags` to store **all keyframe offsets (ms) in `Recordings.keyframes`** "so playback never needs to probe"; resolves fractional start from mtime − duration and chains to the previous segment's end; then decides keep/discard from `SegmentInfo(motion_count, active_object_count, average_dBFS, motion_heatmap)` gathered from the detect pipeline over the segment's time window, per retention mode (`all` / `motion` / `active_objects`) and per overlapping review item (alerts vs detections, with pre/post capture). If the mover cannot keep up, older cache segments are dropped ("Unable to keep up with recording segments in cache").
- `Recordings` table (`frigate/models.py`): `id, camera, path, start_time, end_time, duration, motion, objects, dBFS, segment_size (MB), regions, motion_heatmap (16×16 JSON), keyframes (ms offsets JSON), stream_type ('main'|'sub'), has_audio, audio_rate, audio_codec, video_codec`. Also `ReviewSegment(id, camera, start_time, end_time, severity alert|detection, thumb_path, data{objects,zones,sub_labels,...})`, `Previews(id, camera, path, start_time, end_time, duration)`, `Event`, `Timeline`, `Export`, `User(role)`.
- Retention (docs): `record.continuous.days`, `record.motion.days`, `record.alerts.retain.{days,mode}`, `record.detections.retain.{days,mode}`; pre_capture ≤ 60 s; emergency deletion when free space < ~1 h of recording at the current bitrate "regardless of retention settings". Storage page sums `segment_size` from the DB rather than scanning disk. `record.sub` lets you record the substream too with its own retention; playback prefers main and falls back to sub.
- Cleanup (`frigate/record/cleanup.py`): deletes by (continuous/motion expiry) ∪ (per-review-item retention), 100 000 rows per batch, truncates SQLite WAL, expires previews similarly.

### 2.2 Detect path
- ffmpeg decodes the `detect` input at `detect.fps` (default 5) and pipes raw frames: `DETECT_FFMPEG_OUTPUT_ARGS_DEFAULT = ["-threads","2","-f","rawvideo","-pix_fmt","yuv420p"]` (https://raw.githubusercontent.com/blakeblackshear/frigate/dev/frigate/config/camera/ffmpeg.py). Hardware decode presets: `FFMPEG_HWACCEL_NVIDIA = "-hwaccel_device {3} -hwaccel cuda -hwaccel_output_format cuda"` with scale `"-r {0} -vf fps={0},scale_cuda=w={1}:h={2},hwdownload,format=nv12"`; `preset-nvidia-h264`/`preset-nvidia-h265` are aliases of it (ffmpeg_presets.py L92–L143). Docs: image `ghcr.io/blakeblackshear/frigate:stable-tensorrt`, `hwaccel_args: preset-nvidia`. https://raw.githubusercontent.com/blakeblackshear/frigate/dev/docs/docs/configuration/hardware_acceleration_video.md
- Motion (`frigate/motion/improved_motion.py`): luma plane → resize to `frame_height` (small) → percentile contrast stretch → mask → gaussian blur → `absdiff` against a running average → `cv2.threshold(threshold=30)` → dilate → `findContours` → boxes with `contourArea > contour_area`; `skip_motion_threshold` ignores whole-frame changes (IR switch, lightning). Docs: https://raw.githubusercontent.com/blakeblackshear/frigate/dev/docs/docs/configuration/motion_detection.md
- Object detectors (docs/object_detectors.md): Coral, OpenVINO, Hailo, ROCm, Apple Silicon, **TensorRT detector = Jetson only (`-tensorrt-jp6` image)**; **discrete NVIDIA GPUs use the `onnx` detector in the `-tensorrt` image** (ONNX Runtime with CUDA/TensorRT EPs; YOLOv9, RF-DETR, YOLO-NAS, D-FINE). Requirements: CUDA 12.x, driver ≥ 545, compute capability ≥ 5.0. Measured inference (docs/frigate/hardware.md): GTX 1070 YOLOv9-s-320 16 ms, RTX 3050 YOLOv9-t-320 8 ms, **Tesla P40 (Pascal) YOLO-NAS-320 ~105 ms** — a warning for our Pascal P2200: expect tens of ms per inference, so budget ~10–20 inferences/s, which is fine for motion-gated detection on 23 cameras.
- Review items (`frigate/review/maintainer.py`): a `PendingReviewSegment` per camera opens when active objects appear, is upgraded alert↔detection, saves one full-frame thumbnail (`thumb_path`), and closes after `cutoff_time` with no activity. UI: https://raw.githubusercontent.com/blakeblackshear/frigate/dev/docs/docs/configuration/review.md

### 2.3 go2rtc restream (one camera connection, many consumers)
- Frigate bundles go2rtc (v1.9.14 in docs); cameras are declared under `go2rtc.streams` and Frigate's own `record`/`detect` inputs point at `rtsp://127.0.0.1:8554/<camera>` with `preset-rtsp-restream` (`-rtsp_transport tcp -timeout 10000000`). "One connection is made to the camera. One for the restream, detect and record connect to the restream." https://raw.githubusercontent.com/blakeblackshear/frigate/dev/docs/docs/configuration/restream.md
- Live view (docs/configuration/live.md): jsmpeg (≤10 fps, 720p, no go2rtc) / **MSE (native res, default; iPhone needs iOS 17.1+, Firefox H.264 only)** / WebRTC (needs `candidates`, port 8555; used when MSE fails or for two-way talk). For Tailscale: "the Frigate system's Tailscale IP must be added as a WebRTC candidate" (100.64.0.0/10). Camera recommendation: H.264 + AAC, I-frame interval = fps, avoid H.264+/H.265+ "smart" codecs because they remove keyframes.

### 2.4 Playback: nginx `ngx_http_vod_module` packaging recordings as HLS/fMP4 on the fly (IMPORTANT)
Frigate does **not** serve raw segment files to the player. The React UI asks for `/vod/<camera>/start/<ts>/end/<ts>/master.m3u8` (or `/vod/event/<id>`, `/vod/clip/...`, `/vod/<camera>/<main|sub>/start/.../end/...`), nginx's vod module (mapped mode) calls back into the Python API for a JSON **mapping**, then packages an HLS playlist + fMP4 segments straight from the 10 s files, keyframe-aligned, with no transcode.

nginx.conf (https://github.com/blakeblackshear/frigate/blob/dev/docker/main/rootfs/usr/local/nginx/conf/nginx.conf), verbatim:
```nginx
# vod settings
vod_base_url '';
vod_segments_base_url '';
vod_mode mapped;
vod_max_mapping_response_size 1m;
vod_upstream_location /api;
vod_align_segments_to_key_frames on;
vod_manifest_segment_durations_mode accurate;
vod_ignore_edit_list on;
# short leading segments at each playlist start; sources start at
# the seek target, so the ladder applies to every seek. Only
# effective when clips declare real keyFrameDurations
vod_bootstrap_segment_durations 1000;
vod_bootstrap_segment_durations 2000;
vod_bootstrap_segment_durations 4000;
vod_segment_duration 10000;
# MPEG-TS settings (not used when fMP4 is enabled, kept for reference)
vod_hls_mpegts_align_frames off;
vod_hls_mpegts_interleave_frames on;
# file handle caching / aio
open_file_cache max=1000 inactive=5m; open_file_cache_valid 2m; open_file_cache_min_uses 1; open_file_cache_errors on;
aio on;
vod_open_file_thread_pool default;
# vod caches
vod_metadata_cache metadata_cache 512m;
vod_mapping_cache mapping_cache 5m 10m;
gzip on; gzip_types application/vnd.apple.mpegurl;

location /vod/ {
    include auth_request.conf;
    aio threads;
    vod hls;
    # Use fMP4 (fragmented MP4) instead of MPEG-TS for better performance
    vod_hls_container_format fmp4;
    # fMP4 playlists use EXT-X-MAP, which requires HLS protocol version 6
    vod_hls_version 6;
    secure_token $args;
    secure_token_types application/vnd.apple.mpegurl;
    add_header Cache-Control "no-store"; expires off;
    keepalive_disable safari;
}
location /api/vod/ { include auth_request.conf; proxy_pass http://frigate_api/vod/; proxy_cache off; }
```
Mapping JSON built by `_vod_response()` in `frigate/api/media.py` (https://raw.githubusercontent.com/blakeblackshear/frigate/dev/frigate/api/media.py):
```json
{"cache": <start older than 1h>, "discontinuity": <bool>, "consistentSequenceMediaInfo": true,
 "durations": [ms, ...],
 "sequences": [{"clips": [{"type":"source","path":"/media/frigate/recordings/.../MM.SS.mp4",
                            "clipFrom": <ms>, "firstKeyFrameOffset": <ms>, "keyFrameDurations": [ms,...]}]}],
 "initialClipIndex": 1 }
```
`keyFrameDurations` come from the `Recordings.keyframes` column probed at record time; when unknown, one segment per file is emitted. `discontinuity: true` (per-clip init segments, `EXT-X-MAP` per clip) is forced whenever codec, audio parameters or stream type (main/sub) change inside the range — needed because iOS only configures the decoder from the init segment. Ranges with more clips than `NGINX_VOD_MAX_CLIPS` fail. Direct downloads (`/clip.mp4`) use ffmpeg's concat demuxer with `-c copy -movflags frag_keyframe+empty_moov -f mp4 pipe:`; docs warn "For iOS devices, use the master.m3u8 HLS link instead of clip.mp4. Safari does not reliably process progressive mp4 files."

The web player (`web/src/components/player/dynamic/DynamicVideoController.ts`, `HlsVideoPlayer.tsx`) has two modes: `playback` (hls.js on the vod playlist; `seekToTimestamp()` maps wall time → media time using the `recordings` list and an inpoint offset) and `scrubbing` (the `PreviewController` drives the hourly preview MP4 / cached preview frames), plus `AutoQualityGovernor.ts` for main/sub switching.

### 2.5 Previews and thumbnails (the timeline trick)
`frigate/output/preview.py` (https://raw.githubusercontent.com/blakeblackshear/frigate/dev/frigate/output/preview.py): from the already-decoded detect frames, write a **180 px-high WebP every 1 s (2 s⁻¹ when a car is active)** into `/tmp/cache/preview_frames/preview_<camera>-<ts>.webp`; at the top of each hour (UTC) concat them with ffmpeg into one **hourly preview MP4** (`-g 40 -bf 0 -b:v 7168–10096 (bps) [-qmax 25] -fps_mode vfr -movflags +faststart -pix_fmt yuv420p`, optional hardware encoder preset) stored under `clips/previews/` and indexed in the `Previews` table. Endpoints: `/api/preview/<camera>/start/<ts>/end/<ts>` (list of preview clips), `/api/preview/<camera>/start/<ts>/end/<ts>/frames` (cached WebP names, bisected from one sorted directory listing), `/api/review/<id>/preview`, `/api/<camera>/recordings/<frame_time>/snapshot.png` (ffmpeg seek into a segment). The review grid hovers/scrubs these tiny MP4s (≈4 MB per camera-hour *(est.)*); full-res HLS is only opened on click. This is why Frigate 0.14+ feels instant.

### 2.6 Export and Birdseye
- Export: `/media/frigate/exports`, never expired; `ffmpeg_output_args` for custom filters (time-lapse example `-vf setpts=PTS/60 -r 25`), NVENC presets `h264_nvenc`/`hevc_nvenc` for time-lapse; `cpu_fallback`. `POST /api/media/sync` reconciles DB vs disk.
- Birdseye (`frigate/output/birdseye.py`): composes the decoded detect YUV frames of active cameras into one canvas and encodes it (jsmpeg to browser, optional RTSP restream via go2rtc). It is the one place Frigate encodes video continuously.

---

## 3. go2rtc (Go) — the fan-out/negotiation hub

Repo: https://github.com/AlexxIT/go2rtc (README + `internal/*/README.md` read 2026-09-27).

### 3.1 Output codec matrix ("Codecs madness", README)
| Client | WebRTC | MSE | HTTP progressive | HLS |
|---|---|---|---|---|
| Desktop Chrome 136+ / Edge / Android Chrome 136+ | H264, **H265\***, PCMU, PCMA, OPUS | H264, **H265\***, AAC, FLAC\*, OPUS | H264, H265\*, AAC, FLAC\*, OPUS, MP3 | no |
| Desktop Firefox | H264, PCMU, PCMA, OPUS | H264, AAC, FLAC\*, OPUS | H264, AAC, FLAC\*, OPUS | no |
| Desktop Safari 14+ / iPad Safari 14+ / iPhone Safari 17.1+ | H264, H265\*, PCMU, PCMA, OPUS | H264, H265, AAC, FLAC\* | **no** | H264, H265, AAC, FLAC\* |
| iPhone Safari 14+ (pre-17.1) | H264, H265\*, PCMU, PCMA, OPUS | **no** | **no** | H264, H265, AAC, FLAC\* |

Footnotes in the README: WebRTC H265 = Chrome 136+ and Safari 18+; MSE on iPhone = iOS 17.1+; "*" = hardware-dependent. Internal modules: MSE = fMP4 over WebSocket (`/api/ws`), `/api/stream.mp4` (H264, H265, AAC; `duration`, `filename`, `rotate`/`scale` via metadata only), `/api/stream.m3u8?src=X&mp4` = HLS/fMP4 (H264, H265, AAC), `/api/frame.mp4` single-frame MP4. "MP4 over WebSocket was created only for Apple iOS because it doesn't support file streaming" (internal/api/README). HLS module: "the worst technology for real-time streaming... may not work with all players".

What "H265 via MSE works in Chrome" means in practice: go2rtc probes `MediaSource.isTypeSupported()` in the browser and only offers H265 when the OS decoder is available (Chrome ≥107 hardware-only; see §5). Firefox: H264 only for both MSE and WebRTC in the go2rtc table (even though Firefox ≥134/136/137 can play HEVC in `<video>`, go2rtc/Frigate still treat it as unsupported for MSE/WebRTC).

### 3.2 Sources, transcoding, hardware (`internal/ffmpeg/README.md`, `internal/ffmpeg/hardware/README.md`)
- `ffmpeg:{input}#video=h264|h265|mjpeg|copy#audio=opus|aac|pcmu...#hardware[=cuda|vaapi|v4l2m2m|dxva2|videotoolbox]#width=1280#rotate=90#raw=...#input=rtsp/udp`. Templates: `h264: "-codec:v libx264 -g:v 30 -preset:v superfast -tune:v zerolatency -profile:v main -level:v 4.1"`. go2rtc starts the ffmpeg child **on demand when a consumer attaches** and stops it when the last one leaves.
- Hardware: "You NEED hardware acceleration if you're using #video=h264/h265/mjpeg"; "go2rtc will enable hardware decoding only if hardware encoding is supported"; "NVIDIA will fail if the input codec isn't supported by the hardware decoder"; NVIDIA via CUDA in the `master-hardware` Docker image.
- Multi-source negotiation: a stream can list `rtsp://cam` and `ffmpeg:cam#audio=opus`; go2rtc picks the compatible tracks per client ("multi-source two-way codec negotiation").

### 3.3 `/api/frame.jpeg` cost (`internal/mjpeg/mjpeg.go`)
Handler `handlerKeyframe`: attaches a `magic.NewKeyframe()` consumer, waits for the next keyframe (`core.OnceBuffer`), detaches, then for `CodecH264/CodecH265` calls `ffmpeg.JPEGWithQuery(b, query)` — i.e. **it spawns an ffmpeg process per request** to decode that one keyframe to JPEG (`width/height/rotate/hardware` query params); only true MJPEG sources skip ffmpeg. A `cache=10s` query parameter returns the last JPEG if younger than the timeout. Cost per call = keyframe wait (up to one GOP, 1–2 s on Hikvision defaults) + process spawn + one intra decode; fine for occasional snapshots, wrong for a 23-camera thumbnail loop.

### 3.4 WebRTC for remote (Tailscale) users (`internal/webrtc/README.md`)
- Media is peer-to-peer from go2rtc to the browser; the HTTP/WS server is only for signalling. Default `webrtc.listen: ":8555"` TCP+UDP.
- `webrtc.candidates: [216.58.210.174:8555, stun:8555, home.duckdns.org:8555]` adds host candidates; `ice_servers` for STUN/TURN; `filters.candidates/ips/interfaces/networks` to constrain discovery. For Tailscale add the node's 100.x address as a candidate (Frigate docs say the same). Note "UDP is not suitable for 2K and 4K high bitrate video over open networks" and ~20% of users are behind symmetric NAT (TURN needed) — with Tailscale, the tunnel itself solves NAT, so a single `100.x.y.z:8555` candidate (TCP works too) is enough.
- Security: go2rtc trusts localhost/Unix-socket requests without auth (`local_auth: true` to change); keep it behind our API.

---

## 4. Commercial NVRs: UniFi Protect, Synology, Hikvision, Blue Iris — the common pattern

### 4.1 UniFi Protect (reverse-engineered)
- **Storage**: proprietary `.ubv` container per camera, `video/yyyy/mm/dd/<cameraMAC>_0_rotating_<startUtcMillis>.ubv` (`0_rotating` = full-quality; `2_rotating` = 1 fps 640×480; `*_timelapse`); files pre-allocated at 1.0 GB and filled sequentially. Record framing `HEADER(8B) → HEADER EXT → SIZE(4B) → PAYLOAD → PAD → BACK_SIZE(4B)` (back-size enables reverse seeking); partitions per recording session; packet types clock-sync (DTS→UNIX s + ns), video (I/P only, no B-frames), audio, skip; tracks H.264 (7), HEVC (1003), AV1 (1004), AAC ADTS (1000); flags carry keyframe bit, 32/64-bit DTS, CTS presence; 90 kHz video clock. Export/remux = pure copy ("no transcoding takes place"). https://github.com/petergeneric/unifi-protect-remux/wiki and README https://raw.githubusercontent.com/petergeneric/unifi-protect-remux/master/README.md
- **Live view**: WebSocket `/proxy/protect/api/ws/livestream` delivering **fMP4 segments** (init `ftyp+moov` + `moof/mdat` media segments, `segmentLength` ≥ 100 ms, `chunkSize` 4096, `rebaseTimestampsToZero`, optional per-frame timestamps) from the camera's **own encoded stream** ("H.264, HEVC, or AV1"), selecting a quality **channel** (0 = high, 1 = medium, 2 = low; each channel has `width/height/fps/bitrate/idrInterval`); browser plays via MSE. hjdhjd/unifi-protect docs: https://github.com/hjdhjd/unifi-protect (README), `docs/interfaces/LivestreamOptions.md`, `docs/type-aliases/Segment.md`, `docs/interfaces/ProtectCameraChannelConfigInterface.md`.
- **Events/timeline**: cameras run person/vehicle detection on-device (G4/G5/G6; AI Port/AI Key for third-party) and push motion/smart-detect events over the binary `updates` WebSocket; the NVR just indexes them. https://help.ui.com/hc/en-us/articles/360058867233
- Why it feels instant on an ARM box: the NVR never decodes (recording is a byte copy into pre-allocated files with an internal frame index; live is a copy of the camera's fMP4-able elementary stream; three parallel encodes come from the camera so grids use the 360p channel); the browser does all decoding; motion metadata comes from the camera. Note Ubiquiti controls both ends (camera firmware emits the right GOP/profile); with Hikvision we get the same effect only by configuring the cameras (I-frame interval = fps, no H.265+, CBR-ish).

### 4.2 Synology Surveillance Station, Hikvision NVR, Blue Iris
- Synology SS: records camera streams as-is (MJPEG/H.264/H.264+/H.265/H.265+ over RTSP), multi-stream profiles per camera, no server-side transcode for recording; the client (desktop app / browser plugin) decodes. https://www.synology.com/en-eu/dsm/7.3/software_spec/surveillance_station
- Hikvision NVRs: raw stream written into proprietary disk partitions; the web/iVMS client decodes; H.265+ (proprietary, drops keyframes) yields the vendor's claimed 83.7% average bitrate reduction vs H.264 (47.8% for plain H.265) — https://www.hikvision.com/en/core-technologies/storage-and-bandwidth/h-265-plus/ ; recommended bitrate document: https://www.hikvision.com/content/dam/hikvision/ca/faq-document/H.2645-&-H.2645-Recommended-Bit-Rate-at-General-Resolutions.pdf (2688×1520@20 fps H.264 ≈ 6144 kbps max; H.265+ 2688×1520@25 fps ≈ 4096 max / 2048 avg).
- Blue Iris: "direct-to-disk" writes the camera stream unmodified into its `.bvr` container with metadata (overlays, motion rectangles, status bits); "Record dual-streams if available" stores main + sub in the same BVR and **uses the sub stream for multi-camera timeline playback "to keep CPU usage from ballooning"**; BVR can be read while being written. https://ipcamtalk.com/wiki/sub-stream-guide/ , https://www.houselogix.com/docs/blue-iris/BlueIris/recording.htm
- **Common pattern**: never decode for storage; decode only the substream, at low fps, for analytics; keep an index of keyframes/time; ship the camera's bits to the client and let the client's hardware decoder work; use the substream (or a preview track) for grids and scrubbing.

---

## 5. Browser-side playback: HEVC vs H.264 in 2026

### 5.1 Support matrix (decode)
| Browser / OS | `<video>`/MSE HEVC | WebCodecs HEVC | WebRTC HEVC |
|---|---|---|---|
| Safari macOS (2017+ Intel w/ HEVC HW, all Apple Silicon), iOS/iPadOS | yes (MSE on iPhone from iOS 17.1) | Safari 16.4+ video-only, full WebCodecs 26 | Safari 18+ |
| Chrome/Edge Windows | yes since Chrome 107 (Oct 2022), **hardware only** (D3D11 decoder, no HEVC Video Extension needed for Chrome; Edge often lacks it: 55.9% of Edge/Windows sessions) | Chrome 107+ (8-bit), 108+ (10-bit) | Chrome 136+ (Mar 2025), hardware only |
| Chrome macOS | yes (VideoToolbox) — 96.7% of sessions | yes | Chrome 136+ |
| Chrome Android | yes (MediaCodec) — 81.4% | yes | Chrome 136+ |
| Chrome Linux | VAAPI, Chrome 108+, but HW video decode is off by default on Linux — 54.6% | partial | mostly no |
| Firefox | Windows 134+ (WMF, needs HEVC Video Extension), macOS 136+, Linux/Android 137+ (needs distro ffmpeg with HEVC + VA-API) — dataset still shows <2% WebCodecs HEVC | no | no |
Sources: StaZhu HEVC guide https://github.com/StaZhu/enable-chromium-hevc-hardware-decoding/blob/main/README.md ; WebCodecs Fundamentals 2026 dataset (1.14 M sessions Jan–Mar 2026: HEVC decode overall 78.9%; Chrome Win 86.0 / macOS 96.7 / Android 81.4 / Linux 54.6; Safari macOS 91.3 / iOS 85.7; Edge Win 55.9; Firefox <2%) https://webcodecsfundamentals.org/codecs/hevc.html and https://webcodecsfundamentals.org/datasets/codec-analysis-2026/ ; Firefox release notes https://www.mozilla.org/firefox/137.0/releasenotes/ (via MDN https://developer.mozilla.org/docs/Mozilla/Firefox/Releases/137), Bugzilla 1894818 (Linux) ; Chrome WebRTC HEVC intent-to-ship https://groups.google.com/a/chromium.org/g/blink-dev/c/3h8lL8a377c and chromestatus 5153479456456704 ; mediamtx note "Chrome supports publishing and reading H265 tracks only ... when a capable GPU is present" https://github.com/bluenviron/mediamtx/blob/main/docs/2-features/25-webrtc-specific-features.md.
- `MediaSource.isTypeSupported('video/mp4; codecs="hvc1.1.6.L153.B0"')` (and `hev1.*`) returns true on Chrome when the platform decoder exists; StaZhu documents a Chrome ≤108 Windows bug where it returned true although D3D11 decode was disabled (fixed in 109). Treat `isTypeSupported` as necessary, not sufficient: probe with a real 1-GOP append and fall back on `error`/stall. (The RTP payload RFC for HEVC is RFC 7798, not 9328 — 9328 is VVC.)

### 5.2 Practical recommendation
**(a) Keep the cameras on HEVC** (as today) and play the native stream through MSE (live via WebSocket fMP4, playback via time-range fMP4/HLS-fMP4), gating on `isTypeSupported`+probe; for clients that fail (Firefox in practice, Edge without the extension, Linux Chrome, old Android) fall back to (1) the **H.264 substream** for live/grid/scrub and (2) a **server-side NVENC H.264 transcode** of the main stream only when a full-res view is explicitly requested from such a client. Our primary clients (Chrome/Safari on macOS/Windows, iOS, Android) all decode HEVC in hardware; the P2200's single NVENC can serve 2–4 concurrent 4-MP fallback transcodes *(est.: Pascal NVDEC ≈ 810 fps @1080p HEVC → ~400 fps of 4 MP; NVENC is the bottleneck at a few hundred 1080p fps)* — enough for occasional guests, not for a 23-camera wall in Firefox.
**(b) Switching main streams to H.264** buys universal MSE/WebRTC (including Firefox and WebRTC without HW gating) at roughly 1.5× the bits (Hikvision: 47.8% average H.265 saving → H.264 ≈ 1.9× plain H.265; conservative planning 1.5×).

Storage impact for our fleet *(est., 24×7, before substreams/previews)*:
| Assumption | HEVC (today) | H.264 (1.5×) | H.264 (1.9×) |
|---|---|---|---|
| 22 × 4 MP @ 3.5 Mbps avg + 1 × 4K @ 8 Mbps | 22×37.8 GB + 86 GB = **0.92 TB/day** | 1.38 TB/day | 1.75 TB/day |
| Days on 17 TB (leave 10% headroom) | **~16.6 days** | ~11 days | ~8.7 days |
| 22 × 4 MP @ 2.5 Mbps + 4K @ 6 Mbps (VBR quiet scenes) | 0.66 TB/day → ~23 days | 1.0 TB/day → ~15 days | 1.26 TB/day → ~12 days |
(1 Mbps ≈ 10.8 GB/day.) Verdict: stay HEVC for the main streams; H.264 costs a third to a half of the retention window to solve a Firefox/Edge problem that the substream + a small NVENC fallback already covers. Re-evaluate only if AV1-capable cameras replace these (AV1 is universal in Chrome/Firefox and Safari 17+ on M3+, but no current Hikvision fleet emits it).

Client-side details that matter: (i) Hikvision GOP defaults (I-frame interval 40–50 = 2 s) mean MSE start-up needs up to 2 s; set I-frame interval = fps (Frigate/go2rtc advice) or keep the last GOP in RAM server-side and send it first; (ii) disable H.265+/H.264+ (missing keyframes break both MSE seeks and segmenting); (iii) B-frames are absent on Hikvision by default (WebRTC forbids them); (iv) on iOS, MSE only from 17.1 — HLS-fMP4 (`EXT-X-MAP`, version 6) is the safe playback path there and works with `hvc1`.

---

## 6. Fast timeline / thumbnail techniques

1. **Hourly low-res preview MP4 + per-second WebP frames** (Frigate, §2.5): produced from frames that are *already decoded for detection*, so cost is one WebP encode/s/camera (~1–2 ms at 320×180) plus one 180p x264/NVENC encode per camera-hour. Playback: a 4 MB/hour MP4 scrubs at any speed; 23 cameras × 24 h ≈ 2.2 GB/day *(est.)*.
2. **VTT sprite sheets** (Video.js/Plyr/Shaka `#xywh=` cues): `ffmpeg -skip_frame nokey -i seg.mp4 -vf "fps=1/10,scale=160:-1,tile=10x10" sprite.jpg` + generated WebVTT; standard for VOD scrub bars. https://dev.to/masonwritescode/build-scrub-bar-thumbnail-previews-with-ffmpeg-and-a-webvtt-sprite-3ei2 . For an NVR, generate per segment-close (one sprite per camera-hour, 360 tiles at 10 s spacing).
3. **Keyframe-only decode**: `-skip_frame nokey` makes the decoder skip P/B frames entirely; an HEVC I-frame at 2688×1520 decodes in software in roughly 15–30 ms on one Xeon Silver core *(est.; no public per-frame benchmark found — measure)*. With a 2 s GOP that is 0.5 keyframes/s/camera → 11.5 I-frames/s for 23 cameras ≈ 0.2–0.35 cores if you thumbnail every keyframe; thumbnails every 10 s cost ~0.05 cores. **NVDEC batch is not worth it here**: the P2200 has one engine (Pascal: 810 fps HEVC @1080p per NVIDIA's NVDEC application note https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvdec-application-note/index.html), it is shared with any fallback transcode, and the CUDA context/frame download overhead for sparse single frames dominates. Reserve it for transcodes; use software I-frame decode for stills.
4. **Motion heatmap**: Frigate stores a 16×16 (256-value) `motion_heatmap` per 10 s segment plus `motion` count → the timeline's density bars and per-region search come straight from SQL. Cheap to compute from the detector's contour boxes.
5. **One JPEG per detection/event** (Frigate `ReviewSegment.thumb_path` = best full frame, `Event.thumbnail` = cropped object): stored once per event, not per frame.
6. **UniFi's approach**: a 1 fps 640×480 `2_rotating` low-quality track recorded in parallel — same idea as (1) but done at the camera.

Recommended combination: (1) for scrubbing/review grid (from the detect decode), (5) for event cards, (4) for the timeline, and (2) only for the export/VOD player scrub bar of a single clip.

---

## 7. Rust ecosystem readiness (2026)

| Need | Crate(s) | Status / notes |
|---|---|---|
| RTSP client | `retina` 0.4.20 (scottlamb) | production in moonfire; TCP interleaved + experimental UDP, digest auth, H.264/H.265/MJPEG/AAC/G.711 depacketizers, ONVIF metadata; no RTSP server; API not yet stable (#47). https://github.com/scottlamb/retina . Alternative: `webrtc-rs`'s RTP stack, or shell out to go2rtc/mediamtx for ingest. |
| FFmpeg bindings | `rsmpeg` 0.18 (+ffmpeg 8.0, larksuite) — actively maintained, safe-ish wrappers; `ffmpeg-next` = "maintenance-only" (no new API); `ffmpeg-sys-next` raw FFI | Use `rsmpeg` for in-process decode (hwaccel `cuda` via `AVHWDeviceContext`) and `-skip_frame nokey`; or **spawn `ffmpeg`** for the few decode paths (substream decode → rawvideo pipe is exactly what Frigate does and is trivially robust). https://github.com/larksuite/rsmpeg , https://crates.io/crates/ffmpeg-next , https://dev.to/yeauty/the-state-of-media-processing-in-rust-2026-what-each-crate-actually-covers-2k2c |
| MP4/fMP4 (de)muxing | `mp4-atom` (ISOBMFF encode/decode, active 2026-07), `re_mp4` (Rerun's reader), `mp4e` (fMP4 + MP4 muxer, H.264/HEVC/AAC/Opus), `muxide`, `mse_fmp4`; moonfire's `server/src/mp4.rs` (GPL) is the reference for virtual-file + range | Writing our own `moof/mdat` + `moov` builder is ~1–2 kLOC on top of `mp4-atom`; `symphonia` is audio-only decoding (not relevant). https://crates.io/crates/mp4-atom , https://crates.io/crates/re_mp4 , https://crates.io/crates/mp4e |
| Motion detection | pure Rust: `imageproc` (rayon), `scirs2-vision` (frame differencing/background subtraction); `opencv` crate (bindings, needs libopencv) | Frigate's algorithm (downscale luma, blur, absdiff vs EMA, threshold, dilate, contours) is ~200 lines of pure Rust on a 640×360 luma plane; no OpenCV needed. https://github.com/image-rs/imageproc , https://lib.rs/crates/scirs2-vision |
| Object detection | `ort` 2.0.0-rc.13 (pyke) — ONNX Runtime with `cuda`/`tensorrt` features, `load-dynamic`, TensorRT engine cache | "production-ready, not API-stable". Pascal P2200 (CC 6.1) is supported by CUDA 12 EP; TensorRT 10 dropped Pascal, so use the **CUDA EP** (or ORT ≤ 1.17-era TensorRT 8.x). Expect YOLOv9-t/s-320 in the 20–50 ms range on Pascal *(est. from Frigate's Tesla P40 105 ms for YOLO-NAS-320)*. https://ort.pyke.io/perf/execution-providers |
| NVDEC/NVENC | via ffmpeg (`-hwaccel cuda`, `hevc_cuvid`, `h264_nvenc`) — no mature pure-Rust NVCodec crate | Quadro P2200 has unlimited NVENC sessions (Quadro ≥ x2000 exemption; consumer cards capped at 8) https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvenc-application-note/index.html |
| Web | `axum` + `tokio` + `tower-http` (`ServeFile`/`ServeDir` support `Range`, or hand-roll range on a virtual file), `tokio-tungstenite` for WebSocket fMP4 | mature |
| DB | `rusqlite` (bundled SQLite, WAL) or `sqlx`; moonfire schema as a template | mature |
| WebRTC | `webrtc-rs` (retina ships a `webrtc-proxy` example) — or delegate WebRTC to go2rtc/mediamtx | webrtc-rs is usable but heavier to operate than go2rtc's battle-tested ICE handling |

Go alternatives: `gortsplib` v4 (bluenviron; RTSP client/server, RTP (de)packetizers for H.264/H.265/AV1/Opus/AAC...) https://github.com/bluenviron/gortsplib ; mediamtx itself (§8) is a Go program with internal packages (`internal/recorder`, `internal/playback`) that are not published as a library API, but it can be embedded as a sidecar; go2rtc likewise (its `pkg/*` are importable Go packages but the project offers no API stability promise).

**Stack recommendation**: Rust core (`tokio`/`axum`/`rusqlite`/`mp4-atom`/`rsmpeg` or spawned `ffmpeg`/`ort`), **go2rtc or mediamtx as the ingest/fan-out sidecar** for the first iteration (RTSP pull once, RTSP loopback for the recorder, WebRTC/MSE live for browsers), replacing the sidecar with `retina` + own WebSocket-fMP4 live path once the recorder is proven. SPA: React/Preact or SvelteKit with `hls.js` + MSE.

---

## 8. mediamtx (Go) — record + playback server as a building block

Repo: https://github.com/bluenviron/mediamtx . Docs read: `docs/2-features/09-record.md`, `10-playback.md`, `20-hooks.md`, `25-webrtc-specific-features.md`, `api/playback.openapi.yaml`, `mediamtx.yml`, source `internal/playback/on_get.go`, `segment_fmp4.go`, `internal/recordcleaner/cleaner.go`, `internal/recorder/format_fmp4.go`.

### 8.1 Recording
```yml
pathDefaults:
  record: yes
  recordPath: ./recordings/%path/%Y-%m-%d_%H-%M-%S-%f   # %path %Y %m %d %H %M %S %f %z %s; extension added
  recordFormat: fmp4          # or mpegts
  recordPartDuration: 1s      # fMP4 = concatenation of small moof/mdat "parts"; last part lost on crash → RPO
  recordMaxPartSize: 50M
  recordSegmentDuration: 1h   # minimum segment length; switches on the next random-access (key) frame
  recordDeleteAfter: 1d       # 0s disables; age-based only
```
fMP4 container accepts AV1, VP9, **H265, H264**, MPEG-4/1/2, M-JPEG video and Opus/AAC/MP3/AC-3/G.711/LPCM audio. Segment files are written incrementally part by part (a part = one `moof`+`mdat`, `IsNonSyncSample` flagged from the codec's random-access detection), each segment carries an `mtxi` user-data box with a stream ID and segment number. Hooks `runOnRecordSegmentCreate` / `runOnRecordSegmentComplete` (env `MTX_PATH`, `MTX_SEGMENT_PATH`) let an indexer react to every closed segment. `alwaysAvailableRecorded` keeps recording an offline placeholder.

### 8.2 Playback server (`playback: yes`, `playbackAddress: :9996`)
- `GET /list?path=<name>&start=<RFC3339>&end=<RFC3339>` → `[{"start": "...", "duration": 60.0, "url": ".../get?path=...&start=...&duration=60.0"}]` — timespans, merged across contiguous segments.
- `GET /get?path=<name>&start=<RFC3339>&duration=<seconds>&format=fmp4|mp4` → one `video/mp4` body **spliced across segment files** (`seekAndMux`: read the first segment's init, seek inside the parts to `start` (the code computes `startOffset` and starts on the first sync sample), then keeps appending following segments while `segmentFMP4CanBeConcatenated` (same `mtxi` stream ID and consecutive segment number, or legacy: equal tracks and start within a tolerance of the previous end)). `format=mp4` re-muxes into a non-fragmented MP4 for picky players. **`Accept-Ranges: none`** — no byte-range/seeking inside the response (OpenAPI: "The playback server does not support byte-range requests"); MPEG-TS playback is "not supported yet".
- Permissions: `playback` is an auth action (internal users, HTTP, or JWT auth) scoped by path — usable for per-camera guest permissions.
- The response is meant to be dropped into `<video src>` directly; that works for Safari/Chrome for a bounded duration but the lack of ranges and of an index means: no fast seek inside a long range, no scrub bar without a second index, and whole-duration downloads for exports only.

### 8.3 Outputs and design
- WebRTC (WHEP; AV1, VP9, VP8, H265, H264 video; Opus, G.722, G.711 audio; static UDP port `webrtcLocalUDPAddress :8189`, `webrtcAdditionalHosts` for extra ICE hosts → add the Tailscale IP), HLS (AV1/VP9/H265/H264; LL-HLS variants; fMP4), RTSP/RTMP/SRT/MoQ. Browser caveats in docs: H265 over WebRTC only where the browser has a hardware decoder (they cite Chrome on Windows with a capable GPU); H264 with B-frames is rejected by browsers.
- No hardware acceleration and no transcoding inside mediamtx (re-encode = run ffmpeg via `runOnAvailable`); CPU profile shows the hot path is UDP recv + RTP write (pprof, docs/23-performance.md). Snapshots are "run ffmpeg in a hook every N seconds" (docs/13-extract-snapshots.md) — i.e. nothing built in.

**Assessment**: mediamtx is a strong *ingest + record + live* block (one RTSP pull, fMP4 recording with 1-s parts, WebRTC/HLS out, hooks per segment, JWT/per-path auth). Its playback endpoint is a good *export/download* block but not a timeline player: no ranges, no keyframe index, age-only retention (no size-based, no per-event retention), no motion. If used, we would (a) own the index (parse each closed segment's `moof` boxes once, store keyframe offsets/times in SQLite), (b) implement our own range-capable fMP4 packager over its files, and (c) implement retention ourselves (delete files + let mediamtx's cleaner stay disabled with `recordDeleteAfter: 0s`).

---

## 9. Recommended reference architecture

### 9.1 Per-camera pipeline
```
Hikvision cam ──RTSP main (HEVC)──┐
                                  ├─ INGEST (one pull per stream; retina in-process, or go2rtc/mediamtx sidecar exposing rtsp://127.0.0.1:8554/<cam>)
Hikvision cam ──RTSP sub (H.264)──┘
        │ main (Annex-B → length-prefixed NALs, 90 kHz media ts + wall ts per frame)
        ├─► SEGMENT WRITER (passthrough): appends samples to the open segment file; rotates on the first IDR after 60 s
        │      writes per-frame index (duration, size, keyframe) → SQLite `recording`/`recording_playback` (moonfire model)
        │      fsync file+dir, then commit row; sample-file dir per volume (RAID = long retention, 11 TB disk = bulk)
        ├─► LIVE FAN-OUT: last GOP kept in RAM; WebSocket fMP4 (MSE) and/or WebRTC via sidecar
        └─► (on demand) NVENC FALLBACK TRANSCODE (hevc_cuvid → h264_nvenc, 1 session per viewer, idle-stop)
        │ sub
        ├─► DECODE @ 5 fps (ffmpeg -skip to yuv420p pipe or rsmpeg) → 640×360 luma
        │      ├─► MOTION (downscale → blur → absdiff vs EMA → threshold → contours; masks; 16×16 heatmap)
        │      ├─► (motion-gated) OBJECT DETECTION (ort CUDA EP, YOLOv9-t 320, batched across cameras; ≤ ~30 inferences/s on P2200)
        │      ├─► EVENT MAINTAINER: opens/extends/closes "review items" (Frigate semantics: one item per camera per activity burst; alert/detection severity; zones)
        │      └─► PREVIEW: 180p WebP every 1 s → hourly preview MP4 (x264 ultrafast or NVENC) + event thumbnail JPEG (best frame)
        └─► SUB-STREAM RECORDER (optional, Blue Iris style): same passthrough writer, 640×360 H.264 (~0.5 Mbps → ~5 GB/day/camera) for grid/scrub/mobile
```
- **Playback**: `GET /api/cameras/{id}/video?start=<wall_ts>&end=<wall_ts>&stream=main|sub` → virtual MP4 (unfragmented, `moov` first, edit list for sub-keyframe start, **etag + HTTP Range**) synthesized from the index exactly like moonfire's `view.mp4`; `.../segments.m4s?...` and `/init/{vse}.mp4` for MSE; `.../index.m3u8` (HLS-fMP4 v6, `EXT-X-MAP`, keyframe-aligned 2–10 s parts with a 1/2/4 s bootstrap ladder like Frigate's nginx-vod config) for iOS/Safari. All three are the same index-driven packager; zero decode.
- **Live**: MSE fMP4 over WebSocket from the ring buffer (start with the cached IDR so first frame ≤ 100 ms), one WebSocket per tile (Chrome allows ~256); WebRTC (via go2rtc/mediamtx) for lowest latency / two-way audio; grid tiles use the H.264 substream, single-camera view uses main HEVC if `isTypeSupported`+probe passes, else substream, else NVENC H.264 fallback.
- **Scrubbing/timeline**: SQL query over `recording` (per-minute coverage), `event` intervals, and per-segment `motion_count`/heatmap → timeline bars; hover/scrub plays the hourly preview MP4; click → full-res packager at that wall time.
- **Export**: same packager with `Content-Disposition` (unfragmented MP4, `ts=true` burnt-in timestamp subtitle track optional); long exports streamed, no temp files; optional NVENC re-encode only for time-lapse.
- **Retention**: per-volume byte budget (moonfire) + per-camera policy: continuous N days, then keep only segments overlapping events/motion for M days (Frigate modes `all|motion|active_objects`), emergency oldest-first delete below a free-space floor; `garbage` table protocol for crash safety; deletion at segment (60 s) granularity.

### 9.2 Data model (SQLite, WAL, on the SSD)
- `camera(id, uuid, name, config json)`; `stream(id, camera_id, kind main|sub, dir_id, codec vse_id, cum_recordings, cum_runs)`.
- `recording(composite_id = stream_id<<32|n, open_id, run_offset, flags, start_wall_90k, wall_dur_90k, media_dur_delta_90k, bytes, samples, sync_samples, vse_id, end_reason)` + covering index on `(stream_id, start_wall_90k, …)`; `recording_playback(composite_id, video_index blob)` (moonfire varint encoding); `video_sample_entry(id, codec rfc6381, w, h, hvcC/avcC blob)`.
- `segment_stats(composite_id, motion_count, active_objects, heatmap blob16x16, dbfs)` — the retention/timeline input (Frigate).
- `event(id, camera_id, start_90k, end_90k, severity, labels json, zones json, thumb_path, best_frame_path, data json)`; `event_object(event_id, track_id, label, score, box_path)`; `preview(camera_id, hour_start, path, duration)`.
- `signal/signal_change` (moonfire) for external inputs (alarm arm state, HA binary sensors) drawn on the timeline.
- `user(id, username, pw_hash, role)`, `session(...)`, `camera_group(id, name)`, `camera_group_member`, `grant(user_id, camera_group_id, perms bitmask: live, playback, export, config)` — guests get a group; every media URL is authorised per camera (auth_request-style middleware in axum; no unauthenticated media paths).
- `volume(id, path, budget_bytes, uuid, last_open_id)` with `garbage(volume_id, composite_id)`.

### 9.3 Process model
- **One supervisor process** (Rust, tokio multi-thread runtime) owning SQLite, the index, the HTTP/WS API, retention, and one lightweight task set per camera (RTSP reader task, writer task, live fan-out). This is what moonfire, mediamtx and go2rtc all do (a single process with per-stream goroutines/tasks); it keeps the shared index in memory and avoids IPC for the hot path. Per-camera crash isolation is handled by task restart with backoff and by the open-id protocol, not by OS processes.
- **Child processes only for decoders/encoders**: one `ffmpeg` per substream decode (Frigate's model: cheap to supervise, cheap to restart, immune to codec-library crashes), one `ffmpeg` per active NVENC fallback session, one for preview hourly encodes. If retina is not used for ingest, one go2rtc/mediamtx sidecar for RTSP pull + WebRTC.
- **Detection worker**: one process/thread pool owning the CUDA context (ort session), fed via a bounded channel (frames + camera id), batching across cameras; keeps the GPU context out of the supervisor so a driver fault cannot take down recording.
- Expected steady-state load *(est.)*: writer path ≈ 23 × 4 Mbps = 12 MB/s sequential (negligible CPU); substream decode 23 × 5 fps × 640×360 ≈ 0.5–1 core; motion ≈ 0.2 core; previews ≈ 0.5 core; detection on GPU; the 48 threads are mostly idle — headroom for software HEVC I-frame thumbnails and for a few software transcodes if the single NVENC is busy.

### 9.4 GPU usage (Quadro P2200, Pascal)
- **Detection**: ONNX Runtime CUDA EP (TensorRT 10 dropped Pascal; CUDA 12 still supports CC 6.1). Model: YOLOv9-t/s at 320 (Frigate default class of model) — plan on ~20–50 ms/inference *(est.)*, motion-gated so 23 cameras rarely exceed ~10–20 inferences/s. 5 GB VRAM is ample.
- **NVDEC**: reserved for the fallback transcode input (hevc_cuvid) and optional 4K thumbnailing; not for detection (substream is H.264 640×360 — software decode is cheaper than PCIe round-trips).
- **NVENC**: on-demand H.264 fallback for non-HEVC clients (unlimited sessions on Quadro; practical ceiling a few 4-MP@20 fps sessions on one Pascal engine *(est.)*), time-lapse exports, and (optionally) the hourly 180p preview encodes.
- Do **not** plan on NVDEC/NVENC for anything continuous per camera — one engine each.

### 9.5 Comparison of candidates
| | ZoneMinder today | Frigate 0.16/dev | moonfire-nvr | mediamtx | UniFi Protect |
|---|---|---|---|---|---|
| Ingest | ffmpeg per monitor (zmc), decodes everything | go2rtc pull once → ffmpeg `-c copy` record + ffmpeg decode detect | retina, one RTSP session per stream, no decode | gortsplib, one pull, fan-out | camera → NVR proprietary, camera-side encode ladder |
| Storage unit | per-event MP4 or JPEG frames; decode+re-encode common | 10 s MP4 `-c copy` in cache → date/hour/camera dirs | 60 s raw sample file + varint frame index in SQLite | fMP4 segments (1 s parts, 1 h default) | 1 GB `.ubv` per camera with internal frame index |
| Index granularity | per event | per 10 s segment + keyframe ms list + motion/object counts + 16×16 heatmap | per frame (2 B/frame) | per part/segment (`moof` scan) | per frame (record header) |
| Playback | zms transcodes/streams MJPEG or MP4 | nginx-vod HLS-fMP4 mapped from segment files, keyframe-aligned | virtual MP4/M4S with edit lists, etag, Range | `/get?start&duration` spliced fMP4, no Range | WebSocket fMP4 to MSE |
| Live | MJPEG/HLS/WebRTC via go2rtc (recent) | MSE/WebRTC (go2rtc), jsmpeg fallback | WebSocket fMP4 (experimental) | WebRTC/HLS/RTSP/SRT | WebSocket fMP4 per quality channel |
| HEVC | record yes; browser playback via transcode | record yes; live/playback where browser supports | since v0.7.19 (2025) | yes (record, WebRTC, HLS) | yes (camera-native) |
| Motion / objects | zma per-monitor pixel analysis on decoded main stream | substream 5 fps OpenCV motion + Coral/OpenVINO/ONNX-TensorRT objects, review items | none (signals draft) | none | on camera |
| Timeline thumbnails | montage/event thumbs | hourly 180p preview MP4 + 1 s WebP frames | none | none | camera low-res track |
| Retention | per-monitor filters/purge | days per mode + event overlap + emergency free-space | bytes per stream, oldest-first, crash-safe garbage table | age only | age/size (opaque) |
| CPU profile (23 cams) | decode of all main streams ⇒ many cores | ~1 core substream decode + encoder for birdseye/previews | ~0 (RPi2 handles 6×1080p) | ~0 | n/a |
| Permissions | users, per-monitor | roles → camera lists | per-user/session permissions | per-path auth actions incl. `playback` | users/groups |
| What we take | nothing structural | segment stats, retention modes, review items, previews, nginx-vod style mapping, detector layering | schema, index encoding, virtual-MP4 packager with Range + edit lists, time model, crash protocol, WS live | optional ingest/live sidecar, fMP4 part/segment writer semantics, hooks | client-side decode + quality ladder + camera-side events as the design ideal |

### 9.6 Build order (so each step is usable)
1. Rust recorder: retina (or RTSP loopback from go2rtc) → 60 s raw segments + SQLite index (moonfire schema), volume budgets, garbage protocol; `view.mp4` packager with Range; CLI export. Validate zero-decode CPU on all 23 cameras.
2. Substream pipeline: ffmpeg → yuv pipe → motion (pure Rust) → `segment_stats`, previews (WebP/hourly MP4), event maintainer; timeline API.
3. SPA: live grid (substream MSE via go2rtc), single-camera HEVC MSE with probe + fallbacks, timeline with heatmap bars + preview scrubbing, click-to-play via packager (MSE on desktop/Android, HLS-fMP4 on iOS).
4. Objects: ort CUDA EP YOLOv9 on motion-gated substream crops; event labels/thumbnails; HA-friendly REST/MQTT events.
5. NVENC fallback sessions, per-camera-group guest permissions, retention by event.

---

## Appendix A — moonfire-nvr API excerpts (verbatim from ref/api.md)

Recording list request/response:
```
/api/cameras/fd20f7a2-9d69-4cb3-94ed-d51a20c3edfe/main/recordings?startTime90k=130888729442361&endTime90k=130985466591817
```
```json
{
  "recordings": [
    {"startId": 1, "startTime90k": 130985461191810, "endTime90k": 130985466591817,
     "sampleFileBytes": 8405564, "videoSampleEntryId": 1},
    {"endTime90k": 130985461191810, "...": "..."}
  ],
  "videoSampleEntries": {"1": {"width": 1280, "height": 720}}
}
```
Virtual MP4 selectors (`s=` may repeat; segments are concatenated):
```
/view.mp4?s=1                 # all of recording 1
/view.mp4?s=1-5&ts=true       # recordings 1..5 with a timestamp subtitle track
/view.mp4?s=1.26-             # recording 1, skipping its first 26/90000 s (edit list, no re-encode)
/view.mp4?s=5680@42.5220058-5400061   # recording 5680 of open 42, wall-time window inside it
```
Live WebSocket message (headers + fMP4 media segment):
```
Content-Type: video/mp4; codecs="avc1.640028"
X-Recording-Id: 42.5680
X-Recording-Start: 130985461191810
X-Prev-Media-Duration: 10000000
X-Media-Time-Range: 5220058-5400061
X-Video-Sample-Entry-Id: 4

<binary mp4 data>
```
followed by `GET /api/init/4.mp4` for the initialization segment (`X-Aspect: 16:9`). Text control messages: `{"type":"drop","frames":5}`, `{"type":"error","message":"..."}`; ping every 30 s.

`GET /api/?days=true&cameraConfigs=true` returns per stream `retainBytes`, `minStartTime90k`, `maxEndTime90k`, `totalDuration90k`, plus per-day totals used by the UI's calendar (a cheap "coverage" query we should replicate for the timeline overview).

## Appendix B — Frigate HTTP surface relevant to a timeline UI (frigate/api/media.py, preview.py, record.py)

| Endpoint | Purpose |
|---|---|
| `GET /api/vod/{camera}/start/{ts}/end/{ts}/master.m3u8` | HLS-fMP4 of a wall-time range, packaged by nginx-vod from the mapping JSON |
| `GET /api/vod/{camera}/{main|sub}/start/{ts}/end/{ts}/...` | same, pinned to one stream type (quality selector) |
| `GET /api/vod/event/{event_id}?padding=N` | HLS for one tracked object ± padding |
| `GET /api/vod/clip/{camera}/start/{ts}/end/{ts}/...` | forced `discontinuity` manifest (per-clip init) |
| `GET /api/{camera}/start/{ts}/end/{ts}/clip.mp4` | download via ffmpeg concat `-c copy -movflags frag_keyframe+empty_moov` |
| `GET /api/{camera}/recordings/{frame_time}/snapshot.png` | one frame from a segment (ffmpeg seek) |
| `GET /api/preview/{camera}/start/{ts}/end/{ts}` | list of hourly preview MP4s overlapping the range (`{camera, src, type, start, end}`) |
| `GET /api/preview/{camera}/start/{ts}/end/{ts}/frames` | cached 1 s WebP preview frame names (bisect over a sorted, mtime-cached listing) |
| `GET /api/{camera}/recordings/summary`, `.../recordings?after&before` | per-hour coverage + segment rows (`motion`, `objects`, `duration`) feeding the timeline |
| `GET /api/review?...`, `/api/review/{id}/preview` | review items and their preview clip/GIF |
| `GET /api/events/{id}/thumbnail.jpg`, `/snapshot.jpg`, `/clip.mp4` | per-object media |
| `POST /api/export/{camera}/start/{ts}/end/{ts}` | export job (custom ffmpeg args allowed for admins; `cpu_fallback`) |
| `POST /api/media/sync` | reconcile DB vs disk (aborts if >50% would be deleted unless `force`) |
All media routes go through `require_camera_access` / `get_allowed_cameras_for_filter` (role → camera list), which is the same shape as our per-camera-group guest requirement.

Frigate `User.get_allowed_cameras(role, roles_dict, all_camera_names)`: "Empty list means all cameras"; roles map to camera lists in config.

## Appendix C — mediamtx playback JSON and concatenation rule (verbatim)

`/list` response:
```json
[
  {"start": "2006-01-02T15:04:05Z07:00", "duration": 60.0,
   "url": "http://localhost:9996/get?path=[mypath]&start=2006-01-02T15%3A04%3A05Z07%3A00&duration=60.0"},
  {"start": "2006-01-02T15:07:05Z07:00", "duration": 32.33,
   "url": "http://localhost:9996/get?path=[mypath]&start=2006-01-02T15%3A07%3A05Z07%3A00&duration=32.33"}
]
```
`internal/playback/segment_fmp4.go`:
```go
func segmentFMP4CanBeConcatenated(prevInit *fmp4.Init, prevEnd time.Time, curInit *fmp4.Init, curStart time.Time) bool {
    mtxi1 := findMtxi(prevInit.UserData); mtxi2 := findMtxi(curInit.UserData)
    switch {
    case mtxi1 == nil && mtxi2 == nil: // legacy method
        return segmentFMP4TracksAreEqual(prevInit.Tracks, curInit.Tracks) &&
            !curStart.Before(prevEnd.Add(-concatenationTolerance)) &&
            !curStart.After(prevEnd.Add(concatenationTolerance))
    default:
        return bytes.Equal(mtxi1.StreamID[:], mtxi2.StreamID[:]) && (mtxi1.SegmentNumber+1) == mtxi2.SegmentNumber
    }
}
```
`internal/playback/on_get.go` sets `Accept-Ranges: none` and `Content-Type: video/mp4`, finds segments with `recordstore.FindSegments(pathConf, pathName, &start, &end)`, then `seekAndMux(...)`; `internal/recordcleaner/cleaner.go` runs every `min(30 min, recordDeleteAfter/2)` and only deletes by age (`now - RecordDeleteAfter`). The `mtxi` box (stream ID + segment number) is the mechanism that makes a run of segments provably contiguous — worth copying into our own segment headers.

## Appendix D — client capability probe and fallback order (for the SPA)

```js
// 1. Cheap capability check (necessary, not sufficient)
const hevcMse = window.MediaSource &&
  (MediaSource.isTypeSupported('video/mp4; codecs="hvc1.1.6.L153.B0"') ||
   MediaSource.isTypeSupported('video/mp4; codecs="hev1.1.6.L153.B0"'));
const nativeHls = document.createElement('video').canPlayType('application/vnd.apple.mpegurl') !== '';

// 2. Real probe: append the camera's init segment + one GOP to a SourceBuffer and wait for
//    'updateend' without 'error' and with video.readyState >= 2 within ~1.5 s.
//    (Chrome <=108/Windows reported support while D3D11 decode was disabled: StaZhu README.)

// 3. Fallback order per tile:
//    single view : [main HEVC via MSE] -> [main HEVC via HLS-fMP4 (iOS < 17.1 / Safari)] ->
//                  [sub H.264 via MSE/WebRTC] -> [server NVENC H.264 of main via MSE]
//    grid tiles  : [sub H.264 via MSE (or WebRTC for <500 ms)] always; upgrade to main only on click.
```
Codec strings: `hvc1.1.6.L153.B0` = HEVC Main, level 5.1 (fits 3840×2160@20); `hvc1.1.6.L123.B0` = level 4.1 (2688×1520@20). Prefer `hvc1` (parameter sets in `hvcC`) over `hev1` for MSE/HLS on Apple platforms (Frigate's `force_record_hvc1` exists for this reason). WebRTC HEVC requires Chrome ≥136 / Safari ≥18 and hardware decode on the client; treat it as opportunistic.

## Appendix E — capacity arithmetic (inputs and formulas)

- 1 Mbps sustained = 0.125 MB/s = 450 MB/h = **10.8 GB/day**.
- Hikvision guidance (vendor PDF, §4.2): 2688×1520@20 H.264 ≈ 6.1 Mbps max; H.265+ ≈ 4.1 Mbps max / 2.0 Mbps avg; vendor-stated H.265 saving 47.8% and H.265+ 83.7% vs H.264 (https://www.hikvision.com/en/core-technologies/storage-and-bandwidth/h-265-plus/). Planning values used above: HEVC 4 MP = 3.5 Mbps (busy) / 2.5 Mbps (quiet), 4K HEVC = 8 / 6 Mbps; H.264 = 1.5× (conservative) or 1.9× (vendor).
- Main streams, HEVC, busy: 22 × 3.5 × 10.8 GB = 831 GB + 4K 86 GB = **917 GB/day**; 17 TB × 0.9 / 0.917 ≈ **16.7 days**.
- Add sub-stream recording (23 × 0.5 Mbps = 124 GB/day) and previews (~2.2 GB/day) → ~1.04 TB/day → ~14.7 days. Sub-stream recording is optional; it buys Blue-Iris-style cheap multi-camera timeline playback on weak clients and is retention-independent (keep 3 days, for example → 370 GB).
- Index: ~2 bytes/frame → 23 cams × 20 fps × 86 400 s × 2 B ≈ **80 MB/day** of `video_index` (moonfire encoding), i.e. ~1.3 GB for 16 days; keep SQLite on the SSD.
- Write pattern: ~11–13 MB/s aggregate sequential, one open file per stream, fsync per 60 s segment → negligible for RAID or a single 11 TB disk; the 11 TB single disk is the natural "bulk continuous" volume and the 6.3 TB RAID the "events/long-retention" volume (per-volume budgets in the `volume` table).
- Substream decode: 23 × 640×360 × 5 fps = 115 fps of H.264 ≈ 0.3–0.6 core on Skylake-SP *(est.)*; at full 20 fps still ≈ 1–2 cores. Motion on 23 × 5 fps luma at ~160×90 ≈ negligible. YOLO on GPU only after motion.
- NVENC fallback: each session = 1 NVDEC decode (HEVC 4 MP @ 20 fps ≈ 2.4 × 1080p equivalent → ~50 of the 810 fps budget) + 1 NVENC encode; realistic simultaneous sessions on one Pascal engine: 2–4 at 4 MP *(est.)*. Mark it as a scarce resource with per-user limits.

## Appendix F — source files and documents inspected

moonfire-nvr: `README.md`, `design/schema.md`, `design/glossary.md`, `design/time.md`, `design/signal.md`, `ref/api.md`, `ref/config.md`, `CHANGELOG.md`, `server/db/schema.sql`, `server/src/mp4.rs` (header/box layout, ftyp brands). retina: `README.md`.
Frigate (dev): `docs/docs/configuration/{record,review,motion_detection,live,restream,hardware_acceleration_video,object_detectors}.md`, `docs/docs/frigate/hardware.md`, `frigate/const.py`, `frigate/models.py`, `frigate/record/{maintainer,cleanup}.py`, `frigate/review/maintainer.py`, `frigate/output/preview.py`, `frigate/motion/improved_motion.py`, `frigate/ffmpeg_presets.py`, `frigate/config/camera/ffmpeg.py`, `frigate/util/media.py` (`get_keyframe_offsets`), `frigate/api/{media,preview}.py`, `web/src/components/player/dynamic/DynamicVideoController.ts`, `docker/main/rootfs/usr/local/nginx/conf/nginx.conf`.
go2rtc: `README.md` (codec tables), `internal/{mp4,webrtc,ffmpeg,hls,api,mjpeg}/README.md`, `internal/ffmpeg/hardware/README.md`, `internal/mjpeg/mjpeg.go`.
mediamtx: `docs/2-features/{02-architecture,07-remuxing,09-record,10-playback,13-extract-snapshots,20-hooks,23-performance,25-webrtc-specific-features}.md`, `docs/4-read/{03-webrtc,06-hls,07-web-browsers}.md`, `api/playback.openapi.yaml`, `mediamtx.yml`, `internal/playback/{on_get,segment_fmp4}.go`, `internal/recordcleaner/cleaner.go`, `internal/recorder/format_fmp4.go`.
UniFi Protect: hjdhjd/unifi-protect `README.md`, `docs/interfaces/LivestreamOptions.md`, `docs/type-aliases/Segment.md`, `docs/interfaces/ProtectCameraChannelConfigInterface.md`; petergeneric/unifi-protect-remux `README.md` + wiki (UBV format).
Browser codecs: StaZhu/enable-chromium-hevc-hardware-decoding README; webcodecsfundamentals.org HEVC page and 2026 dataset; Mozilla release notes 134/136/137 (via MDN/Phoronix/Neowin); Chromium blink-dev "Intent to Ship: H265 in WebRTC"; NVIDIA NVDEC/NVENC application notes (Video Codec SDK 13.0); Hikvision H.265+ and recommended-bitrate documents; ipcamtalk Blue Iris sub-stream guide; Synology Surveillance Station spec.
