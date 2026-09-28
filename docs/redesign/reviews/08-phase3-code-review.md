# zmng phase-3 code review (027ae7a..HEAD on `redesign`)

Scope: preview strips, object detection, review-fix batch c2d5ede, transcode, ONVIF PTZ, federation, PWA, audio. Read-only review; nothing built or run. Paths are relative to `zmng/`.

## Findings (ranked)

### HIGH

**H1. Peer proxy allow-list is bypassed with `..` path segments (ACL hole, token privilege escalation).**
`src/peers.rs:47-62`, `src/api.rs:1202-1235`.
`allowed()` does prefix matching on the raw captured path; `peer_proxy` then builds `format!("{peer.url}/{path}")` and hands it to reqwest, whose `url::Url` parser normalizes dot segments. Axum's `Path` extractor percent-decodes the `{*path}` capture, and hyper passes literal `..` through untouched. So any logged-in local **viewer** can send
`GET /api/peers/barn/api/cameras/../users` (or `%2e%2e`) → forwarded as `GET http://peer/api/users` with the peer token;
`GET .../api/cameras/../peers/other/api/cameras` → chaining, which the module doc says is impossible;
`GET .../api/cameras/../metrics`, `.../api/cameras/../storages`;
`PATCH /api/peers/barn/api/events/../cameras/3` → rewrites a peer camera's RTSP URL if the peer token is an admin (the README only *recommends* a viewer token).
Every "not proxied" guarantee in the doc comment and README is void. Fix: reject the request when any path segment is empty, `.` or `..`, or contains `%`/`\`; better, parse the final URL with `url::Url::parse` and re-run `allowed()` on `u.path()` after normalization, and also require `u.host()/port()` equal the peer's. Add a test with `..` and `%2e%2e` variants (the existing "not proxied" loop in `tests/peers.rs:74-77` only tries direct paths).

### MEDIUM

**M1. zmNinja `view_video` (and any `tfdt_shift != 0` plan) corrupts the audio track's timestamps.**
`src/zmapi.rs:612` (`plan.tfdt_shift = -first_dts`), `src/zmapi.rs:679-682`, `src/mp4.rs:641-698`.
`shift_tfdt` adds the same 90 kHz offset to *every* `traf`'s `tfdt`, including the audio `traf` (track 2), whose baseMediaDecodeTime is in the audio timescale (e.g. 48000). For a `record_audio` camera the exported/zmNinja MP4 gets an audio tfdt off by `first_dts * (1 - 48000/90000)` ≈ hours to years; result: silent or unplayable audio, and players that trust the audio edit list can refuse the file. `rebase_fragment` has the same shape (only reached with `dts_offset != 0`, i.e. legacy imports, which have no audio today — but nothing prevents adopting an AV file with an offset later). Fix: read the `tfhd` track id per `traf` and scale the offset by `audio_rate / TIMESCALE` for track 2 (pass the rate in, or pass a per-track offset table). Cover with a test: write an AV segment via `fragment_av`, request `index.php?view=view_video` (or any shifted plan), and ffprobe the audio stream's `start_time`.

**M2. Detector config changes for objects/require_object/labels never take effect (doc/behaviour mismatch).**
`src/detect.rs:58-64` (`detect_key`), `src/detect.rs:75-83`, `src/detect.rs:117`.
`detect_key` omits `objects`, `object_labels` and `require_object`, so `reconcile` sees "unchanged", `continue`s, and — unlike the recorder (`src/recorder.rs:218`) — never writes the new row into `d.camera`. `run_detector` clones `h.camera` on every session, so even an ffmpeg restart keeps the stale settings; only a process restart or a change to a `detect_key` field applies them. README ("Per camera: object detection on/off, a label override, and require_object … edited in the Admin page") is wrong today. Fix: add those fields to `detect_key` (a restart is fine; the gate is built per session) and also `*d.camera.write() = cam.clone()` on the unchanged path. Same class of bug for `record_audio` (`src/recorder.rs:209-212`): the running session ignores a toggle until the next reconnect; include `record_audio` in the restart predicate.

**M3. Late/in-flight object detections are attributed to the wrong event or silently lost.**
`src/objects.rs:238-266`, `src/detect.rs:606-628`, `src/detect.rs:632-642`.
A detection is answered up to `timeout_ms` (3 s) after the frame; the gate is polled per frame and results are merged into whatever `event_id` is open *now*. If the event closed in between (cooldown), a non-empty result goes to `pending` and is stapled onto the *next* event up to 10 s later (wrong objects on the wrong event). With `require_object`, `close()` deletes the event while a positive detection may still be in flight — the exact case the feature exists for (short person crossing). Fix: tag each result with the event id it was sent for (store `event_id` in the gate request), drop results whose event is gone, and on `close()` with `require_object` and `in_flight`, defer the delete/keep decision until the pending result arrives (or a bounded wait). Not exercised by `tests/objects.rs`.

**M4. Camera label override is ignored when pending detections are replayed at event start.**
`src/detect.rs:684-687`.
`step()` replays `pending` with `ctx.objects.cfg.labels` (global list) whereas live results use the gate's per-camera list (`src/detect.rs:429-431`). Kind/priority differs from the per-camera setting for the very first result. Fix: keep the label list in `EventState` (set from the gate at `DetectRun::new`) and use it in both paths.

**M5. `ptz_presets` leaks the internal ONVIF/PTZ URL to viewers.**
`src/api.rs:938-945`, `src/api.rs:882-884`, `src/onvif.rs:173`.
`Client::call` wraps transport errors with `POST {url}` context; `ptz_err` returns `format!("camera: {e:#}")` to the caller. `ptz_presets` is viewer-reachable (`can_see`), so a viewer gets `camera: POST http://10.10.0.9/onvif/PTZ: connection refused` — the same `ptz_url` that `camera_out` deliberately blanks for viewers (`src/api.rs:334-340`). Also lets any viewer make the server open SOAP connections to the camera on demand (cheap DoS of the camera's ONVIF service). Fix: strip the URL context for non-admins (or map to a fixed "camera unreachable"), and consider admin-only for `presets` (the pad is admin-only in the UI anyway: `web/app.js:329`).

