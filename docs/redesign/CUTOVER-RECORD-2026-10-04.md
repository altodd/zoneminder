# Cut-over record — securityserver, 2026-10-04

zmng `0ab0b4de9` replaced ZoneMinder 1.38.4 as the recorder of record on the church security server
(10.10.100.100) at 18:29 ET. The parallel pull meant **no recording gap**: zmng recorded 22/23 cameras
while ZoneMinder still ran (camera 13 refused a third main-stream session until ZoneMinder let go), then
23/23 within 45 s of `systemctl disable --now zoneminder go2rtc`. ZoneMinder closed all its events cleanly.

## What was done (in order)
1. Backups: full `zm` dump (Frames separately), every config table as TSV, config files, the test
   instance's index, the event catalogue as TSV → `/media/zmoverflow/zm-backup-20261004/` + an off-box copy.
2. Build with four fixes (see the commit): NVDEC decode for transcodes, whole-directory retirement of
   imported ZoneMinder events, `--motion-min-alarm-frames`, `zm` markers never pin, overlap-based orphans.
3. GPU: detector venv with onnxruntime-gpu **1.25.1** (CUDA 12; PyPI ≥ 1.26 is CUDA 13, which dropped
   Pascal) and cuDNN pinned to **9.7.1.26** (9.27 → "no kernel image is available" on the P2200).
   YOLO11s 640 px: ~21 ms. `[transcode] hwaccel = "cuda"`, `h264_nvenc`.
4. Install (`deploy/securityserver/`): config, units (no nice, `MemoryHigh=64G`/`MemoryMax=96G`,
   `CAP_NET_BIND_SERVICE`, `SupplementaryGroups=www-data`), storages 1 (USB archive) / 2 (RAID primary,
   3-day tiering at 200 Mbit/s) / 3+4 (ZoneMinder trees, read-only). Apache → :8081, zmng → :80.
5. `provision.py`: 23 cameras with ZoneMinder's ids, groups as tags, the four custom zones normalized,
   the test's detection settings, 7-day / 120-day retention, 12 accounts, 2 tokens.
6. Cut over; heartbeat and Kuma repointed; Field camera restored to 3840×2160 via ISAPI.
7. `import-zm` for both trees in the background (62k events, 15.5 TB, in place).
8. Test instance removed (190 GB freed), one rollback anchor kept.

## Lessons for CUTOVER.md (now corrected there)
- On this box `/var/cache/zoneminder` is on the root LV: the RAID is mounted at
  `/var/cache/zoneminder/events`, and ZoneMinder's USB root is `/media/zmoverflow` itself.
- `add-camera` on the CLI defaults to storage 1 (the archive): always pass `--storage`.
- `import-zm` on a single USB disk does ~1,200 small reads per 10-minute file (~3 ms each): budget
  1–2 days for 35k events and run it in the background; it is idempotent.
- The pip `nvidia-cudnn-cu12` that onnxruntime-gpu pulls in by default is too new for Pascal; pin it.
- A `tar` made on macOS carries `._*` AppleDouble files and the Mac's uid/gid: clean the web dir after extracting.

## Addendum, 21:19 ET — passwords did come over after all
Hashes cannot be converted (bcrypt → argon2 needs the plaintext), but zmng now verifies ZoneMinder's
`$2y$` bcrypt hashes at login and re-hashes with argon2 on the first successful login (`336820c4a`).
`import-zm-users <Users.tsv>` copied the 12 hashes onto the accounts of the same name, so every
ZoneMinder login works unchanged. CUTOVER.md §2 now says so instead of "add the staff users".
