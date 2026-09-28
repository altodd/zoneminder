# zmng test coverage review (after phase 3)

Source: fresh copy of `/home/user/zoneminder/zmng` (HEAD 209e318) at scratchpad `zmng-cov2/`; `cargo llvm-cov --locked --all-targets` (85 tests: 34 unit + 51 integration, all pass, ~25 s). Per-line dump `zmng-cov2/cov.txt`, condensed uncovered ranges `zmng-cov2/uncov.txt`. Line numbers refer to the copy (identical to the repo at copy time).

## 1. Per-file line coverage

| File | Lines | Missed | Line % | Fn % | Previous (line/fn) |
|---|---:|---:|---:|---:|---|
| preview.rs | 287 | 3 | 99.0 | 100 | new |
| objects.rs | 219 | 3 | 98.6 | 100 | new |
| peers.rs | 54 | 1 | 98.1 | 85.7 | new |
| thumbs.rs | 42 | 1 | 97.6 | 100 | 97.6 / 100 |
| config.rs | 32 | 1 | 96.9 | 66.7 | 73.1 / 33.3 |
| onvif.rs | 243 | 9 | 96.3 | 89.4 | new |
| mp4.rs | 947 | 56 | 94.1 | 91.7 | 91.7 / 87.2 |
| transcode.rs | 183 | 17 | 90.7 | 85.7 | new |
| notify.rs | 262 | 31 | 88.2 | 93.5 | 86.6 / 90.6 |
| health.rs | 383 | 52 | 86.4 | 69.2 | 85.2 / 71.4 |
| video.rs | 182 | 26 | 85.7 | 100 | 75.5 / 88.2 |
| db.rs | 1010 | 146 | 85.5 | 82.9 | 80.4 / 78.5 |
| retention.rs | 193 | 28 | 85.5 | 73.7 | 71.2 / 55.6 |
| tier.rs | 122 | 20 | 83.6 | 80.0 | 83.6 / 80.0 |
| api.rs | 1054 | 279 | 73.5 | 61.1 | 62.1 / 51.7 |
| detect.rs | 591 | 158 | 73.3 | 73.7 | 56.4 / 64.6 |
| zmapi.rs | 896 | 379 | 57.7 | 41.4 | 49.8 / 32.9 |
| recorder.rs | 528 | 512 | 3.0 | 9.3 | 3.1 / 10.0 |
| import.rs | 149 | 149 | 0.0 | 0.0 | 0 / 0 |
| main.rs | 262 | 262 | 0.0 | 0.0 | 0 / 0 |
| **TOTAL** | 7639 | 2133 | **72.1** | **67.1** | **61.0 / 56.1** |

Regions 69.5 %. Of the 2133 missed lines, 923 (43 %) are recorder.rs + import.rs + main.rs. Excluding those three, the crate is at 82 % lines.

Fixed since the last audit (now pinned by tests): setup-mode privilege escalation (`setup_mode_cannot_be_used_to_create_users_without_the_token`), zm `event_delete` admin-only, schema upgrade (`phase1_database_upgrades_in_place`), H265/hvc1 (`hevc_segments_serve_with_hvc1_codec_string`), thumbnail task, webhook, MQTT broker outage, `Config::load`, `live_mp4` (via `live_stream_is_transcoded`, which builds a fake `CamHandle`).

## 2. Untested paths by file

### recorder.rs (3 %) - needs an RTSP source, see section 4
Unchanged from the previous audit: everything from `reconcile` L181 down.
- `MotionLog::push/max_in/latest` L63-77, `StreamKind::parse` L95-101, `LiveHub::all_handles` L146, `remove_camera` L152 (0 %; the detector e2e test never has a recorder handle, so `DetectRun::on_frame` L444 `hh2.motion.push` is also dead).
- `reconcile` L181-256: stop removed/disabled, restart on `stream_url`/`storage_id` change vs in-place update L206-215, substream only when `record_sub && sub_url starts_with("rtsp")` L191, panic supervisor L240-253.
- `run_camera` L266-311: missing storage -> 30 s sleep L275-278, status reset L285-291, backoff 1->30 s and reset after 60 s L301-310.
- `record_session` L336-614: DESCRIBE/SETUP/PLAY errors, "no h264/h265 video stream", audio SETUP fallback L371-378, 20 s packet timeout L419, mid-stream parameter change flush+close L488-495, wall-clock anchor + re-anchor after 30 s drift + `force_new_segment` L530-556, strict monotonic dts L558-561, GOP->fragment + rollover at `segment_secs` L570-582, drop pre-keyframe frames L584-587, 5 s stats L594-604, end-of-session flush L612-631.
- `open_segment` L617 (`-N.mp4` collision suffix), `flush_gop` L661, `close_segment` L706 (motion recompute, fsync, empty file unlink, `insert_segment_ext`).

