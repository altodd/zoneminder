# zmng test coverage review

Source: copy of `/home/user/zoneminder/zmng` at scratchpad `zmng-cov/`, `cargo llvm-cov --locked --all-targets` (23 integration tests + 3 unit tests, all pass; run needs ffmpeg/ffprobe). Full per-line dump: `zmng-cov/cov.txt`; condensed uncovered-range list: `zmng-cov/uncov.txt`. Line numbers below refer to the copied sources (identical to the repo at copy time).

## 1. Per-file line coverage

| File | Lines | Missed | Line % | Fn % | Notes |
|---|---:|---:|---:|---:|---|
| mp4.rs | 736 | 61 | 91.7 | 87.2 | H265 arm of `Codec::parse` (L42) and `prefer_hvc1` rewrite (L600) never hit: no test uses libx265 |
| thumbs.rs | 42 | 1 | 97.6 | 100 | only the "ffmpeg produced no image" bail (L43) |
| notify.rs | 262 | 35 | 86.6 | 90.6 | webhook/mqtt error+reconnect paths |
| health.rs | 324 | 48 | 85.2 | 71.4 | every Fail/Warn branch of `doctor`, `report` storage issues, `probe_rtsp` |
| tier.rs | 122 | 20 | 83.6 | 80.0 | missing archive, pressure path, reserve stop, checksum mismatch, throttle |
| db.rs | 893 | 175 | 80.4 | 78.5 | migrations, user/session/push-token CRUD, validation error arms |
| video.rs | 188 | 46 | 75.5 | 88.2 | `live_mp4` (0%), legacy `dts_offset` HLS branch, file-open error |
| config.rs | 26 | 7 | 73.1 | 33.3 | `Config::load` never called |
| retention.rs | 191 | 55 | 71.2 | 55.6 | `delete_camera_files`, `backup_db`, archive-fallback, no-victims, thumb cleanup |
| api.rs | 825 | 313 | 62.1 | 51.7 | 83 of 172 functions never entered (see §2) |
| detect.rs | 514 | 224 | 56.4 | 64.6 | event continuation/close, thumbnail, `detect_from_recorder` (0%), restart on config change |
| zmapi.rs | 832 | 418 | 49.8 | 32.9 | 114 of 170 functions never entered |
| recorder.rs | 484 | 469 | 3.1 | 10.0 | only `MotionLog`/`LiveHub` helpers; the whole RTSP session is untested |
| import.rs | 149 | 149 | 0.0 | 0.0 | no test |
| main.rs | 256 | 256 | 0.0 | 0.0 | CLI + 30 s supervisor loop, not unit-testable as written |
| **TOTAL** | 5844 | 2277 | **61.0** | **56.1** | |

## 2. Untested code paths by file

### api.rs (62%)
- `auth_mw` L159-163: the zero-users "setup" pseudo-admin (`id: 0`) is never exercised. Because only `create_token` (L799) checks `u.id == 0`, in setup mode an unauthenticated `POST /api/users` (`user_create` L825) or `POST /api/cameras`/`/api/storages` succeeds **without the setup token**, i.e. the token gate in `setup` (L272) can be bypassed. Likely real bug; no test covers it.
- `setup` L268-283: wrong-token 403 (L272-274), short-password 400 (L276), `add_first_admin` race returning false (L280). Only the "already set up" 403 is hit.
- `login` L225: the 429 after `MAX_FAILS`(10) is never reached (test does one failure).
- `logout` L250-257 (0%), `token_from` cookie parsing L134-138 (cookie auth never used by a test; only Bearer/`?token=`).
- `camera_create` L358-366, `camera_delete` L379-389 (0%): deleting a camera with segment files + thumbs, `hub.remove_camera`, `delete_camera_files` error -> 500.
- `event_thumb` L547-560 (0%): thumb_path None -> 404, missing file -> 404, cache headers.
- `video_range` L586 (>6 h -> 400), `video_hls` L613 (>24 h -> 400), `video_hls` legacy URL closures L623-624, `segment_file` L644 (416 on bad Range).
- `segment_init` L665-679, `segment_frag` L682-698 (0%): legacy (`dts_offset != 0`) HLS path, incl. out-of-range `fi` -> 404 and viewer visibility.
- `live` L713-732, `snapshot` L738-747 (0%): 503 when camera not in hub; mime/last-dts from `Recent`.
- `frame_jpeg_pub` L86-112 (0%): "recent" path (dts within 5 s of now, reads hub) vs. on-disk path; 404 "no recording at that time"; `decode_sem`.
- `create_token` L797-805, `users` L807-814, `user_create` L825-839 (short password, bad role, duplicate username -> 400), `user_update` L847-853, `user_delete` L855-862 (self-delete 400).
- `storage_update` L876-880, `storage_create` L889-899 (non-dir -> 400), `stats` L901-913 (0%).
- `event_stream` L930 (viewer filtering `continue`), L934 (`None => admin` branch for non-camera notifications), L942-943 (Lagged/closed).
- `serve` L1034-1045 (real listener; fine to leave).

