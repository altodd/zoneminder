# Side-by-side test deployment on securityserver — 2026-09-28/29

zmng `da693add6` running next to ZoneMinder on 10.10.100.100, recording **Front Door (ZM 1)** and
**Playground (ZM 9)**. ZoneMinder, MySQL, go2rtc, Apache and Tailscale were not changed.

## How it is isolated
| Risk | Control |
|---|---|
| zmng touches ZM files | `ProtectSystem=strict`; the only writable paths are `/var/lib/zmng`, `/var/cache/zoneminder/events/zmng-test`, `/media/zmoverflow/zmng-test`, `/media/zmoverflow/zmng-test-backups` (kernel-enforced) |
| zmng eats ZM's retention | byte caps: 40 GB RAID (tiers after 1 day) → 150 GB USB; reserves 150/300 GB. Worst case ≈190 GB of ZM's oldest footage purged early by ZM's own 95 % filters |
| CPU/IO contention | `Nice=10`, `CPUWeight=50`, IO best-effort 7, `MemoryMax=4G`. Measured: **13 % of one core, 190 MB RSS** for 2 cameras incl. detectors |
| NVDEC (already 93–100 % from ZM) | zmng decodes nothing on the GPU; the H.264 transcode decodes in software and only uses NVENC |
| Camera RTSP limits | +2 sessions per camera (ch101, ch102); both cameras accepted them, 0 reconnects on the LAN |
| Exposure | listens on `10.10.100.100:8080` only; not in the tailnet `group:cameras` ACL |
| Reboots | unit enabled; `RequiresMountsFor` on the real mount points |

Files: `local-test/deploy/{zmng.service,zmng.toml}`. Creds: `claude-ops/secrets/zmng-test.env`.
Night Watch monitors it (runbook §4c). Removal one-liner is in §4c.

## ZoneMinder before / after
| | before (09-28 21:36) | after (09-29 06:35) |
|---|---|---|
| Monitors | 23/23 Connected | 23/23 Connected, avg 18.5 fps |
| Monitor 1 capture / analysis fps | 18.0 / 3.0 | 17.8 / 4.7 |
| Monitor 9 capture / analysis fps | 18.0 / 5.0 | 18.4 / 4.8 |
| NVDEC | 96 % | 100 % (ZM's own; unchanged pattern) |
| Load | 31 | 18 (box was rebooted for kernel 6.8.0-142 at 02:23; not zmng) |

Detection parity overnight (Playground): ZM flagged alarm frames in the 03:16 section (score 26); zmng
opened one event at 03:23:22 (22 s). Front Door: neither flagged anything.

## Camera-dependent paths now exercised on real cameras (coverage-audit §4 items)
All 101 tests (35 unit + 66 integration) pass on x86_64. On the live cameras:
- **recorder**: DESCRIBE/SETUP/PLAY, HEVC 2688×1520 main + H.264 640×360 sub, 60 s segments, 1 s GOPs
  (18 frames), strictly increasing DTS, 0 reconnects over 8 h on the LAN (the Mac-over-VPN run had 2
  retina errors from WAN packet loss — that was the path, not the code)
- **run_camera backoff / reconcile**: bad URL → retries capped at 30 s for 8 h, `issues[]` + Status show it;
  restoring the URL reconnected within 1 s. `record_audio` toggle restarts the main recorder and falls
  back to video-only ("camera offers no AAC track")
- **probe_rtsp** Pass (`doctor --cameras`), **detect_from_recorder** (detectors fed by the substream at 5 fps)
- **API**: snapshot (sub 0.45 s, main 1 s), live.mp4 main + sub, frame.jpg from disk, video.mp4 range
  (2178 frames/120 s, decodes clean), HLS playlist (121 × 1 s fragments), transcode live + range (NVENC)
- **zmNinja API**: login (bad password 401 after 0.8 s), version, monitors, nph-zms single + MJPEG, daemonCheck, diskPercent, events
- **ONVIF**: Front Door answers 404 on `/onvif/device_service` (ONVIF off on the camera); probe fails cleanly with 502
- **Browser (Chrome 152 in the Claude pane, over the site VPN)**: HEVC via MSE true; substream MSE wall
  plays both tiles; camera page plays 4 MP HEVC live and recorded (click on timeline → 05:06 footage);
  Status, Events, Review, Admin render; previews load in 34–270 ms

## Findings to fix before the cut over

*Update 2026-09-30 (`reviews/11-field-review-2026-09-30.md`):* 4 (`-fps_mode passthrough`), 5 (open-segment
frames), 6 (said in the UI) and 7 (`null`; thumbnails render) are fixed and deployed; 2 is fixed in the Admin
UI only (the CLI `add-camera` still defaults to storage 1); 1, 3 and 8 are open. New build `f294ddec…`.
1. **CUTOVER.md paths are wrong for this box.** `/var/cache/zoneminder` is on the root LV —
   `/var/cache/zoneminder/zmng` would record onto the 55 GB-free system SSD; use `/var/cache/zoneminder/events/zmng`.
   ZM's USB storage root is `/media/zmoverflow` (monitor dirs directly under it), not
   `/media/zmoverflow/events` — so §3's import path and the unit's `ReadOnlyPaths=` are wrong, and
   `ReadWritePaths=/media` leaves ZM's USB events writable. `RequiresMountsFor=/var/cache/zoneminder`
   does not require the RAID mount.
2. **`add-camera` defaults to storage 1**, which the runbook makes the *archive* (USB) — every camera
   would record straight onto the archive volume. Pass `--storage <primary>` or default to a storage that has `archive_to`.
3. **Web files keep their source mode.** Installed from this checkout they were `600`, so the UI
   answered 404 on `/` while `doctor` said `PASS web_dir` (it checks `is_file`, not readability).
   Install with `-m 644` and have doctor open the file.
4. **Transcode duplicates frames**: ffmpeg turns the 18 fps VFR stream into 30 fps CFR (1829 frames out
   for 1098 in). Add `-fps_mode passthrough`. Throughput on this loaded box is ~1.5× real time at best
   (60 s in 40 s); a 30 s range under load took >40 s. Good enough for one live viewer, marginal for
   range playback; revisit with `-hwaccel cuda` once ZM no longer saturates NVDEC.
5. **`/api/cameras/{id}/frame.jpg` 404s for the open segment** (t within the last ~60 s); only the zm
   `frame_jpeg_pub` path reads the live hub.
6. Config PATCHes take effect on the next reconcile tick (~30 s), not immediately — say so in the Admin UI.
7. UI nits: a stray `null` above the Status heading; a "Sign in" button on the camera page while signed
   in; the scrub-preview popup sits visible at the left edge with a black tile; the Events list showed no
   thumbnail `<img>` although `/api/events/1/thumb.jpg` returns 200.
8. No alert sink is configured, so `camera_down` delivery (webhook/MQTT) is untested; Status/`issues[]` worked.

## Still to observe (time-based)
Tiering RAID → USB (first segments are 1 day old at ~01:45Z 09-30), cap retention at 40 GB (~2.6 days),
the daily backup into `zmng-test-backups`, daytime event volume vs ZM (the VPN run saw 129 events/15 h on Front Door).
