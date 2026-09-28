# Cut over from ZoneMinder to zmng (phase 2 runbook)

*For the church security server (10.10.100.100): 23 Hikvision cameras, RAID `/var/cache/zoneminder/events` (6.3 TB) and USB `/media/zmoverflow` (11 TB), staff on the LAN, the CBA guest group over Tailscale, Home Assistant. Every step is reversible until step 7; ZoneMinder keeps running until then.*

## 0. Before you start

| Check | How |
|---|---|
| zmng builds and its tests pass on the VM | `cd zmng && cargo test` |
| The parallel run produced clean recordings for a week | Status page: every camera *recording*, reconnects flat, no issues |
| Playback works in the browsers people use | Safari (Mac/iPhone), Chrome on the office PCs (HEVC via MSE or the H.264 fallback), Firefox (substream / transcode) |
| Backups exist | `zmng backup --to /media/zmoverflow/zmng-backups/zmng.db.backup` and an off-box copy |
| Phase 0 quick wins are still in place on ZoneMinder | they buy time if you have to roll back |

Run the pre-flight on the production binary and config:
```
sudo -u zmng zmng -c /etc/zmng/zmng.toml doctor --cameras
```
Every line must be `PASS` (a `WARN` on `users` is fine before the first admin exists).

## 1. Install on the production box (ZoneMinder still running)

```
sudo deploy/install-ubuntu.sh            # binary, web files, zmng user, /etc/zmng/zmng.toml, systemd unit
```
Edit `/etc/zmng/zmng.toml`:
```toml
db_path   = "/var/lib/zmng/zmng.db"      # on the system SSD, never on a recording volume
listen    = "127.0.0.1:8080"             # Caddy fronts it (step 4); use 0.0.0.0:8080 only on a trusted LAN
web_dir   = "/usr/local/share/zmng"
thumb_dir = "/var/lib/zmng/thumbs"
backup_dir = "/media/zmoverflow/zmng-backups"   # daily VACUUM INTO on the *other* volume
secure_cookies = true                    # behind HTTPS
alert_webhook = "https://ntfy.sh/…"      # or Home Assistant's /api/webhook/<id>
[mqtt]
host = "10.10.100.5"
username = "zmng"
password = "…"
```
Extra RTSP clients are harmless to Hikvision cameras (they allow six), so zmng can pull every camera while `zmc` still does.

## 2. Storage: two volumes, tiered, nothing copied by hand

The design keeps the most recent footage of every camera on the RAID and moves whole 60 s segments to the USB disk as they age (checksummed, rate-limited); nothing is ever purged "when full".

Create the directories next to ZoneMinder's data (do **not** point zmng at ZoneMinder's event directories as recording targets):
```
sudo install -d -o zmng -g zmng /var/cache/zoneminder/zmng /media/zmoverflow/zmng
sudo -u zmng zmng -c /etc/zmng/zmng.toml add-storage /var/cache/zoneminder/zmng --reserve-gb 150 --archive-to 2 --archive-after-days 3   # storage 1 (RAID)
sudo -u zmng zmng -c /etc/zmng/zmng.toml add-storage /media/zmoverflow/zmng --reserve-gb 300                                                   # storage 2 (USB)
```
While ZoneMinder still owns most of both disks, give zmng a **byte cap** on each (`Admin → Storage → Cap`) that fits in today's free space; raise the caps as ZoneMinder's events are deleted (step 7). Capacity math at the current 70–80 Mbit/s aggregate (≈ 0.85 TB/day): 17 TB ≈ 17 days of everything; use `event_retention_days` per camera to keep motion longer than continuous footage.

Add the cameras (main + sub URLs; the names become the UI names, the `tags` field the groups):
```
sudo -u zmng zmng -c /etc/zmng/zmng.toml add-camera "Front Door" "rtsp://user:pass@10.10.0.101:554/Streaming/Channels/101" --sub-url "rtsp://user:pass@10.10.0.101:554/Streaming/Channels/102"
```
or paste them in `Admin → Cameras`. Then `systemctl enable --now zmng`, open the UI, create the first admin with the setup token from `journalctl -u zmng`, set retention/groups/zones per camera, add the staff users (viewers get an explicit camera list; it fails closed).

## 3. Import the existing 14 TB of ZoneMinder events (in place, no re-encode)

