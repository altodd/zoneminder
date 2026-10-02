# Feature parity review: zmng against ZoneMinder and zmNinjaNg — 2026-10-01

**Question.** Is anything that ZoneMinder or zmNinjaNg does well missing from the redesign, before the cut over?

**Method.** Three feature inventories, compared item by item:

| Product | Source | Version |
|---|---|---|
| ZoneMinder | this repository's PHP/C++/Perl tree | `version.txt` 1.39.35 |
| zmNinjaNg | `github.com/ZoneMinder/zmNinjaNg`, code and user guide | v2.6.0, HEAD `9cac05a` |
| zmng | `zmng/` | `3f62b92` |

**Weighting.** Each gap is weighted by what this site uses: 23 fixed Hikvision RTSP cameras, the CBA guest group over Tailscale, Home Assistant, and volunteers rather than staff admins. That is why the ranking differs from a plain feature count.

## Status (2026-10-02)

Built on `redesign` after this review, with Aaron's decisions (CBA guests keep access to recordings, so no live-only role; tokens and self-service passwords pulled forward from P2):

| Item | Done in |
|---|---|
| P1 #6 zmNinjaNg calls that answered "OK" but did nothing | `fix(zmng): zmNinjaNg alarm state, zones and detection picture; refuse what zmng cannot do` |
| P2 #12 tokens per user, listed, revocable, expiring; P2 #13 own password | `feat(zmng): per-user API tokens with list and revoke; users change their own password` |
| P1 #3 export of a range and several cameras with a manifest | `feat(zmng): export a range of several cameras as a ZIP with a manifest and checksums` |
| P1 #1 zone editor, per-zone sensitivity, zone names on events | `feat(zmng): zone editor, per-zone sensitivity and zone names on events` |
| P1 #2 live detector view and dry runs over recorded video | `feat(zmng): live motion view and dry runs of zone settings over recorded video` (also fixed the live detector's timestamps) |
| P1 #4 push route (ntfy), P1 #5 rules, schedules, arming | `feat(zmng): alert rules with schedules and arming, ntfy push with the live picture` |

Each part then had an independent review (alerts and the UI; tokens, export and the ZoneMinder API; the detector, zones and dry runs). Their findings are fixed in `fix(zmng): review of alerts and the UI: …`, `fix(zmng): review of tokens, export and the ZoneMinder API` and `fix(zmng): review of the detector, zones and dry runs`. Upgrade notes are in CUTOVER §10. Two suggestions were left out on purpose:
- Writing ZIP entries as deflate "stored" blocks so Java's `ZipInputStream` can read them. Desktop unzip tools, Windows, macOS and Python read the archive from its central directory, and nobody here uses a Java tool for it.
- Sharing one path per file in the export plan. It saves a few hundred kilobytes on a 6-hour export.

Still open from this review: the rest of P2 (manual events, digital zoom, stream quality choice, event list workflow, operator role, retention by kind and offsite copy, live wall extras, recording-gap report) and P3.

## Verdict

The core NVR is at parity with ZoneMinder or ahead of it: recording, playback, multi-camera review, events, storage and retention, health, groups and permissions, several servers, object detection, and Home Assistant. Most of what ZoneMinder has that zmng does not is either not used here or replaced on purpose (listed at the end).

The real gaps fall into four groups:

1. **Tuning motion detection.** zmng has no zone editor, no per-zone sensitivity, no view of what the detector sees, and no "re-run on old footage". This is ZoneMinder's main remaining strength. It is also the open item from the field review (11).
2. **Handling an incident.** You cannot export a chosen range or several cameras at once, mark a moment, or zoom into the picture. Events cannot be deleted or acted on in bulk.
3. **Notification rules and push.** Every event of every camera goes to every sink. There is no schedule or arming, no per-kind rule, and no push to phones.
4. **zmNinjaNg calls that answer "OK" but do nothing.** Alarm state, arm/disarm, monitor settings and zones. Users will believe these actions worked.

## Where zmng matches or beats both