### zmapi.rs (58 %)
- `local_tz_name` L55-74 (0 %), `get_version` L99, `get_timezone`/`daemon_check`/`get_load`/`get_disk_percent`/`config_by_name`/`empty_list`/`users`/`storage_list`/`not_found` L146-210 (0 %), `monitor_alarm`/`monitor_daemon`/`zones` L261-269 (0 %).
- `login` L112 refresh-token branch, L115 429, L121-124 bad-password branch (dummy hash + guard + 500 ms sleep): zmNinja login with a wrong password is never tested.
- `parse_filter` L310-311 (`Notes REGEXP`), `event_json` L324-325 (`"zm" -> "ZoneMinder"` kind, only produced by import.rs).
- `event_one` L388-398 (0 %), `event_put` viewer 404 L405/409/412, `console_events` L431-442 (0 %), `notifications_get/post/put/delete` L448-475 (0 %) and `db.update_push_token` L1198 / `delete_push_token` L1208.
- `index_php` L487-506 `view=image` (numeric `fid` -> `frame_jpeg_pub`, thumb fast path width<=640, viewer 404), L512, L518-530 `view=view_event_hls`, L535-543 `view=request` + unsupported view. Only `view=view_video` is covered.
- `zm_control` L575 "monitor is not controllable".
- `serve_event_mp4` L611 no recording -> 404, L618-622 `Accept-Ranges: none` when `!exact_len`, L627/631/645 Range parsing, 416, `Content-Range`.
- `nph_zms` L701-777 (0 %): `source=event` 404, hidden monitor 404, `mode=single` -> `frame_jpeg_pub`, MJPEG transcode from the sub/main handle, 503 when no recorder, `decode_sem` permit lifetime.
- `es_session`: BADAUTH L851, NOAUTH L864, Ping/Close L879-881, `("push", _)` L897, Lagged/closed L907-908.

### api.rs (74 %)
- `frame_jpeg_pub` L92-118 (0 %): "recent" branch (dts within 5 s of now reads the hub, 503 without a handle) vs on-disk branch, "no recording at that time" 404, `rebase_fragment` with `dts_offset`. The native `frame_at` L811 is covered; `frame_jpeg_pub` is the zm `view=image` / `nph_zms mode=single` path and is never reached.
- `token_from` cookie parsing L140-144: cookie auth is set by `login` but never used by a test; `logout` L261-268 (0 %).
- `login` L236: 429 after `MAX_FAILS` never reached (test `login_sets_cookie_and_throttles_failures` does one failure; the name promises more).
- `camera_create` L375-383 (0 %; storage default when `storage_id` omitted, "no storage configured" 400), `camera_delete` L396-406 (0 %): `update_camera` on a nonexistent id is a no-op UPDATE (db.rs L650) so `DELETE /api/cameras/999` returns 200 `{"ok":true}` instead of 404.
- `event_thumb` L571-584 (0 %): thumb_path None -> 404, missing file -> 404, immutable cache header.
- `video_range` L640 (>6 h -> 400), `video_hls` L671 (>24 h -> 400) and legacy URL closures L681-682, `segment_file` L702 (416).
- `segment_init` L723-730, `segment_frag` L733-749 (0 %): the legacy `dts_offset != 0` HLS path (imported ZM events), `fi` out of range -> 404.
- `snapshot` L793-802 (0 %): 503 when not in hub / no frames; `live` L765-789 is covered only through the transcode test.
- `ptz_command` L920 auto-stop error, L933 unknown action -> 400.
- `create_token` L996-1004, `users` L1008-1013, `user_create` L1026-1038 (short password, bad role, duplicate username), `user_update` L1046-1052, `user_delete` L1054-1061 (self-delete 400), `storage_update` L1075-1079, `storage_create` L1088-1098 (non-dir 400), `stats` L1100-1112 - all 0 %. The whole admin user/storage management surface is untested.
- `event_stream` L1129 viewer filtering `continue`, L1136 non-camera notification -> admin only, L1144-1145 Lagged/closed. `serve` L1315-1326 (fine to leave).