### zmapi.rs (50%)
- `login` L112 refresh-token branch, L115 429, L121-124 bad-password branch (dummy hash + guard + 500 ms sleep).
- `get_timezone`/`local_tz_name` L55-74 (TZ env, /etc/timezone, /etc/localtime symlink), `daemon_check`, `get_load`, `get_disk_percent` L158-171 (percent maths with 0 capacity), `config_by_name` L173-182 (ZM_PATH_ZMS / ZM_GO2RTC_PATH), `empty_list`, `users` L188-196 (viewer vs admin permission strings), `storage_list` L198-206, `not_found`, `saved`.
- `monitor_one` L254-259 (0%): `.json` suffix parsing, viewer 404 for hidden monitor.
- `parse_filter` L310-311 (`Notes REGEXP`/`Cause REGEXP`, ignored segments), `event_json` L324-325 kind mapping "zm" -> "ZoneMinder".
- `event_one` L388-398, `event_put` L400-414, `event_delete` L416-425 (0%). Note `event_delete` has no admin check: any viewer with camera visibility can delete events, while the native API exposes no delete at all. Decide and pin it with a test.
- `console_events` L427-438 (0%): hour/day/week interval parsing, per-camera counts.
- `notifications_get/post/put/delete` L444-471 (0%) and the underlying `db.update_push_token`/`delete_push_token`.
- `index_php` L483-536: `view=image` (numeric `fid` -> `frame_jpeg_pub`, thumb fast path for width<=640, viewer 404), `view=view_event_hls` L513-526, `view=request` L528-534, unsupported view 404. Only `view=view_video` is covered.
- `serve_event_mp4` L544 (no recording -> 404), L551-555 (`Accept-Ranges: none` when `!exact_len`), L560/564/578 (Range parsing, 416, Content-Range header).
- `nph_zms` L634-710 (0%): MJPEG transcode from the sub-stream hub handle, 503 when no recorder, `decode_sem` permit lifetime, init-change abort L689.
- `es_session`: BADAUTH L784-785, NOAUTH L797-798, Ping/Close L812-814, `("push", _)` L830-831, Lagged/closed L840-841.

### recorder.rs (3%) — requires an RTSP source
Everything from `reconcile` (L176) down is uncovered:
- `reconcile` L176-252: stop of removed/disabled cameras, restart when `stream_url`/`storage_id` changes vs. in-place `camera` update, substream recorder only when `record_sub && sub_url starts_with("rtsp")`, panic supervisor with 5 s restart.
- `run_camera` L261-306: missing storage -> 30 s sleep, status reset on session end, backoff doubling to 30 s and reset after a 60 s healthy session, stop via `watch`.
- `record_session` L331-566: DESCRIBE/SETUP/PLAY errors, "no h264/h265 video stream", 20 s packet timeout, parameter change mid-stream (flush + `close_segment` + new init, L399-425), wall-clock anchoring and re-anchor after 30 s of >1 s drift at a keyframe (L440-462) incl. `force_new_segment` when the clock steps back, strict monotonic `dts` (L464-466), pending-sample duration finalisation (L469-472), GOP -> fragment on keyframe, segment rollover at `segment_secs` (L480-486), dropping non-key frames before the first keyframe (L488-490), 5 s stats (L498-508), end-of-session flush (L515-533).
- `open_segment` L568-610: `create_new` collision -> `-N.mp4` suffix loop (L589-597), day directory layout, sub prefix.
- `flush_gop` L612-653: write failure -> Err and gop cleared, `FragEntry.motion` from `MotionLog`, `recent.frags` capped at 3, broadcast.
- `close_segment` L656-686: motion recompute at close, fsync warning, empty segment file removed, `insert_segment_ext`.