**M6. `find_box` can panic on a truncated `stsd` in a file on disk; reindex runs at boot.**
`src/mp4.rs:1160` (`&b[16..]`), callers `src/retention.rs:224` (adopt at startup, `src/main.rs:258`), `src/import.rs:133`, `src/transcode.rs:171`.
`find_box` guarantees `size >= 8` but slices `[16..]` for `stsd`; a segment file whose `stsd` box is 8..15 bytes (torn write, disk corruption, hostile file dropped into a storage dir) panics `reindex`, which runs synchronously inside `run()` before the server starts → crash loop at boot until the file is removed by hand. Fix: `b.get(16..)?` (and `b.get(8..)?`), and wrap `adopt_file` per file (it already is, but a panic escapes `Result`). Add a fuzz-ish test with truncated init boxes.

**M7. `shift_tfdt` is applied to transcoded moofs but `Content-Length` logic can lie.**
`src/api.rs:655-663`.
`transcoded = mime != source_mime`; if `rfc6381_from_sample_entry` fails for the ffmpeg moov (mime stays `video/mp4`) and the source mime is also plain `video/mp4` (sample entry without `avcC/hvcC`), the response carries the *source* byte length for a *transcoded* body. Clients hang or truncate. Fix: return a `bool` from `maybe_transcode` instead of comparing MIME strings.

### LOW

**L1. ONVIF credentials from the RTSP URL are not percent-decoded; the unit test enshrines it.**
`src/onvif.rs:245-253`, test `src/onvif.rs:306-316`. `url::Url::password()` returns the encoded form, so `rtsp://u:pw%40x@…` sends `pw%40x` as the WS-Security password. (retina receives the raw form too — `src/recorder.rs:342-346` — so the *RTSP* side is equally undecoded; either fix both with `percent_encoding::percent_decode_str` or document that passwords must be typed raw. The test should assert the decoded value, not the encoded one.)

**L2. `xml_attrs` mis-scopes self-closing elements.**
`src/onvif.rs:135-136`: for `<trt:Profiles token="P1"/>` the "element xml" runs to the *next* sibling's `</trt:Profiles>`, so `probe` can select a fixed non-PTZ profile that merely precedes the PTZ one. Fix: detect `/` before `>` as in `xml_section`. No panics found in the scanner (all indices come from `find`, so they sit on char boundaries).

**L3. Unbounded ONVIF / detector response bodies.**
`src/onvif.rs:175` (`res.text()`), `src/objects.rs:188` (`res.json()`): no size cap; a misbehaving device or a URL pointing at a large file (admin-supplied `onvif_url`, or a `probe` reply that redirects `ptz_url` anywhere) buffers arbitrarily. Cap with `Content-Length` check / `bytes_stream` + limit (1 MB is plenty). Note also reqwest follows redirects by default; a camera reply can redirect the WS-Security-bearing POST to another host (auth headers are kept on same-host redirects only, but the digest is in the body). Consider `redirect::Policy::none()`.

