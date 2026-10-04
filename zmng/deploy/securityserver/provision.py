#!/usr/bin/env python3
"""Provision the production zmng from the ZoneMinder configuration backup (2026-10-04 cutover).

Reads the TSV dumps in claude-ops/state/zm-cutover-20261004/ and creates, through the zmng API:
cameras 1..23 (same ids as ZoneMinder, so the event import maps 1:1), their groups as tags, the
non-full-frame ZoneMinder zones as normalized zmng zones, the proven detection settings of the
side-by-side test, the user accounts (new passwords: ZoneMinder's bcrypt hashes cannot be
migrated) and the API tokens for night-watch and the camera heartbeat.

  ZMNG_URL=http://10.10.100.100 ZMNG_USER=aaron ZMNG_PASS=... provision.py [--dry-run]
Writes the generated passwords/tokens to secrets/zmng-users.env and secrets/zmng.env (0600).
"""
import csv, json, os, secrets, sys, urllib.request, urllib.error, http.cookiejar

STATE = "/Users/atodd/cowork/claude-ops/state/zm-cutover-20261004"
SECRETS = "/Users/atodd/cowork/claude-ops/secrets"
BASE = os.environ["ZMNG_URL"].rstrip("/")
DRY = "--dry-run" in sys.argv
PRIMARY_STORAGE = int(os.environ.get("ZMNG_PRIMARY_STORAGE", "2"))

cj = http.cookiejar.CookieJar()
opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(cj))