How to cover without a camera:
1. Cheapest and most valuable: split the per-frame body of `record_session` (L378-509) into a `SessionState` struct with `fn on_frame(&mut self, ctx, h, cam, storage, is_key, elapsed_90k, wall_now, data, params_changed: Option<VideoParams>) -> Result<()>` and `async fn finish(self, ...)`. `tests/common::parse_encoded` already yields `(duration, is_key, Bytes)` samples plus `VideoParams`; feed them with synthetic RTP/wall clocks. This lets a test assert: segment rollover at `segment_secs`, `-1.mp4` collision naming, re-anchor + `force_new_segment` on a backwards wall clock, monotonic dts, `Recent.frags.len() <= 3`, broadcast `LiveFrag`, motion max merged at close, empty segment unlinked, and DB rows (`segments_in_range`, `FragEntry` offsets equal to file offsets, `scan_segment_file` of the written file matches the index).
2. For the retina glue (`DESCRIBE`..`demuxed`), spawn ffmpeg as a one-shot RTSP server: `ffmpeg -re -f lavfi -i testsrc=size=320x180:rate=10 -c:v libx264 -g 10 -bf 0 -f rtsp -rtsp_flags listen rtsp://127.0.0.1:<port>/t` and point `main_url` at it; mark the test `#[ignore]`-free but gated on `which ffmpeg`. Assert `CamStatus.connected`, a segment row appears within ~15 s, and killing ffmpeg drives `run_camera` to `reconnects == 1`, `last_error` set, then a fresh segment after restart.
3. `flush_gop`/`open_segment`/`close_segment` can be made `pub(crate)` and covered by an in-file `#[cfg(test)]` module with a fake `CamHandle` (all fields are `pub`, so `tests/` can also build one: `CamHandle { kind, camera, status, live: broadcast::channel(64).0, recent, motion, stop }`). That same fake handle inserted into `hub.cams` unlocks `api::live`, `api::snapshot`, `zmapi::nph_zms`, `video::live_mp4`, `frame_jpeg_pub`'s recent path and `detect_from_recorder` with no network.

### detect.rs (56%)
- `detect_key`/`reconcile` L61-106: restart-on-config-change and the "unchanged -> continue" branch; panic restart.
- `run_detector` L118-138: session failure -> `running=false`, warn, exponential backoff to 60 s.
- `EventState::step` `Some(id)` arm L568-609 (0%): peak update, 2 s throttled `update_event_progress`, cooldown close, 10 min hard cap (`max_len`), `EventEnd` publish, `in_event=None`, thumbnail task. The e2e test stops at `EventStart`.
- `make_thumbnail` L618-649 (0%): retry loop waiting for the segment to close, `set_event_thumb`, `EventThumbnail` publish.
- `detect_from_recorder` L422-526 (0%): 30 s wait for `recent`, init-change abort, Lagged, `pts` parsing from ffmpeg stderr, 30 s no-frame timeout.
- `DetectRun::new` mask rasterisation L169-170 (no test sets `masks_json`), `rasterize` degenerate polygon L267/L288.
- `detect_session` RTSP branch L326-338 (0600 concat list file; quote escaping of `'` in the URL L334) and ffmpeg stderr logging L360-361.

### db.rs (80%)
- `migrate` L207-222: every `ALTER TABLE` upgrade step is dead in tests (fresh schema only). No test opens a v1..v5 database and upgrades it.
- `validate_camera_field` error arms L162-194: empty `name`/`main_url`, non-string `tags`, non-string `sub_url`, non-bool `enabled`, malformed `zones_json`/`masks_json`, unknown key accepted.
- `update_storage` L400-412: non-updatable field, non-number value, `archive_to == id` bail, Null.
- 0%: `delete_camera` L550, `add_first_admin` L569, `zm_event_imported` L665, `insert_event_full` L673, `coverage` L758, `set_event_thumb` L860, `close_stale_events` L984, `update_push_token` L1052, `delete_push_token` L1062, `users` L1085, `delete_user` L1106, `delete_session` L1154.
- `events()` start/end filter arms L909-916, `events_page` empty-camera short-circuit L996, `earliest_segment_dts(None)` L774, `upsert_push_token` empty-token bail L1039.