### detect.rs (73 %)
- `detect_key` L58-64 + `reconcile` restart-on-change L77-82 (0 %): a config patch that should restart the detector (e.g. `masks_json`, `record_sub`) is never verified to do so; panic restart L99-103; `run_detector` backoff L129/135.
- `detect_session` RTSP branch L323-335 (0 %): 0600 concat list, `'` escaping in the URL, `-protocol_whitelist`; L307-308 dispatch to `detect_from_recorder` when `record_sub` and a sub handle exists.
- `detect_from_recorder` L467-571 (0 %): 30 s wait for `recent`, init-change abort, Lagged, `pts_time:` parsing, fallback to `now_dts()` when pts <= 1e9, 30 s no-frame timeout.
- `DetectRun::new` mask rasterisation L166 (no test sets `masks_json`; `rasterize` degenerate polygon L264), preview error L422, fps stats L436-439, `Drop` close error L457.
- `EventState::on_objects` L624-625 (deferred `EventStart` when `require_object` and the first object arrives on an open event), `step` L707-708 close via cooldown/10 min cap (the e2e tests close events through `Drop`, not through `step`), `make_thumbnail` retry exhaustion L758.

### db.rs (86 %)
- `validate_camera_field` L189-190 empty name/main_url, L199-200 non-string `sub_url`, L217-221 malformed `zones_json`/`masks_json` (an invalid polygon string is accepted by the API today only if these lines work; untested).
- `update_storage` L496 non-updatable field, L499 Null, L503 non-number, L508 `archive_to == id`.
- 0 %: `delete_camera` L659, `zm_event_imported` L791, `insert_event_full` L799, `coverage` L884, `close_stale_events` L1130, `update_push_token` L1198, `delete_push_token` L1208, `users` L1231, `delete_session` L1312.
- `events()` L1057-1062 start/end filter placeholders (`?N` numbering after the `kind IN` list) never executed; `events_page` empty-camera short-circuit L1142; `earliest_segment_dts(None)` L900; `upsert_push_token` empty-token bail L1185.

### health.rs (86 %)
- `report` L151-163: "volume not mounted", "under reserve", "<2 days" warn, archive-volume-unavailable warn.
- `doctor`: ffmpeg non-zero exit L278, clock L284, bad `listen` L291, unwritable `thumb_dir` L304, `backup_dir` L308-310, owner mismatch L320, integrity/open failures L326-333, no storage L342, unwritable storage L354, `archive_to` check L357-358, camera with missing storage L368, non-URL `main_url` L375.
- `probe_rtsp` Pass L394-396 (needs RTSP) and 5 s timeout L399; `restore` integrity failure L485; `writable` non-dir L260.

### video.rs (86 %)
- `hls_playlist` legacy branch L163-187: `#EXT-X-DISCONTINUITY` on gap / sample-entry change, `#EXT-X-MAP` + per-fragment `frag_url` lines for `dts_offset != 0`. Nothing writes a segment with `dts_offset != 0` except import.rs (0 %).
- `plan_range_stream` L66 `exact_len=false`, L70 missing storage; `stream_plan` file-open error L111-113; `live_mp4` re-send init on parameter change L226-229, Lagged/closed L232-239.