**L4. Audio dts mapping assumes both RTP streams share an origin.**
`src/recorder.rs:436-441`: `dts90k = a_wall + (audio.elapsed - video_anchor_elapsed)`. retina's `elapsed()` is per-stream (RTP-Info `rtptime` or first packet). Fine when the camera sends RTP-Info for both tracks; without it the offset equals the arrival gap between the first audio and first video packets (hundreds of ms of permanent A/V skew). Fix: anchor audio on its own `received_wall()` at its first frame (or use RTCP SR NTP). The recorder path is untested; `tests/audio.rs` hand-builds the file.

**L5. Preview store durability/perf.**
`src/preview.rs:104-126, 148-161, 239-242`: (a) a torn record makes every later record in that hour invisible (scanner stops at the first bad header) — truncate to the last good record on open; (b) sprite cache files are written non-atomically to the served path, so a concurrent reader can get a partial JPEG and a crash leaves a bad cache forever — write to `.tmp` + rename; (c) `nearest()` does ~720 seek+read syscalls per hour file (×3 files) per hover request, not "one small file read" as the README claims — cache the `TileRef` table per (cam, hour) keyed by mtime.

**L6. CPU work and sync file I/O on the async runtime in the detector loop.**
`src/objects.rs:242-243` (full-frame RGB conversion + JPEG q85 per detection), `src/preview.rs:277-287` (tile encode + `std::fs` append every 5 s), `src/detect.rs:419` (motion). Few ms each, but it accumulates across 23 cameras on the runtime workers. Move encode+append to `spawn_blocking` or a dedicated thread.

**L7. ptz auto-stop timer race.**
`src/api.rs:905-924`: remove-abort happens before the awaited `continuous_move`; two concurrent moves can leave the first timer running unaborted (extra Stop later; harmless) and the map keeps one `AbortHandle` per camera forever (tiny). Take the lock once after the move.

**L8. Peer 401 logs the local user out.**
`web/app.js:35`, `src/api.rs:1226`: the proxy passes the peer's status through; an expired/revoked peer token → `showLogin()` on the local UI on every peer call. Map upstream 401/403 to 502 in `peer_proxy`.

**L9. Service worker caches non-OK responses and serves `index.html` for any missing same-origin asset.**
`web/sw.js:11`: guard `cache.put` with `res.ok && res.type === 'basic'`; only fall back to `/index.html` for navigation requests (`e.request.mode === 'navigate'`).

**L10. Dead code / minor.**
`src/peers.rs:52`: the `"{x}?"` branch can never match (query is stripped before `allowed()`). `src/detect.rs:566`: showinfo `pts > 1.0e9` guard silently falls back to wall clock for any camera whose tfdt is not absolute (fine, but undocumented). `src/transcode.rs`: a range spanning a sample-entry change feeds two `moov`s to one ffmpeg (`plan_range_stream` emits a second `Init`) — output stops at the change; document or split sessions. `web/app.js:329` never shows the PTZ pad for peer cameras although the proxy allows `POST …/ptz` for admins — pick one.

## Tests

* `tests/peers.rs`: meaningful for the happy path and direct denies; **no normalization/traversal case**, which is where the allow-list actually fails (H1).
* `tests/ptz.rs`: good — the mock verifies the WS-Security digest, profile choice, auto-stop timing, viewer/admin split, zmNinja mapping. Misses the viewer-facing error leak (M5). `src/onvif.rs` unit test asserts the un-decoded password (L1).
* `tests/transcode.rs`: strong (real ffmpeg, tfdt within 0.5 s, 501/503/400 paths, live). Nothing on multi-init ranges.
* `tests/audio.rs`: only the writer/scan/serve path. The README claims "Playback, HLS and export carry the audio" and zmNinja `view_video`; HLS, `view_video` (M1) and the recorder mapping (L4) are unexercised.
* `tests/objects.rs`: real motion-gated flow against a mock server, `require_object`, kind filter — good. Nothing on late results / event boundary (M3), label override replay (M4), or config change without restart (M2).
* `tests/preview.rs`: good, including the `..%2Fetc.jpg` traversal check.
* `tests/notify.rs`, `tests/health.rs`: MQTT fake broker and backup/restore/lock are exercised with real assertions.

## Verdict

The phase-3 code is generally careful (bounds-checked box readers, kill_on_drop ffmpeg children, permits released with the response body, ACL re-checked on SSE, previews' hour key validated), but it ships one real security hole — the federation proxy's prefix allow-list is defeated by `..`, giving any local viewer the peer token's full read (and PATCH) reach, contradicting the module's central promise — and one data-corrupting bug for the new audio feature (audio `tfdt` shifted with a 90 kHz offset in zmNinja exports). Two behaviour/doc gaps (object-detection settings not applied without a process restart; late detections mis-attributed or lost under `require_object`) undermine the object-detection feature as documented. Fix H1 and M1–M3 before merging; the rest are follow-ups. Test coverage is above average for this repo but is aimed at the happy paths; the failing cases above are the ones to add.