### health.rs (85%)
- `report` L151-163: "volume not mounted", "under reserve", "<2 days" warn, archive-volume-unavailable warn.
- `doctor`: ffmpeg non-zero exit L266, clock-before-2025-10 L272, bad `listen` L279, unwritable `thumb_dir` L290, `backup_dir` L293-295, integrity_check != ok L303-304, `Db::open` failure early return L308-310, no storage L319, unwritable storage L329, `archive_to` pass/warn L332-333, camera with missing storage L343, non-URL `main_url` L349-351.
- `probe_rtsp` L369-374: Pass (needs an RTSP server) and 5 s timeout Fail (a listening-but-silent `TcpListener` would do).
- `restore` L414 integrity failure; `writable` non-dir L248.

### retention.rs (71%)
- `run_once` L86-90: storage with reachable `archive_to` is skipped by the byte budget (tier handles it); L103-104 over budget but nothing left; L125 orphan-event thumb unlink (test events have no `thumb_path`).
- `remove_segment_file` L27-31: NotFound tolerated (row still deleted) vs. EACCES keeps the row.
- `delete_camera_files` L138-161 (0%), `backup_db` L164-169 (0%).
- `reindex` L199/203/207-208: unknown camera dir skipped, non-mp4 skipped, adopt error; `adopt_file` L221-223 empty file removed.

### tier.rs (84%)
- `run_once` L38-39 missing `archive_to` target, L56-59 space-pressure path (moves oldest regardless of age), L66-67 archive under reserve -> stop, L74-76 move failure -> break.
- `move_segment` L98-99 checksum mismatch (needs a fault-injected copy), L110 source unlink failure; `copy_throttled` sleep L138-139 (never throttled: tests use a high rate).

### notify.rs (87%)
- `webhook_sink` L166-190: client build failure, Lagged, non-2xx warn, connection error warn (test only exercises 200).
- `mqtt_sink` L319 credentials, L327 publish error, L348-350 broker error -> 5 s reconnect (test never drops the broker), L355-356 Lagged/closed.
- `mqtt_messages` L293/300/306: `ServiceStarted` -> retained "online", `EventUpdate`, `StorageLow` mapping. `Notification::name()` for those variants L100-107.

### video.rs (76%)
- `live_mp4` L214-247 (0%): primes with `recent.init` + last frag, re-sends init on parameter change, Lagged/closed.
- `hls_playlist` legacy branch L169-193: `#EXT-X-DISCONTINUITY` on >0.5 s gap and on sample-entry change, `#EXT-X-MAP:URI` without BYTERANGE for `dts_offset != 0` (imported ZM events). No test builds a segment with `dts_offset != 0`, so `rebase_fragment`'s tfdt shifting in `stream_plan` is only exercised with offset 0.
- `stream_plan` L117-119 file-open error propagates as `Err` to the body; `plan_range_stream` L66 `exact_len=false`, L70 missing storage.

### mp4.rs (92%)
- `Codec::parse` H265 arm L42-43 and `prefer_hvc1` L600: **no test encodes H265** although `encode_fmp4` supports `libx265`.
- `scan_segment_file` 64-bit `largesize` L409-411, size-0 "to EOF" L413, oversize moof bail L424; `shift_tfdt`/`rebase_fragment` version-0 32-bit tfdt overflow L484-491, L562-565; `parse_moof` malformed-box bails.

### import.rs (0%), config.rs (73%), main.rs (0%)
- `import_zm` L52-112: TSV row parsing, `camera_map`, `zm_event_imported` skip, `incomplete`/missing file counting; `import_one` L114-183 (dts_offset from StartDateTime, `insert_event_full` kind "zm", thumbnail downscale); `parse_camera_map` L196-203.
- `Config::load` L70-75 (parse error message with path) — `example-config` round trip untested.
- `main::run` L244-394: camera-down/up and storage-low state machines (L296-337) are inline in a spawned loop and unreachable by tests; extract to `fn camera_transitions(prev: &mut HashSet<i64>, cams: &[(i64,String,i64,bool)], now, alert_after) -> Vec<Notification>`.

## 3. Prioritised tests to add