On the ZoneMinder host, one TSV per ZoneMinder storage:
```
mysql -B -N zm -e "SELECT Id,MonitorId,UNIX_TIMESTAMP(StartDateTime),UNIX_TIMESTAMP(EndDateTime),DATE(StartDateTime),Length,Frames,AlarmFrames,MaxScore,Archived,IFNULL(Notes,''),DefaultVideo,StorageId FROM Events WHERE EndDateTime IS NOT NULL AND StorageId=1 ORDER BY Id" > /var/lib/zmng/events-storage1.tsv
mysql -B -N zm -e "… AND StorageId=2 …" > /var/lib/zmng/events-storage2.tsv
```
Register ZoneMinder's event roots as **read-only legacy storages** (no cap, big reserve so retention never deletes there before you decide to) and import:
```
sudo -u zmng zmng -c /etc/zmng/zmng.toml add-storage /var/cache/zoneminder/events --reserve-gb 100000     # storage 3
sudo -u zmng zmng -c /etc/zmng/zmng.toml add-storage /media/zmoverflow/events       --reserve-gb 100000     # storage 4
sudo -u zmng zmng -c /etc/zmng/zmng.toml import-zm /var/lib/zmng/events-storage1.tsv --storage 3 --camera-map 1=1,2=2,…   # ZM MonitorId=zmng camera id
sudo -u zmng zmng -c /etc/zmng/zmng.toml import-zm /var/lib/zmng/events-storage2.tsv --storage 4 --camera-map …
```
The files are not touched; each event becomes a legacy segment (timestamps shifted when served) plus an event row with its `snapshot.jpg` as thumbnail. The import is idempotent; `--dry-run` first. Old events then play in the timeline, Review and Events views like new ones. The `zmng` user needs read access to ZoneMinder's event directories (`usermod -aG www-data zmng` or an ACL).

## 4. TLS and the network

* **LAN**: Caddy in front (`deploy/Caddyfile`): HTTPS with a local CA or a real certificate, HTTP/2 (the live wall opens many connections), websocket and SSE pass-through. `secure_cookies = true` in `zmng.toml`.
* **Guests (CBA group) over Tailscale**: `tailscale serve --bg https / http://127.0.0.1:8080` or the Caddy vhost on the tailnet address, and replace the per-monitor `10001–10023` port rules with one rule for port 443 (`deploy/tailscale-acl.example.json`). Their accounts are *viewers* with only the cameras they may see.
* **go2rtc** stays bound to the LAN only (`api: listen: 10.10.100.100:1984`); guests never reach it. The UI marks the WebRTC option accordingly.
* **zmNinjaNg**: portal URL `https://<host>/zm`, event server `wss://<host>/zm/ws`.

## 5. Home Assistant

`deploy/homeassistant.yaml` has the pieces:
* MQTT discovery creates one device per camera (Motion, Recording, Last event, Last event thumbnail) as soon as `[mqtt]` is configured; nothing to add in HA.
* Live images: `camera: platform: generic` on `/api/cameras/<id>/snapshot.jpg?width=1280` with a long-lived token from `Admin → Create API token` (`Authorization: Bearer …`).
* Automations: trigger on `binary_sensor.<camera>_motion` or the `zmng/camera/<id>/event` topic (payload = the event JSON); or point `alert_webhook` at an HA webhook trigger.
* Health: `sensor` on `/api/status` or scrape `/api/metrics` with Prometheus/Grafana.

## 6. Parallel run checklist (one week, all 23 cameras)

Daily, on the Status page: every camera recording, detector fps ≈ 5, reconnects not climbing, both volumes with headroom, effective days ≥ what you expect, tiering moving segments after 3 days, no issues. Compare a few events with ZoneMinder's (same times, sensible thumbnails). Tune `pixel_threshold`/`min_area_pct`/zones on the noisy cameras.

## 7. Cut over

1. `zmng backup` and `zmng doctor --cameras` one more time.
2. Stop ZoneMinder's capture and filters, keep its web UI for reference:
   `systemctl stop zoneminder` (this stops `zmc`, `zma`, `zmfilter`, `zmaudit`, `zmwatch`); `systemctl disable zoneminder`.
3. Raise zmng's byte caps (`Admin → Storage`) to the real budgets: RAID ≈ 5.8 TB, USB ≈ 10 TB, reserves ≈ 150 GB / 300 GB.
4. Decide what happens to the imported ZoneMinder files: leave storages 3/4 with the huge reserve (never deleted, browsable as long as the disks last) or lower the reserve so zmng's retention starts deleting the oldest imported events too. Deleting them is what frees the space for the caps in step 3, so most installs will lower the reserve to the same value as the new storage on that disk.
5. Tailscale ACL: apply the port-443-only policy; remove the `10001–10023` rules.
6. MySQL: keep it running read-only for 30 days (`SET GLOBAL read_only=1`) in case a re-import is needed, then `systemctl disable --now mysql`.
7. Cron/systemd: nothing to add; zmng runs retention, tiering, backups and alerts itself. Watch `journalctl -u zmng -f` for the first hour.

## 8. Roll back (any time before MySQL is retired)

`systemctl stop zmng`, `systemctl enable --now zoneminder`. ZoneMinder's events were never modified; its config is untouched; the phase-0 quick wins are still applied. Lower zmng's caps first if it took space ZoneMinder needs.

## 9. Restore drill (do it once before the cut over)

```
systemctl stop zmng
sudo -u zmng zmng -c /etc/zmng/zmng.toml restore /media/zmoverflow/zmng-backups/zmng.db.backup --stopped
systemctl start zmng            # reindex on start re-adopts any segment files written after the backup
```
Motion scores of segments recorded after the backup are lost (re-adopted files carry none); events after the backup are lost unless they are in a newer backup. That is the whole blast radius.