### retention.rs (86 %) / tier.rs (84 %) / notify.rs (88 %) / mp4.rs (94 %) / transcode.rs (91 %)
- retention: `remove_segment_file` NotFound-tolerated L27 vs other error L28-31; `run_once` L90-94 archive reachable -> skip byte budget, L107-108 nothing left to delete, L129 orphan-event thumb unlink; `delete_camera_files` partial failure L162; `reindex` L197/201/205-206, `adopt_file` empty file L219-221.
- tier: L38-39 missing `archive_to`, L56-59 pressure path, L66-67 archive under reserve, L74-76 move failure break, L98-99 checksum mismatch, L110 unlink failure, L138-139 throttle sleep.
- notify: `webhook_sink` client build failure L168-170, Lagged L176-178, non-2xx/connection error L187-188; `mqtt_messages` `ServiceStarted`/`EventUpdate`/`StorageLow` L294/301/307 and `Notification::name` for those variants L100-107; mqtt credentials L333, publish error L341.
- mp4: 64-bit `largesize` L601-603, size-0 box L605, oversize moof L616, version-0 32-bit tfdt shift L676-682 and clamp L754-757, `default-base-is-moof` flag injection L743-744, malformed-box bails L813/824/902/914/924/955/966.
- transcode: `busy()` L112-114, ffmpeg stderr warn L138, input-ended L152-154, "moof without tfdt" L188, bad box size L226.

### import.rs (0 %), main.rs (0 %)
- `import_zm` L52-112, `import_one` L114-183 (dts_offset from StartDateTime, kind "zm"), `downscale_jpeg` L185, `parse_camera_map` L196-203: no test at all; this is the only producer of `dts_offset != 0` segments and `kind="zm"` events, so the legacy HLS path in video.rs/api.rs and `event_json` L324 are dead too.
- `main::run` L248-407: camera-down/up and storage-low state machines L296-337 inline in the 30 s loop; still not extractable without a refactor (`fn camera_transitions(...) -> Vec<Notification>`).

## 3. Prioritised tests to add (10)