1. **api.rs setup-mode privilege escalation** (`tests/api.rs`): fresh `Fixture` with zero users; `POST /api/users {"username":"evil","password":"password123","role":"admin"}` with no token. Assert 403/400, not 200; then assert `POST /api/setup` with the right token still works. Today this returns 200 and creates an admin without the setup token (`auth_mw` L159, `user_create` L825). Also cover `POST /api/setup` wrong token -> 403, short password -> 400, and `/api/tokens` -> 400 "finish setup first".
2. **recorder.rs session state machine** (new `tests/recorder.rs` after extracting `SessionState::on_frame`; or in-file unit tests): feed `parse_encoded(encode_fmp4(25,"libx264",..))` samples with `segment_secs=10`. Assert three segment rows, `scan_segment_file(abs)` frags == indexed `FragEntry`s, `Recent.frags.len()==3`, and `MotionLog::push` scores before close appear in `FragEntry.motion`. Second case: jump `wall_now` back by 5 s for 31 s at a keyframe -> new segment, `dts` strictly increasing. Third: `open_segment` twice with the same `start_dts` -> second file is `HHMMSS-1.mp4`.
3. **recorder.rs against ffmpeg `-rtsp_flags listen`** (`tests/recorder.rs`, gated on ffmpeg): `reconcile` with a real `rtsp://127.0.0.1:port/t`; assert `status.connected`, a `main` segment appears, then kill ffmpeg and assert `reconnects==1`, `last_error` contains "stream ended"/"no packets", and no zero-fragment file is left in the storage dir (`close_segment` L677-679).
4. **detect.rs event lifecycle** (`tests/notify.rs` extension): sub_url `lavfi:testsrc=..., then a static source` (e.g. `lavfi:color=black` after N s via `-t` concat, or lower `cooldown_secs` to 1 and use `testsrc` then stop the camera); wait for `EventEnd`, assert `event.end_dts == last_motion + post_secs`, `meta_json.frames > motion_frames > 0`, `status.in_event == None`, `EventThumbnail` published and `event.thumb_path` file exists. Second case: `cooldown_secs=600` with continuous motion -> event closes at the 10 min cap (`max_len`, L586) — drive `EventState::step` directly with synthetic `dts` to avoid waiting.
5. **detect_from_recorder via fake CamHandle** (`tests/notify.rs`): build a `CamHandle{kind: Sub, recent: Some(Recent{init, frags: from a written segment})}` in `hub.subs`, `record_sub=true`; broadcast `LiveFrag`s in a loop from the fixture's fragments and assert the detector reaches `running=true` and produces an `EventStart` with `dts` taken from the ffmpeg `pts` (L520-522) rather than `now_dts()`.
6. **db.rs schema upgrade** (`tests/db.rs`): create a DB with the v1 schema (copy the `CREATE TABLE` text minus the columns added at L207-222), set `user_version=1`, `Db::open` it, assert `PRAGMA user_version` == current and `add_camera`+`insert_segment_ext(stream="sub")`+`update_storage(archive_to)` all succeed. Would catch a broken `ALTER TABLE` sequence that only bites upgrading installs.
7. **api.rs camera_delete** (`tests/api.rs`): camera with two segments + an event thumb file; `DELETE /api/cameras/{id}` as admin -> 200, storage dir `storage/<id>` gone, `db.camera(id)` None, `db.events` empty; as viewer -> 403; with a read-only day directory -> 500 and the DB rows still present (`delete_camera_files` L155-157 keeps rows when unlink fails).
8. **zmapi.rs event_put/event_delete/console_events/notifications** (`tests/api.rs`): via form posts `Event[Archived]=1`, `Event[Notes]=x`, then `GET /zm/api/events/{id}.json` shows `Archived:"1"`; `DELETE` as viewer for a hidden camera -> 404 and as viewer for a visible camera — pin whichever policy is intended; `consoleEvents/1 day.json` counts only events since now-86400; `POST notifications.json` with `Notification[Token]=abc` then `GET` returns it and `DELETE` removes it (`db.delete_push_token` scoped by `user_id`: assert user B cannot delete A's token).
9. **index_php view=image** (`tests/api.rs`): event with `thumb_path` -> width 320 returns the stored JPEG bytes; width 1280 -> decoded frame; `fid=3` -> frame at `start+2/18 s` (assert 200 + `image/jpeg`); `fid=snapshot` on an event whose camera is hidden -> 404. Also `view=view_event_hls` -> playlist whose URLs carry `?token=`.
10. **HLS/segment legacy path with `dts_offset != 0`** (`tests/api.rs`): write a segment via `insert_segment_ext(..., dts_offset=some 90k value, ...)` (as `import.rs` does); `GET playlist.m3u8` must contain `#EXT-X-MAP:URI="/api/segments/N/init.mp4"` and `frag/{fi}/seg.m4s` lines; fetch `init.mp4` + each `seg.m4s`, concatenate, `count_frames` == expected, and `ffprobe` start_time ≈ start of range (proves `rebase_fragment` tfdt shift). `fi` out of range -> 404. `view=view_video` must return `Accept-Ranges: none` (L551-555).
11. **import.rs** (`tests/import.rs`): stage `<storage>/<mid>/<date>/<eid>/<eid>-video.mp4` from `encode_fmp4` (tfdt near zero) plus a `snapshot.jpg`; TSV with 4 rows (good, missing file, `incomplete`, unmapped monitor); assert `ImportStats{imported:1, missing_file:2, failed:1}`, second run gives `skipped_existing:1`, event kind "zm" archived flag honoured, `segments_in_range(cam, start_s*90000, ..)` finds it and `video_range` over it decodes (`dts_offset` correct).
12. **health.rs doctor/report failure branches** (`tests/health.rs`): config with `ffmpeg="/bin/false"`, `listen="nope"`, `web_dir` without index.html, `db_path` in a non-existent dir, storage row whose path is a file, a storage with `archive_to` pointing at a missing id, camera `main_url="http://x"` and one with `main_url="not a url"`; assert one `Verdict::Fail` per case by `name`. `report`: storage path removed -> issue "volume not mounted"; `reserve_bytes=u64::MAX/2` -> "under the reserve". `probe_rtsp` against a bound-but-silent `TcpListener` -> Fail "no answer ... in 5 s" (shorten the timeout via a param).
13. **retention.rs edge cases** (`tests/retention.rs`): (a) storage over `max_bytes` but `archive_to` reachable -> `run_once` deletes nothing (L86-90) and `tier::run_once` moves instead; (b) archive dir removed -> falls back to deleting; (c) orphan event with a `thumb_path` -> thumb file removed (L125); (d) segment file already gone (NotFound) -> row deleted anyway; segment file in a `chmod 0500` dir -> row kept and `run_once` returns Ok.
14. **tier.rs** (`tests/retention.rs`): `reserve_bytes` on the archive larger than free -> nothing moved and warn; `archive_rate_mbps=1` on a 2 MB file -> elapsed >= ~1 s (throttle L138); make the destination read-only -> move fails, `stats.moved==0`, source row unchanged and no `.part` left.
15. **notify.rs failure paths** (`tests/notify.rs`): webhook server returning 500 then 200 -> sink keeps running and delivers the second; drop the fake MQTT broker (`tests/common/mqtt.rs`) after ConnAck and restart it -> sink republishes `status` "online" + discovery on the second ConnAck (L340-346 via L348-350); `mqtt_messages` for `ServiceStarted`/`StorageLow`/`EventUpdate` topic + retain flags; `webhook_sink` never posts non-external notifications.
16. **mp4.rs H265** (`tests/api.rs` or `tests/mp4.rs`): repeat `video_range_streams_decodable_fmp4_across_segments` with `encode_fmp4(.., "libx265", ..)`; assert `Codec::parse("hvc1")`, `prefer_hvc1` rewrites `hev1`, `rfc6381` starts with `hvc1.`, mime `video/mp4; codecs="hvc1..."`, and the concatenated stream decodes with ffprobe.

## 4. Real-RTSP-only code and how to fake it
- Truly needs RTSP: `recorder::record_session` L353-375 (retina DESCRIBE/SETUP/PLAY/demux), `health::probe_rtsp`, `detect_session`'s concat-list RTSP branch L326-338. Fake with `ffmpeg -re -f lavfi -i testsrc ... -f rtsp -rtsp_flags listen rtsp://127.0.0.1:PORT/t` spawned per test (one client per process; pick a free port with `TcpListener::bind("127.0.0.1:0")` then drop it).
- Everything after `demuxed.next()` (L378-533) does not need the network once the frame loop is factored out of `record_session`: pass `(is_key, elapsed_90k, wall_now, Bytes, Option<VideoParams>)` tuples produced from `tests/common::parse_encoded`.
- Consumers of the hub (`api::live`, `api::snapshot`, `zmapi::nph_zms`, `video::live_mp4`, `frame_jpeg_pub` recent path, `detect_from_recorder`) need no RTSP at all: insert a hand-built `Arc<CamHandle>` into `hub.cams`/`hub.subs` (fields are `pub`) with `Recent` filled from a fixture segment and push `LiveFrag`s on `handle.live`.