def api(method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(BASE + path, data=data, method=method, headers={"Content-Type": "application/json"})
    try:
        with opener.open(req, timeout=30) as r:
            t = r.read().decode()
            return json.loads(t) if t else None
    except urllib.error.HTTPError as e:
        raise SystemExit(f"{method} {path} -> {e.code}: {e.read().decode()[:300]}")

def tsv(name):
    with open(f"{STATE}/table-{name}.tsv", newline="") as f:
        return list(csv.DictReader(f, delimiter="\t", quoting=csv.QUOTE_NONE))

monitors = sorted(tsv("Monitors-min"), key=lambda m: int(m["Id"]))
zones = tsv("Zones")
groups = {g["Id"]: g["Name"] for g in tsv("Groups")}
gm = {}
for r in tsv("Groups_Monitors"):
    gm.setdefault(int(r["MonitorId"]), []).append(groups[r["GroupId"]])
users = tsv("Users")
gperm = {}
for r in tsv("Groups_Permissions"):
    if r["Permission"] == "View":
        gperm.setdefault(r["UserId"], []).append(groups[r["GroupId"]])

# the settings the side-by-side test ran a week with (Front Door + Playground)
DETECT = dict(retention_days=7.0, event_retention_days=120.0, detect_fps=5.0, detect_width=320, detect_height=180,
              pixel_threshold=25, min_area_pct=1.0, min_blob_pct=0.5, pre_secs=5.0, post_secs=8.0, cooldown_secs=10.0,
              record_sub=True, objects=True, require_object=False)

def zone_for(mid, w, h):
    """ZoneMinder 'All' zones that are not the whole frame become one named zmng zone (normalized)."""
    out = []
    for z in zones:
        if int(z["MonitorId"]) != mid:
            continue
        pts = [tuple(int(v) for v in p.split(",")) for p in z["Coords"].split()]
        full = len(pts) == 4 and min(x for x, _ in pts) == 0 and min(y for _, y in pts) == 0 and max(x for x, _ in pts) >= w - 2 and max(y for _, y in pts) >= h - 2
        if full:
            continue
        out.append({"name": z["Name"], "points": [[round(x / w, 4), round(y / h, 4)] for x, y in pts]})
    return out

print(f"== login as {os.environ['ZMNG_USER']} at {BASE}")
me = api("POST", "/api/login", {"username": os.environ["ZMNG_USER"], "password": os.environ["ZMNG_PASS"]})
existing = {c["name"]: c for c in api("GET", "/api/cameras")}

print("== cameras")
for m in monitors:
    mid = int(m["Id"]); w, h = int(m["Width"]), int(m["Height"])
    tags = ",".join(gm.get(mid, []))
    zj = json.dumps(zone_for(mid, w, h))
    patch = dict(DETECT, tags=tags, zones_json=zj, sort_order=mid)
    if m["Name"] in existing:
        cid = existing[m["Name"]]["id"]
        print(f"   {mid:2} {m['Name']:<24} exists as {cid}; patching")
    else:
        print(f"   {mid:2} {m['Name']:<24} tags={tags or '-':<28} zones={len(json.loads(zj))}")
        if DRY:
            continue
        cid = api("POST", "/api/cameras", {"name": m["Name"], "main_url": m["Path"], "sub_url": m["SecondPath"] or None, "storage_id": PRIMARY_STORAGE})["id"]
    if cid != mid:
        raise SystemExit(f"camera id {cid} != ZoneMinder monitor {mid}: the import camera-map would not be identity; stop")
    if not DRY:
        api("PATCH", f"/api/cameras/{cid}", patch)

print("== users")
have = {u["username"]: u for u in api("GET", "/api/users")}
all_ids = [int(m["Id"]) for m in monitors]
creds = []
def mkuser(name, role, cameras=None, groups=None, note=""):
    if name in have:
        print(f"   {name:<26} exists ({have[name]['role']})"); return have[name]["id"]
    pw = secrets.token_urlsafe(12)
    print(f"   {name:<26} {role:<6} cams={len(cameras or [])} groups={groups or []} {note}")
    creds.append((name, pw, role, note))
    if DRY:
        return None
    body = {"username": name, "password": pw, "role": role}
    if cameras: body["cameras"] = cameras
    if groups: body["groups"] = groups
    return api("POST", "/api/users", body)["id"]

ids = {}
for u in users:
    name = u["Username"]
    if u["Enabled"] != "1":
        print(f"   {name:<26} disabled in ZoneMinder; skipped"); continue
    if u["System"] == "Edit":
        ids[name] = mkuser(name, "admin", note="(ZoneMinder System=Edit)")
    elif u["Monitors"] == "None":
        ids[name] = mkuser(name, "viewer", groups=gperm.get(u["Id"], []), note="(ZoneMinder group-scoped guest)")
    else:
        ids[name] = mkuser(name, "viewer", cameras=all_ids, note="(ZoneMinder Monitors=View: every camera)")

print("== tokens")
tokens = {}
for tname, owner in (("night-watch", "claudeagent"), ("camera-heartbeat", "claudeagent")):
    existing_t = [t for t in (api("GET", "/api/tokens") or []) if t.get("name") == tname]
    if existing_t:
        print(f"   {tname} exists (id {existing_t[0]['id']}); not re-issued"); continue
    if DRY or ids.get(owner) is None:
        print(f"   {tname} for {owner}"); continue
    t = api("POST", "/api/tokens", {"name": tname, "user_id": ids[owner]})
    tokens[tname] = t["token"]; print(f"   {tname} issued for {owner} (id {t['id']})")

if not DRY:
    with open(f"{SECRETS}/zmng-users.env", "a") as f:
        os.fchmod(f.fileno(), 0o600)
        f.write(f"# zmng production accounts created by provision.py on 2026-10-04 (new passwords; ZoneMinder hashes cannot migrate)\n")
        for name, pw, role, note in creds:
            f.write(f"{name}\t{pw}\t{role}\t{note}\n")
    with open(f"{SECRETS}/zmng.env", "w") as f:
        os.fchmod(f.fileno(), 0o600)
        f.write("# zmng PRODUCTION on securityserver (recorder of record since 2026-10-04). UI: http://10.10.100.100\n")
        f.write(f"ZMNG_URL={BASE}\nZMNG_USER={os.environ['ZMNG_USER']}\nZMNG_PASS={os.environ['ZMNG_PASS']}\n")
        f.write("# long-lived admin API token (acts as claudeagent) for night-watch: /api/status, /api/metrics\n")
        f.write(f"ZMNG_TOKEN={tokens.get('night-watch','<see tokens above>')}\n")
        f.write("# token for /usr/local/sbin/zmng-camera-heartbeat.sh (/etc/zm-watch/heartbeat.env on the box)\n")
        f.write(f"ZMNG_HEARTBEAT_TOKEN={tokens.get('camera-heartbeat','')}\n")
    print(f"== wrote {SECRETS}/zmng.env and appended {SECRETS}/zmng-users.env")
print("done")