1. **api.rs `camera_delete` / `camera_create`** (`tests/api.rs`). Camera with two segments (`write_segment`) + an event with a thumb file under `thumb_dir`; `DELETE /api/cameras/{id}` as admin -> 200, `<storage>/<id>` gone, `db.camera(id)==None`, `db.events(...)` empty, thumb unlinked; as viewer -> 403; `DELETE /api/cameras/999` -> expect 404 (today: 200, because `update_camera` L650 is a no-op UPDATE and `delete_camera` L659 deletes nothing). `POST /api/cameras` without `storage_id` on a fixture with no storage -> 400 "no storage configured".
2. **recorder.rs frame loop without RTSP** (new `tests/recorder.rs`, after extracting L399-604 into `SessionState::on_frame(is_key, elapsed_90k, wall_now, Bytes, Option<VideoParams>)` + `finish()`). Feed `parse_encoded(encode_fmp4(25,"libx264",..))` with `segment_secs=10`: assert 3 segment rows, `scan_segment_file` offsets == indexed `FragEntry`s, `Recent.frags.len()<=3`, `MotionLog::push` values appear in `FragEntry.motion`; jump `wall_now` back 5 s for 31 s at a keyframe -> new segment and strictly increasing `dts`; `open_segment` twice with the same `start_dts` -> `-1.mp4`. Catches: off-by-one in rollover (`>=` L576), tfdt/offset mismatches, unbounded `Recent`.
3. **detect.rs `detect_from_recorder` via fake `CamHandle`** (`tests/notify.rs`, reuse the handle construction from `tests/transcode.rs` L120-128 but in `hub.subs` with `kind: Sub`, `record_sub=true`). Broadcast `LiveFrag`s from a written testsrc segment every 400 ms; assert `status.running`, an `EventStart` whose `start_dts` is within 1 s of the fragment `dts` (proves the `pts_time:` path L560-563 and not the `now_dts()` fallback), and that `hh2.motion.push` L444 shows up in `MotionLog::latest`. Second case: send a `LiveFrag` with a different `init` -> session ends with "codec parameters changed" and `last_error` set.
4. **detect.rs event close through `step`** (unit test in `src/detect.rs`, `#[cfg(test)]`, drive `EventState::step` with synthetic dts). Motion for 3 frames then silence: event closes when `quiet > cooldown_secs`, `end_dts == last_motion + post_secs`, `meta_json.frames/motion_frames` correct, `in_event==None`; continuous motion with `cooldown_secs=600`: closes at exactly 10 min (`max_len` L704) and a new event starts on the next frame. Also `require_object=true` + object arriving after `insert_event`: `EventStart` published once (L624-625), never twice.
5. **zmapi.rs `login` failure + `index_php view=image`** (`tests/api.rs`). `POST /zm/api/host/login.json` wrong password -> 401 in >=500 ms and `login_guard` counts it; 11 failures -> 429 (also pins native `login` L236). `view=image&eid=E&fid=snapshot&width=320` on an event with `thumb_path` -> stored bytes; `width=1280` -> decoded via `frame_jpeg_pub` on-disk path; `fid=3` -> 200 `image/jpeg`; hidden camera as viewer -> 404; `view=view_event_hls` -> playlist whose URLs carry `?token=`; `view=bogus` -> 404.
6. **HLS/segment legacy path with `dts_offset != 0`** (`tests/api.rs`). `insert_segment_ext(..., dts_offset=start_dts - first_tfdt, ...)` on a tfdt-near-zero `encode_fmp4` file; `GET /api/cameras/{id}/playlist.m3u8` must contain `#EXT-X-MAP:URI="/api/segments/N/init.mp4"` and `frag/{fi}/seg.m4s` lines with `#EXT-X-DISCONTINUITY` between two such segments; fetch `init.mp4` + every `seg.m4s`, concatenate, `count_frames` == expected and `first_tfdt` == start (proves `rebase_fragment` L754-757/`shift_tfdt`); `fi` out of range -> 404. This is the imported-ZM-event playback path and has never executed.
7. **import.rs** (new `tests/import.rs`). Stage `<zm>/<mid>/<date>/<eid>/<eid>-video.mp4` from `encode_fmp4` plus `snapshot.jpg`; TSV with 4 rows (ok, missing file, `incomplete`, unmapped monitor); assert `ImportStats{imported:1, missing_file:1, failed/skipped:..}`, second run `skipped_existing:1` (`zm_event_imported` L791), event `kind=="zm"` and `GET /zm/api/events/{id}.json` shows `Cause/Notes` mapping "ZoneMinder" (zmapi L324), `segments_in_range(cam, start,..)` finds it and `video_range` over it decodes (dts_offset correct).
8. **zmapi.rs notifications + console_events + event_one** (`tests/api.rs`). `POST notifications.json` `Notification[Token]=abc&Notification[MonitorList]=1,2` -> `GET` returns it; `PUT` changes `MonitorList`; user B `DELETE`ing A's id -> row still present (`delete_push_token` L1208 is scoped by `user_id`, never verified); empty token -> error (L1185). `consoleEvents/1 day.json` counts only events started within 86400 s and only visible cameras. `GET /zm/api/events/{id}.json` viewer on hidden camera -> 404.
9. **api.rs admin surface: users/tokens/storages/stats** (`tests/api.rs`). `POST /api/users` short password -> 400, role "root" -> 400, duplicate username -> 400, viewer with `cameras:[c1]` then `GET /api/users` lists `cameras`; `DELETE /api/users/{self}` -> 400 "cannot delete yourself"; `POST /api/tokens` -> token usable as Bearer, and 403 as viewer; `POST /api/storages` with a file path -> 400, with a dir -> id; `PATCH /api/storages/{id}` `{"archive_to": id}` -> 400 (db L508), `{"bogus":1}` -> 400; `GET /api/stats` as viewer lists only visible cameras with a hub handle.
10. **health.rs doctor/report failure branches + retention/tier edges** (`tests/health.rs`, `tests/retention.rs`). doctor: `ffmpeg="/bin/false"` -> Fail "exit", `listen="nope"` -> Fail, `thumb_dir` = existing file -> Fail, `db_path` in a missing dir -> Fail + early return (L331-333), storage row whose path is a file -> Fail, `archive_to` -> missing id -> Warn, camera with `storage_id=99` -> Fail, `main_url="not a url"` -> Fail; report: removed storage dir -> "volume not mounted", `reserve_bytes=i64::MAX/2` -> "under the reserve". retention: storage over `max_bytes` with reachable `archive_to` -> `run_once` deletes nothing (L90-92) then `tier::run_once` moves; archive dir removed -> falls back to deleting (L94); orphan event with `thumb_path` -> file removed (L129); `chmod 0500` day dir -> row kept, `run_once` returns Ok; tier: destination read-only -> `moved==0`, no `.part` left (L74-76); `archive_rate_mbps=1` on a 2 MB file -> >=1 s elapsed (L138-139).

