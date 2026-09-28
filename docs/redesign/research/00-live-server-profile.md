# Live server profile — 10.10.100.100 (securityserver), captured 2026-09-27 17:12 ET

Read-only observations from the production box (no changes made).

## Hardware / OS
- Dell, 2x Intel Xeon Silver 4116 @ 2.10GHz (24 cores / 48 threads), 125 GB RAM (93 GB in page cache)
- GPU: NVIDIA Quadro P2200 (Pascal GP106, 5 GB, driver 580.178.04, CUDA 13.0). 1 NVDEC engine, 1 NVENC engine.
- Storage: / 196G LVM (74%); /var/cache/zoneminder/events = /dev/sda4 6.3T (94% full, 8-disk MegaRAID); /media/zmoverflow = /dev/sdb1 11T SATA (94% full)
- /dev/shm 63G tmpfs, 2.2G used (23 x zm.mmap.N of 98 MB each)
- Ubuntu 24.04.5, kernel 6.8.0-139, ffmpeg 6.1.1, MySQL 8.0.46, PHP 8.3.6 (mod_php, prefork MPM, MaxRequestWorkers 150, opcache on), Apache 2.4.58
- ZoneMinder 1.38.4+noble1 (package); go2rtc at /usr/local/bin/go2rtc with /etc/zm/go2rtc.yaml; Tailscale; WireGuard (docker); Pi-hole; HA containers
- Load average 20-21 on 48 threads; CPU 34% us / 60% idle; IO pressure "some" ~1%, full ~0.85% (sda ~55-60% util, ~3.7k r/s + ~400-570 w/s)

## GPU state (nvidia-smi)
- GPU util 40-41%, **decoder (NVDEC) util 100%**, encoder 0%, 3455 / 5120 MiB used
- 23 zmc processes each hold ~142-148 MiB (monitor 23 = 216 MiB)

## Cameras (23 Hikvision, camera VLAN 10.10.0.0/24)
- Main stream ch101: HEVC Main, 2688x1520 (monitor 23 "Field": 3840x2160), yuvj420p, r_frame_rate 30 but avg 18 fps (camera set to 20 fps VBR, GOP 50)
- Sub stream ch102: H.264 640x360 VBR 1024 kbps 20 fps GOP 10
- CaptureBandwidth ~300-720 KB/s per camera (2.4-5.7 Mbit/s); 23 cams ~= 70-80 Mbit/s aggregate

## Monitor configuration (identical across all 23 except noted)
- Capturing=Always, Analysing=Always, AnalysisSource=Secondary, AnalysisImage=YChannel, AnalysisFPSLimit=5 (monitor 12: 2)
- Recording=Always, RecordingSource=Primary, VideoWriter=2 (passthrough), SaveJPEGs=0, SectionLength=600 s, OutputCodec 0/27, Encoder auto/libx264 (irrelevant for passthrough)
- **Decoding=Always**, DecoderHWAccelName=cuda, DecoderHWAccelDevice=/dev/nvidia0, Colours=4 (RGB32), Width x Height = 2688x1520
- ImageBufferCount=3, MaxImageBufferCount=2000, PreEventCount=5, PostEventCount=5
- StreamChannel=CameraDirectSecondary (live view is the 640x360 substream via go2rtc because browsers cannot WebRTC/MSE HEVC)
- Function mode is effectively "Mocord" (record always, flag motion)
- Zones: one "All" zone per monitor, Blobs check method. Zone `Area` values are 2073523 (~1920x1080) or 921747 (~1280x720) — i.e. the zone polygons were drawn at an OLD resolution and never rescaled to 2688x1520 / 640x360. Needs verification of how zm scales zones when AnalysisSource=Secondary.

## Monitor_Status
- All 23 Connected. CaptureFPS ~18 (monitor 23 reports 36.8, monitor 6 22.2). AnalysisFPS 0-4.9 (capped at 5).

## Per-process CPU (top)
- Every zmc at 55-150% CPU. Example: zmc -m 1 has ONE thread at ~73% CPU (the rest idle); zmc -m 23 (4K) has one thread at 90% + a second at 20%.
- mysqld 38% CPU (steady). apache2 workers idle. zmfilter.pl x3 (filters 1,3,4) + zmwatch.pl.
- Sum: ~21-22 cores busy purely for 23 capture processes whose recording path is passthrough (no encode) and whose analysis is on a 640x360 stream at 5 fps.

## Database (zm, MySQL 8.0 InnoDB)
| table | rows | data GB | index GB |
|---|---|---|---|
| Frames | 243,657,381 | 15.52 | 18.54 |
| Stats | 52,369,546 | 5.08 | 2.83 |
| Events | 57,914 | 0.02 | 0.01 |
| Events_Month/Week/Day/Hour | small | | |
- Events: 59,618 rows, 2024-09-29 .. now, 14.33 TB total DiskSpace, avg Length 747 s, avg Frames 13,544 per event, avg AlarmFrames 877
- Storage 1 "Main" 23,975 events 4.68 TB; Storage 2 "USBDrive" 35,643 events 9.64 TB
- Events indexes: PK(Id), MonitorId, StorageId, StartDateTime, (EndDateTime, DiskSpace)
- ZM_RECORD_EVENT_STATS=1 (Stats table grows), ZM_LOG_LEVEL_DATABASE=0
- Filters running as daemons: PurgeWhenFull (StorageId 2 >= 95%, delete, limit 100), MoveToDiskWhenFull (StorageId 1 >= 95% -> AutoMove to 2, limit 200), "Delete after 7 days low motion" (AlarmFrames < 2), Update DiskSpace
- Web: ZM_WEB_EVENTS_PER_PAGE=25, sort StartTime asc, ZM_WEB_LIST_THUMBS=1 width 48, ZM_WEB_EVENT_DISK_SPACE=1

## Immediate interpretation
1. `Decoding=Always` forces full decode of every 2688x1520 HEVC frame of all 23 main streams (and 4K for one) even though analysis runs on the substream and recording is passthrough. That is what saturates NVDEC (100%) and, after hw->sw transfer + NV12->RGB32 conversion into the 98 MB mmap ring, burns ~1 core per camera. This decode output is only used for live JPEG/zms streaming and `zmu` snapshots.
2. The 243M-row Frames table (34 GB) and 52M-row Stats table are written continuously (~23 x 18 = ~414 Frames rows/sec) and are the main reason event browsing/filters/deletes are slow. Deleting one 10-minute event removes ~13.5k Frames rows + Stats rows.
3. Live view is stuck at 640x360 because HEVC cannot be carried by WebRTC to most browsers; NVENC (0% used) could transcode on demand, or cameras could publish H.264 main streams.
4. Both event volumes sit at 94% with purge-at-95% filters; storage is effectively at steady state.