| Area | ZoneMinder | zmNinjaNg | zmng |
|---|---|---|---|
| Recording | passthrough, but every frame decoded; per-frame DB rows | — | passthrough, nothing decoded, one row per segment |
| Synchronized multi-camera review | Montage Review | Nearby → Replay (12–36 events) | Review: drag-scrubs every tile; quiet time normal/fast/skip; per-tile speed; prev/next event; named ranges; saved views |
| Timeline | per-monitor score chart | event bars, heatmap | coverage + motion histogram + coloured event kinds, hover previews |
| Events list | slow at this size (see review 02) | filters, favourites, tags | instant keyset paging; filter by object kind and seconds of motion |
| Retention | purge-when-full filters, AutoMove copies | — | policy per camera and per volume; tiering; archived events pin their footage |
| Health | navbar stats, log view | server page | Status page with issues, effective retention days, archive backlog; Prometheus; `doctor`; backups |
| Full-resolution HEVC in the browser | no (MJPEG or go2rtc substream) | go2rtc | MSE live and playback, H.264 transcode fallback |
| Object detection | external (zmeventnotification + pyzm) | shows `objdetect.jpg` and Notes | built in, motion-gated, backfill, kind filters |
| Home Assistant | MQTT topics only | — | MQTT discovery (motion, recording, last event, thumbnail) |
| Several servers | multi-server cluster | profile groups | federation with a read proxy |
| Groups and permissions | hierarchical groups, per-group grants | group filter | groups as tags, per-group grants with a "will see" preview |
| Phone | responsive pages | native app | PWA, phone layout, and the zmNinjaNg API |

## Gaps, ranked

**P1:** staff will hit these in the first weeks after the cut over, or the redesign's own goals depend on them.
**P2:** clear value at modest effort.
**P3:** only if someone asks.

### P1

**1. Zone editor, per-zone sensitivity, and zone names on events**
- *ZoneMinder:* draws polygons over the live image. Six zone types (Active, Inclusive, Exclusive, Preclusive, Inactive, Privacy). Thresholds per zone, presets, and a tool that measures an object's size against the thresholds (`web/skins/classic/views/zone.php`, `js/zone.js`).
- *zmNinjaNg:* overlays the zones on live view (`GET /api/zones.json`).
- *zmng:* zones and masks are JSON text fields in the camera editor, with one set of thresholds per camera. Events do not say which zone fired. `/zm/api/zones.json` returns an empty list.
- *Proposal:*
  - A canvas editor on the camera page that draws over a snapshot. Each zone has a name and include/exclude. Per zone: minimum area and minimum blob.
  - Events record the zones that fired. `peak_cells` is already stored per event, so this is a lookup.
  - An "only in zone X" filter on Events and Review.
  - Serve the zones as percent polygons on `/zm/api/zones.json`.
- *Why now:* the field review's next step is detection quality on Playground (trees, road) and per-camera thresholds. Editing JSON coordinates by hand is not a volunteer task.

**2. A view of what the detector sees, and a dry run on recorded footage**
- *ZoneMinder:*
  - "Show Analysis" on live view.
  - Per-frame, per-zone statistics in the frame views.
  - `zma` re-runs analysis on old events with the current zones.
- *zmng:* a score bar on each Live tile, and the motion histogram after the fact.
- *Proposal:*
  - A toggle on the camera page that overlays the detector's changed 8×8 blocks, the current score and the threshold, live.
  - "Run these settings over last night." This replays the recorded substream through the detector with the edited zones and thresholds, and shows the events they would have produced, without saving them.
  - The substream recordings make the dry run possible without a camera.
- *Why now:* items 1 and 2 together replace ZoneMinder's tuning workflow. Without 2, every threshold change needs a day of waiting to judge.

**3. Export a chosen range, from several cameras, with a manifest**
- *ZoneMinder:*
  - exports several events as tar or zip (video, images, details);
  - merges several events into one MP4;
  - Montage Review has "Download Video (merged)".
- *zmng:*
  - the camera page exports ±30 s around the playhead;
  - the event dialog downloads the event's own span;
  - nothing exports from Review.