## 4. RTSP: what truly needs a source, and what to use as a fake

Needs a real RTSP server (retina client, no way around it): `recorder::record_session` L353-386 (DESCRIBE/SETUP/PLAY/demux), `run_camera`/`reconcile` end-to-end (backoff, restart-on-change, panic supervisor), `health::probe_rtsp` Pass branch L394-396, `detect_session`'s concat-list RTSP branch L323-335 (ffmpeg client). Everything downstream of `demuxed.next()` (L399-631) and every hub consumer (`api::live/snapshot`, `frame_jpeg_pub` recent path, `zmapi::nph_zms`, `video::live_mp4`, `detect_from_recorder`) does not: `tests/transcode.rs` L120-128 already shows the fake-`CamHandle` pattern.

**`ffmpeg -f rtsp -rtsp_flags listen` does NOT work as a server on this build (ffmpeg 6.1.1-3ubuntu5), and cannot on any build.** Verified: `ffmpeg -re -f lavfi -i testsrc=size=320x180:rate=10 -c:v libx264 -g 10 -bf 0 -f rtsp -rtsp_flags listen rtsp://127.0.0.1:18555/t` fails immediately with `[tcp] Connection to tcp://127.0.0.1:18555 failed: Connection refused` / `Could not write header`; nothing ever listens on 18555 and `ffprobe -rtsp_transport tcp rtsp://127.0.0.1:18555/t` gets Connection refused. `ffmpeg -h muxer=rtsp` lists no `rtsp_flags`; `ffmpeg -h demuxer=rtsp` shows `listen` with the `.D` (demuxer-only) flag. The RTSP muxer is always a client (ANNOUNCE/RECORD to an existing server); `listen` only makes the *demuxer* accept an incoming ANNOUNCE push, which retina (a DESCRIBE/PLAY client) cannot use. No other server is installed (no mediamtx, live555, gst-rtsp-server, vlc).

What would work, in order of preference:
1. **Tiny in-test RTSP server** in `tests/common/rtsp.rs` (~150 lines, tokio `TcpListener`, no new deps beyond what the crate has: `bytes`, `tokio`, `h264-reader` is already a dependency of retina). Answer `OPTIONS` (Public: DESCRIBE, SETUP, PLAY, TEARDOWN), `DESCRIBE` (SDP: `m=video 0 RTP/AVP 96`, `a=rtpmap:96 H264/90000`, `a=fmtp:96 packetization-mode=1;sprop-parameter-sets=<base64 SPS>,<base64 PPS>` taken from the `avcC` in `parse_encoded(...).params.sample_entry`), `SETUP` (echo `Transport: RTP/AVP/TCP;unicast;interleaved=0-1`, `Session: 1`), `PLAY` (`RTP-Info`), then write interleaved RTP (`$`, ch 0, len, 12-byte RTP header with seq/ts=90 kHz, marker on last packet) carrying each sample's NALs (avcC length-prefixed -> split; FU-A for NALs > 1400 bytes), paced by sample duration. Feed it `parse_encoded(encode_fmp4(30,"libx264",320,180))`. Dropping the socket = "stream ended". This covers 100 % of `record_session`, `run_camera` backoff, `reconcile` restart, `probe_rtsp` Pass and (with a `cam.sub_url` rtsp URL) the detector's concat-list branch, deterministically and in-process.
2. **`rtsp-runtime` crate (0.6, sans-IO RTSP 1.0 server state machine, interleaved RTP)** as a dev-dependency to replace the hand-written request parsing in (1); still needs the RTP packetiser. `rtsp-types` 0.1.3 gives only the message parser. The `rtsp-server` crate on crates.io is a 0.0.0 placeholder; `gstreamer-rtsp-server` needs libgstrtspserver (not installed).
3. **External binary**, gated on `which mediamtx`: `mediamtx` + `ffmpeg -re ... -f rtsp rtsp://127.0.0.1:8554/t` (push with the muxer, which is what the muxer is for). Works but adds a download step to CI and a second process per test; keep as an optional smoke test only.
