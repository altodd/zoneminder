#!/bin/bash
# zmng camera-recording heartbeat + safety net (replaces zm-camera-heartbeat.sh, 2026-10-04).
# Healthy  -> ping Kuma push (status=up) if configured.
# Degraded -> ntfy ops-urgent (deduped) + Kuma push (status=down).
set -u
source /etc/zm-watch/heartbeat.env
STATE=/run/zmng-camera-heartbeat.state
LOG=/var/log/zmng-camera-heartbeat.log
read EXP HEALTHY ISSUES < <(curl -s -m 10 -H "Authorization: Bearer ${ZMNG_TOKEN:-}" "${ZMNG_URL:-http://10.10.100.100}/api/status" 2>/dev/null | python3 -c '
import sys, json
try:
    s = json.load(sys.stdin)
    now = s["now_ms"]
    cams = [c for c in s["cameras"] if c.get("enabled")]
    ok = [c for c in cams if c.get("recording") and c.get("last_frame_ms") and now - c["last_frame_ms"] < 60000]
    print(len(cams), len(ok), len(s.get("issues", [])))
except Exception:
    pass')
TS=$(date "+%F %T %Z")
if [ -z "${EXP:-}" ]; then
  MSG="zmng heartbeat: /api/status FAILED (service down or token invalid)"; STATUS=down
elif [ "${HEALTHY:-0}" -lt "${EXP:-0}" ]; then
  MSG="zmng cameras DEGRADED: $HEALTHY/$EXP recording (issues: $ISSUES)"; STATUS=down
else
  MSG="zmng cameras OK: $HEALTHY/$EXP recording"; STATUS=up
fi
echo "$TS $MSG" >> "$LOG"
if [ -n "${KUMA_PUSH_URL:-}" ]; then
  curl -s -m8 -o /dev/null "${KUMA_PUSH_URL}?status=${STATUS}&msg=$(echo "$MSG"|tr " " "+")" || true
fi
PREV=$(cat "$STATE" 2>/dev/null || echo up)
if [ "$STATUS" = down ] && [ "$PREV" != down ]; then
  curl -s -m10 -H "Authorization: Bearer $NTFY_TOKEN" -H "Title: URGENT: zmng cameras degraded" -H "Priority: 5" -H "Tags: rotating_light,camera" -d "$MSG ($TS)" "$NTFY_URL/$NTFY_URGENT_TOPIC" >/dev/null || true
elif [ "$STATUS" = up ] && [ "$PREV" = down ]; then
  curl -s -m10 -H "Authorization: Bearer $NTFY_TOKEN" -H "Title: zmng cameras recovered" -H "Tags: white_check_mark,camera" -d "$MSG ($TS)" "$NTFY_URL/ops-nightly" >/dev/null || true
fi
echo "$STATUS" > "$STATE"