- *Proposal:*
  - **Mark in** and **Mark out** on the camera page and in Review.
  - An export dialog with a camera list (Review's current selection), main or sub stream, and one MP4 per camera in a zip.
  - A `manifest.json` in the zip with each camera, the start and end in local time and UTC, and a SHA-256 per file.
  - `video.mp4?start&end` already serves up to 6 h, so this is mostly UI.
- *Why now:* handing footage to the police or an insurer is the main reason this site keeps recordings.

**4. Push notifications to phones**
- *zmNinjaNg:* FCM push with a picture, an interval per monitor, tap to open the event, a history list. It works either through zmeventnotificationNg or through ZoneMinder 1.39's `/api/notifications` registration (the sending happens outside ZoneMinder's tree).
- *zmng:* stores the app's registrations but sends nothing. Sending needs the zmNinjaNg project's FCM relay or credentials, which we do not control.
- *Proposal:* decide the route.
  - Recommended now: **ntfy**, Pushover, or the Home Assistant companion app, fed by the existing webhook or MQTT. These need no code, only a documented recipe that attaches `thumb`, sent on `event_update`.
  - Revisit FCM if the zmNinjaNg relay becomes available to third-party servers.

**5. Notification rules: which cameras, which kinds, when**
- *ZoneMinder:*
  - filters email, SMS or run a command on any criteria;
  - run states ("Home", "Away") switch whole configurations.
- *zmNinjaNg:* per-monitor push interval.
- *zmng:* every event of every camera goes to every sink. The only lever is `require_object` per camera, and that discards events rather than just muting them.
- *Proposal:*
  - A small rules table. Each rule is: sink × cameras or groups × kinds (person, vehicle, any) × schedule (always, nights, outside service hours) × minimum interval.
  - An **armed/disarmed** switch in the UI, also exposed as an MQTT switch so Home Assistant can arm it with the alarm panel.
  - For this site, this replaces ZoneMinder's run states and filter emails.
  - Do this together with item 4. "Person at the Front Door after 10 PM" is the notification people want; "motion anywhere" gets muted within a day.

**6. zmNinjaNg calls that answer "OK" but do nothing (`zmng/src/zmapi.rs`)**

