# Quick wins on the current production ZoneMinder (10.10.100.100)

**Status 2026-09-27 21:41 ET:** items A1–A4, B7, B9, C11 applied and verified (see the ops log for evidence). **B6/"Decoding=KeyFrames" was tried and reverted**: on 1.38.4 the analysis loop waits for every packet to be decoded, so keyframe-only decoding stops motion detection entirely (AnalysisFPS 0.00 on all cameras). The decode cost therefore stays until the new core replaces `zmc`. D14 dropped by Aaron's decision (recent footage of every camera must stay on the RAID). Ranked by payoff ÷ risk. Evidence in `reviews/*.md` (file:line references) and `research/00-live-server-profile.md`.

## A. Database (highest payoff, low risk)

1. **InnoDB buffer pool 128 MB → 16 GB.** The box has 125 GB RAM and 93 GB sitting in page cache; the 34 GB `Frames` + 8 GB `Stats` indexes are being served from disk (sda ~3.7 k reads/s). `/etc/mysql/mysql.conf.d/mysqld.cnf`: `innodb_buffer_pool_size = 16G`, `innodb_io_capacity = 1000`, `innodb_flush_log_at_trx_commit = 2` (loses ≤1 s of Frames rows on power loss; the UPS shuts the box down gracefully anyway). Needs a MySQL restart (~30 s; zmc keeps recording, events re-open).
2. **`ZM_RECORD_EVENT_STATS=0`** (Options → Config). Stops one `Stats` row per zone per analysed frame. Then, off-hours: `TRUNCATE zm.Stats` (52 M rows, 8 GB, used only by the per-frame stats popup).
3. **Stop zmaudit's orphan scans.** `zmaudit.pl` runs every 900 s: `SELECT DISTINCT EventId FROM Frames WHERE (SELECT COUNT(*) FROM Events …)=0` and a full scan of `Stats NOT IN (SELECT Id FROM Events)` (`zmaudit.pl.in:713-734`). Set `ZM_AUDIT_CHECK_INTERVAL` to 86400, or disable zmaudit on the Servers row, and run it manually when needed.
4. `ANALYZE TABLE zm.Frames;` (optimizer row estimates are off by 20×).
5. Later, off-hours: apply the fork's 1.39.27 index change on `Frames` (add `(EventId, FrameId)`, drop `Type`, `TimeStamp`, `EventId_idx`) — an online index build on 243 M rows takes hours of I/O; do it when nobody reviews footage.

## B. Capture (big CPU/GPU relief; medium risk, reversible per monitor)

6. **Split each camera into two monitors** (the pattern ZoneMinder users with 4K cameras converge on):
   * *Record monitor*: main stream, `Recording=Always`, `VideoWriter=Passthrough`, **`Decoding=KeyFrames`**, `Analysing=None`. Keyframe-only decode keeps snapshot.jpg/alarm.jpg/thumbnails working at 1 decode per GOP (~1/18 of today).
   * *Detect monitor*: substream `…/Channels/102` (640x360 H.264), `Decoding=Always` **software** (no cuda), `Analysing=Always`, `Recording=None`, zones redrawn, `LinkedMonitors` = the record monitor so alarms mark the recording.
   Try it on 2 cameras first. Expected: NVDEC 100 % → <10 %, ~20 cores → ~3.
   Alternative with no re-configuration of zones: just set `Decoding=KeyFrames` on all 23 (motion detection then sees one frame per GOP — acceptable for Mocord since recording is continuous anyway, but detection quality drops).
7. **Zone thresholds.** (Corrected: the polygons on this install already cover the full 2688x1520 frame; the review's "51 %" applied to the stored `Area`, not the coordinates.) `Area` and all `*Pixels` thresholds were still derived from the old 1080p/720p area, making detection 2–9x more sensitive than configured. Fixed by rescaling (`phase0-zones.sql`). Draw future zones in **percent units**.
8. **Fix the 4K "Field" monitor** dimensions (currently declared 3840x2160 but scaled per frame bicubic 8→4 Mpx, `zm_monitor.cpp:3250-3256`): declare the real size or set the record monitor to Decoding=KeyFrames.
9. `ZM_TIMESTAMP_ON_CAPTURE=0` (the camera already burns in a timestamp; ZM's Annotate is a per-frame CPU pass), `ZM_BULK_FRAME_INTERVAL` 100 → 1000.

## C. Web (events list, playback)

10. **`DefaultCodec=MP4HLS`** on monitors (fork feature) so HEVC events play in a `<video>` tag via the byte-range HLS index the videostore already writes, instead of `zms` software-decoding HEVC to MJPEG per viewer.
11. Disable **`ZM_WEB_ANIMATE_THUMBS`/hover preview** (each hover spawns a zms decoding 2688x1520 HEVC at 60 fps).
12. Fork patches (small, in `web/ajax/events.php`): real `LIMIT/OFFSET` + `COUNT(*)`, skip the empty Tags joins/GROUP BY, remove the "All" page size, never call `folder_size()` from the list; backfill the 1,379 NULL `DiskSpace`/`EndDateTime` orphan rows once. Cache `daemonCheck`/storage stats for 30 s; `ob_end_clean()` in `image.php`; stop rotating the auth hash in thumbnail URLs (`ZM_AUTH_HASH_TTL`), so `max-age` caching works.
13. `views/events.php:54` uses `date('Y-m-d h:i:s')` (12-hour `h`) — the default "last hour" window is off by 12 h in the afternoon. One-character fix.

## D. Storage

14. Stop **`MoveToDiskWhenFull`** (AutoMove) and assign monitors to volumes directly (e.g. the 11 TB disk gets the 12 busiest cameras, the RAID the rest); run `PurgeWhenFull` on **both** storages at 90 %, not 95 %, with a bigger limit. Removes 48 GB/pass of copy traffic and the failure mode of copying into a full disk.
15. Retire the no-op "Update DiskSpace" filter.

## E. Things not to do
* Do not enable `ZM_OPT_FAST_DELETE` (moves the cost to zmaudit).
* Do not add an index on `Frames(EventId, Type)` — it doubles write cost for no reader.
* Do not switch the cameras to H.264 to get WebRTC live: storage drops from ~17 to ~9–11 days. HEVC in the browser is solved by MSE/HLS (Safari, Chrome/Edge with hardware decode), which zmng uses.
