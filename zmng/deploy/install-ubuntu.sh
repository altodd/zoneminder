#!/usr/bin/env bash
# Install zmng on Ubuntu 24.04 (test VM). Run as root from the repo's zmng/ directory
# after `cargo build --release` (or copy the binary in).
set -euo pipefail
apt-get update && apt-get install -y ffmpeg
id -u zmng >/dev/null 2>&1 || useradd --system --home /var/lib/zmng --shell /usr/sbin/nologin zmng
install -d -o zmng -g zmng /var/lib/zmng /var/lib/zmng/thumbs /etc/zmng /var/cache/zmng /usr/local/share/zmng
install -m 755 target/release/zmng /usr/local/bin/zmng
cp -r web/* /usr/local/share/zmng/
if [ ! -f /etc/zmng/zmng.toml ]; then
  cat > /etc/zmng/zmng.toml <<CFG
db_path = "/var/lib/zmng/zmng.db"
listen = "0.0.0.0:8080"
web_dir = "/usr/local/share/zmng"
thumb_dir = "/var/lib/zmng/thumbs"
# daily index backup; put it on the other volume (created if missing)
# backup_dir = "/media/zmoverflow/zmng-backups"
segment_secs = 60
fsync = true
ffmpeg = "/usr/bin/ffmpeg"
# go2rtc_url = "http://10.10.100.100:1984"
session_hours = 336
log = "info,retina=warn"
secure_cookies = false
# alert_webhook = "https://ntfy.sh/zmng-alerts"
# [mqtt]
# host = "10.10.100.5"
# username = "zmng"
# password = "..."
CFG
fi
install -m 644 deploy/zmng.service /etc/systemd/system/zmng.service
systemctl daemon-reload
echo "Next:"
echo "  sudo -u zmng zmng -c /etc/zmng/zmng.toml add-storage /var/cache/zmng --max-gb 200 --reserve-gb 20"
echo "  sudo -u zmng zmng -c /etc/zmng/zmng.toml add-camera 'Front Door' 'rtsp://user:pass@10.10.0.101:554/Streaming/Channels/101' --sub-url 'rtsp://user:pass@10.10.0.101:554/Streaming/Channels/102'"
echo "  sudo -u zmng zmng -c /etc/zmng/zmng.toml doctor --cameras"
echo "  systemctl enable --now zmng   # then open http://<vm>:8080 and create the first admin"
echo "Cut-over runbook: docs/redesign/CUTOVER.md (TLS: deploy/Caddyfile, Tailscale: deploy/tailscale-acl.example.json, HA: deploy/homeassistant.yaml)"