| App feature | Call | zmng today | Fix |
|---|---|---|---|
| Live Activity page; alarm ring on live view | `monitors/alarm/id:N/command:status` (polled; `lib/monitor/live-activity.ts`) | always `{"status":0}`, so Live Activity never shows a camera | return 2 (Alarm) while the camera is in an event, else 0 (about ten lines) |
| Arm / force alarm | `command:on`, `command:off` | no-op | map to a manual event (P2 #7), or answer with an error |
| Monitor settings, Function, Enabled | `POST /monitors/N.json` | answers `{"message":"Saved"}` and ignores the change | apply `Enabled` for admins and refuse the rest with 403/501; a fake "Saved" is the worst answer |
| Zone overlay | `GET /zones.json?MonitorId=N` | empty list | serve `zones_json` as percent coordinates |
| Detection picture in the event strip | `index.php?view=image&fid=objdetect` | the plain thumbnail | draw the stored boxes; zmng's own event cards could use the same image |

These are P1 because they are cheap and app users currently believe the actions worked.

### P2

**7. Manual events, bookmarks and external triggers**
- *ZoneMinder:* Force Alarm on live view, `zmtrigger` (TCP/serial; `on+N` auto-cancel), API alarm on/off, and Snapshots (an event on every shown camera at once).
- *zmng:* none of these.
- *Proposal:* `POST /api/cameras/{id}/events {label, notes, seconds}`, usable with a token. Expose it as:
  - a **Mark** button on the camera page and in Review (all shown cameras);
  - the target for Home Assistant door or alarm-panel automations;
  - zmNinjaNg's Arm button.
- Kind `manual` or `trigger` is shown on the timeline and in Events like the other kinds.

**8. Digital zoom and pan on live and playback**
- *ZoneMinder:* zoom and pan on live view and in the event player.
- *zmNinjaNg:* pinch, wheel and drag, with a reset.
- *zmng:* none.
- 4 MP is only useful for a face or a plate if you can zoom into it. A CSS transform on the `<video>` plus wheel, pinch and drag is enough; "Full-res frame" already covers stills.

**9. Choice of stream quality per view (Auto, Full, Sub)**
- *Today:* the camera page and the event player always use the main stream when the browser can decode it. On a phone over Tailscale and LTE that is 3–5 Mbit/s with no way to opt down.
- *ZoneMinder:* high/medium/low bandwidth profiles.
- *zmNinjaNg:* a low-bandwidth mode.
- *Proposal:* a dropdown remembered per device. The substream URLs already exist.

**10. Event list workflow**
- *ZoneMinder and zmNinjaNg both have:*
  - a custom date and time range;
  - prev/next event inside the player (ZoneMinder follows the filter across cameras and has arrow keys);
  - speed buttons;
  - delete;
  - multi-select archive, delete and export.
- *zmng:* fixed ranges (24 h, 7 d, 30 d, all); the event dialog has no prev/next and no speed buttons; there is no delete (`/api/events/{id}` has no DELETE; only the zmNinjaNg API can delete); no bulk actions; no search in notes.
- *Proposal:*
  - a From/To range;
  - N/P keys and buttons in the dialog;
  - speed buttons;
  - select mode with Archive, Export (through #3) and Delete for admins.
- Deleting an event removes its metadata only. Removing the *footage* of a time range (a privacy or legal request) is a separate, admin-only "purge range" action.

**11. Roles beyond admin and viewer**
- *ZoneMinder:* None/View/Edit levels per area (Stream, Events, Control, Monitors, System…), per group and per monitor, plus named roles (`web/skins/classic/views/user.php`, `role.php`).
- *zmng:* admin or viewer. A viewer sees live and all recordings of their cameras, can archive and annotate, and cannot use PTZ.
- *Two combinations that matter here:*
  - **Live only** (no recordings). Are the CBA guests meant to review footage? Aaron's call.
  - **Operator:** can use PTZ, export, delete, and mark events, but cannot configure anything.
- *Proposal:* two flags on viewers, `can_review` and `can_operate`, rather than ZoneMinder's matrix.

**12. API tokens: per user, listed, revocable**
- *Today:*
  - only an admin can create a token, and it is that admin's own login, valid for 10 years (`create_token`, `zmng/src/api.rs`);
  - there is no list and no revoke, short of deleting the user.
- *The federation instructions don't work:* the README tells you to create the peer's token for a **viewer** account, but the API cannot issue one. Every peer and Home Assistant token is therefore a full admin credential.
- *ZoneMinder:* per-user API enable, revoke a user's tokens, revoke all tokens.
- *Proposal:* Admin → Users → Tokens: create a token for a chosen user with a name and an expiry, list tokens, revoke.

**13. Users change their own password**
- *ZoneMinder:* `ZM_USER_SELF_EDIT`.
- *zmng:* only admins can change passwords (`user_update` requires admin).
- Volunteers forget passwords and should not have to ask an admin to choose a new one.

**14. Retention by event kind, and an offsite copy**
- *ZoneMinder:* production runs exactly this kind of filter today ("Delete after 7 days low motion", `AlarmFrames < 2`). Filters can also upload events by FTP/SFTP and copy them to another storage.
- *zmng:* `event_retention_days` treats every event alike.
- *Proposal:*
  - Event retention by kind, for example people 90 d, vehicles 30 d, motion-only with less than 3 s of motion 7 d.
  - Optionally, copy clips and thumbnails of chosen kinds off site (rclone, SFTP or S3), so a stolen server does not take the evidence with it.

**15. Live wall extras**
- *ZoneMinder:* saved montage layouts; sound or popup on alarm.
- *zmNinjaNg:* Live Activity (only the cameras alarming now); layouts per group; kiosk lock; wake lock.
- *Proposal for zmng:*
  - a "cameras with motion now" filter on Live;
  - a fullscreen kiosk mode that hides the chrome and keeps the screen awake;
  - a tile order per user.
- This matters if a screen at the office or sound desk will show the cameras.

**16. Recording-gap report**
- *ZoneMinder:* `report_event_audit` (gaps and missing files per monitor over a date range).
- *zmng:* the coverage data is there (the timeline draws it), but nothing summarises it.
- *Proposal:* on Status, per camera, the gaps longer than a minute in the last 7 days. This answers "was it recording Tuesday at 2 AM?" before anyone needs the footage.

### P3

- **Event tags** (ZoneMinder, filterable in zmNinjaNg). Notes plus the star cover this for now.
- **Camera-side events as event sources.** ZoneMinder has an ONVIF pull-point listener. Hikvision line-crossing and intrusion detection, or the camera's human/vehicle detection on AcuSense models, could replace pixel motion on some cameras.
- **Adding cameras.**
  - ONVIF discovery and a stream-URL probe.
  - Clone a camera's settings.
  - Bulk-edit retention or detection for a group (ZoneMinder: add_monitors, onvifprobe, the monitors bulk edit).
- **Logs and audit.**
  - A "recent warnings" panel (ZoneMinder log view, zmNinjaNg Logs tab).
  - An audit trail of logins, exports and deletes (ZoneMinder logs admin changes at AUDIT level).
- **zmNinjaNg setup QR code** per user. ZoneMinder's user page has one; the app imports `{n,p,a,c,u}`.
- **zmNinjaNg extras:** dashboard and heatmap, "new since you last looked" badges, picture-in-picture, languages, themes, map, sound on alarm.
- **PTZ extras.**
  - Set presets from the pad (the API has `set_preset`; the pad does not).
  - Return home after an idle time.
  - PTZ for operators (with #11).
- **HLS for iPhones** without ManagedMediaSource (field review 11).

## Not carried over on purpose

- **Per-frame `Frames`/`Stats` rows and the frame viewer with per-zone statistics.** Replaced by the per-fragment motion index and the timeline histogram. The diagnostic use is covered by P1 #2.
- **Privacy zones that black out pixels, and timestamp overlays.** Neither is possible in passthrough recording without re-encoding. Use the cameras' own privacy masks and OSD (Hikvision has both). This deserves one line in `CUTOVER.md`, so nobody expects zmng to do it.
- **Linked monitors.** Continuous recording plus Review's synchronized cameras answers "what did the other cameras see". Linked *notifications* ("person at Door, send the Hall snapshot too") can be a rule in P1 #5 later.
- **Filters as a general query-and-action engine.** Replaced by policy (retention, tiering) plus the rules in P1 #5 and P2 #14. A general engine is not worth building for one site.
- **Email sent directly.** Use webhook → ntfy or Home Assistant. Add an SMTP sink only if the volunteers insist on email.
- **Not needed for 23 RTSP Hikvision cameras:**
  - run states and start/stop from the UI;
  - X10, dnsmasq;
  - V4L2/VNC/website/file sources;
  - encoder settings and "generate video" (every range is already an MP4);
  - JPEG saving;
  - 52 PTZ protocol drivers (ONVIF covers these cameras);
  - the AI training catalog;
  - telemetry and the donation prompt.

## zmNinjaNg compatibility at a glance

| App area | Works with zmng today |
|---|---|
| Login and refresh, monitors, groups, events list and filters, thumbnails, MP4 and HLS playback, ZMS live and event streams, go2rtc-style MSE live, event-server websocket, notification registration, console counts, load/disk/storage, users and permissions | yes |
| Live Activity page, alarm ring | **no**: alarm status is always idle (P1 #6) |
| Arm / disarm | **no**: no-op (P1 #6, P2 #7) |
| Monitor settings, Function, Enabled | **no**: fake "Saved" (P1 #6) |
| Zone overlay | **no**: empty (P1 #1, #6) |
| Detection picture (`objdetect`) | partly: thumbnail without boxes (P1 #6) |
| Push to the phone | **no**: registered, never sent (P1 #4) |
| Analysis-frames toggle | no; nothing to show until P1 #2 |
| Tags, run states, server logs, "Linked" scope in Nearby | empty or 404, which the app treats as unsupported (the Group scope in Nearby works) |

## Suggested order

1. **zmNinjaNg honesty fixes** (P1 #6). A few hours, no design needed.
2. **Incident bundle:** range and multi-camera export (P1 #3), digital zoom (P2 #8), quality choice (P2 #9). Mostly UI.
3. **Zone editor and tuning view** (P1 #1, #2). The largest remaining ZoneMinder strength, and the field review's open detection-quality item.
4. **Notification rules and the push route** (P1 #4, #5), with manual and external events (P2 #7).
5. **Roles, tokens, own password** (P2 #11–#13). Do these before the CBA guests and other volunteers get accounts at cut over.
6. The rest of P2 as time allows.
