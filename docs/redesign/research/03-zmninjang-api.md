# 03 - zmNinjaNg: what the mobile client expects from a ZoneMinder server

Research date: 2026-09-27. Source of truth: `github.com/ZoneMinder/zmNinjaNg` at commit
`0739b0d` (v2.6.0, merged 2026-09-27), cloned and grepped; file references below are
relative to `app/src/` in that repo unless noted. Web sources are listed at the end.

## 1. Which client is current in 2026

| | |
|---|---|
| Name | **zmNinjaNg** ("zmNinja Next Generation") |
| Repo | https://github.com/ZoneMinder/zmNinjaNg (canonical; `pliablepixels/zmNinjaNg` and `pliablepixels/zmNg` are the author's early mirrors) |
| Docs | https://zmninjang.readthedocs.io/ and https://zmninjang.zoneminder.com/ |
| Stack | React 19 + TypeScript + Vite; Capacitor 8 (iOS/Android); Electron (Win/macOS/Linux); Zod schemas on every API response; React Query; Zustand stores |
| Platforms | iOS, Android, Windows, macOS, Linux (AppImage/.deb), plain web |
| Release status | GA. Weekly-ish tagged releases: 2.2.1 (Aug 31), 2.3.0 (Sep 5), 2.4.0 (Sep 12), 2.5.0 (Sep 20), **2.6.0 (Sep 25 2026)**. Paid on App Store / Google Play (ZoneMinder org collects the fee), free binaries on GitHub Releases for Android/desktop. |
| License | Apache-2.0 |
| Min ZM | "ZoneMinder 1.36 or newer with API access enabled (`OPT_USE_API = 1`)" (docs/user-guide/faq.md). Feature gates at 1.37.61 and 1.38.0 (see section 4). |
| Relationship to zmNinja | Ground-up rewrite by the same author (pliablepixels). `ZoneMinder/zmNinja` README: "zmNinja is no longer being maintained. Please use zmNinjaNg"; zmNinja was declared EOL May 2026 and the repo archived 2026-06-14. |
| Relationship to ZM project | Lives under the ZoneMinder GitHub org; ZoneMinder "offers limited support, just like it did with zmNinja". Companion server: **zmeventnotificationNg / zmesNg** (ES 7.x, `ZoneMinder/zmeventnotificationNg`); the old `zmeventnotification` repo was archived 2026-05-19. ZM 1.39.2 added a native `Notifications` REST API (ZoneMinder PR #4685, merged 2026-03-07) so the app can do push without ES at all. |

Nobody should target legacy zmNinja in 2026: it is archived, its Ionic/Cordova stack is dead,
and the forum announcement (forums.zoneminder.com t=34288, "[2026] zmNinjaNG, zmEventNotification
7.0+, pyZM 2.0+") frames NG + ES7 + pyzmNg as the supported trio. The **protocol surface is the
same ZoneMinder CakePHP API**, so a shim written for NG also serves any leftover zmNinja installs.

## 2. Complete list of endpoints the client calls

All API paths are relative to `<apiUrl>` (= `<portalUrl>/api`). Every request carries
`?token=<access_token>` as a **query parameter** (never an `Authorization` header) - see
`api/client.ts` `request()`: `params.token = accessToken`. Bodies are
`application/x-www-form-urlencoded` (`client.postForm`/`putForm`), never JSON. Zod schemas strip
unknown keys, so extra fields are harmless; missing *required* keys fail validation but
`withFieldCatch`/`tolerantArray` (lib/zm/schema-tolerance.ts) make almost everything except
`Id`/`Name` optional-with-fallback.

### 2a. Discovery + login sequence (services/discovery.ts, services/profile-bootstrap.ts, docs/developer-guide/call-flows.rst Flow 1)

1. **Probe**: `GET <candidate>/api/host/getVersion.json` (no token) for each base candidate
   (`https://host/zm`, `https://host`, http variants). Any 2xx/401 with `{version, apiversion}` = found.
   Fallback probe: `GET /host/login.json` (`discovery.ts` `probeApi`).
2. `POST /host/login.json` form `user=&pass=` -> `LoginResponse` (`api/types.ts:42`):
   `access_token`, `access_token_expires` (**seconds**, converted to absolute ms in
   `stores/auth.ts:473`), `refresh_token`, `refresh_token_expires`, `credentials`,
   `append_password`, `version`, `apiversion`. **All optional.** If neither `access_token` nor
   `refresh_token` is present the profile is marked no-auth (`stores/auth.ts:453`) and no token is
   ever sent. `credentials`/`append_password` are parsed but unused (no legacy `auth=` hash path).
3. `GET /configs/viewByName/ZM_PATH_ZMS.json?token=` -> `{config:{Value:"/zm/cgi-bin/nph-zms"}}`;
   cgiUrl = `origin + Value` else `portalUrl + '/cgi-bin/nph-zms'`.
4. Bootstrap (each wrapped, failures non-fatal), in order:
   `GET /servers.json` (multi-server map: `Server.Id, Name, Hostname, Protocol, Port, PathToIndex,
   PathToZMS, PathToApi`; rows with empty/`localhost` Hostname or Port<=0 are ignored,
   lib/zm/server-resolver.ts) -> `GET /host/getTimeZone.json` -> `{DateTime:{TimeZone:"America/New_York"}}`
   (accepts `TimeZone|Timezone|timezone|tz` at root or nested) -> `GET /configs/viewByName/ZM_PATH_ZMS.json`
   -> `GET /configs/viewByName/ZM_GO2RTC_PATH.json` (empty Value = no go2rtc) ->
   `GET /configs/viewByName/ZM_MIN_STREAMING_PORT.json` (empty = single port; else stream port = Value + MonitorId).
5. `GET /users.json` once, to derive permissions (`api/users.ts`): reads `User.Username, System,
   Monitors, Stream, Events, Control, Groups`. A **401 here is expected** and means System=None.
6. **Refresh**: `POST /host/login.json` form `token=<refresh_token>` -> same LoginResponse.
   Proactive refresh 5 min before `access_token_expires` (`ZM_INTEGRATION.accessTokenLeewayMin`),
   and any 401 whose body is not a permission refusal triggers refresh -> relogin -> logout
   (`client.ts` `recoverFromAuthFailure`). Permission refusals are detected by body text
   (lib/permissions/permission-error.ts). There is **no `/host/logout.json` call** anywhere.
7. Periodic while app open: `GET /host/getVersion.json` (`version`, `apiversion` drive feature gates).

### 2b. Monitors (api/monitors.ts)

- `GET /monitors.json` -> `{monitors:[{Monitor:{...}, Monitor_Status:{...}}]}` polled every 20 s
  (40 s low-bandwidth). Fields the UI actually reads (grep count across src): `Monitor.Id, Name,
  Width, Height, Orientation, Function, Capturing, Analysing, Recording, Decoding, Enabled, Type,
  Controllable, ControlId, ServerId, Sequence, Deleted, LinkedMonitors, Path, Options,
  StreamChannel, RTSPServer, Go2RTCEnabled, Go2RTCType, RTSP2WebEnabled, JanusEnabled, DefaultPlayer`;
  `Monitor_Status.Status ('Connected'), CaptureFPS, AnalysisFPS, CaptureBandwidth`.
  Run state (lib/monitor/monitor-status.ts): ZM>=1.38 uses `Capturing!='None'` and
  `Analysing!='None'`, else `Function`; `Status!=='Connected' || CaptureFPS==0` -> offline.
  Schema (`api/types.ts:148-259`) lists ~110 columns; all coerced to string, `Id`/`Name` required.
- `GET /monitors/<id>.json` -> `{monitor:{Monitor, Monitor_Status}}`.
- `POST /monitors/<id>.json` form `Monitor[Function]=`, `Monitor[Enabled]=0|1`,
  `Monitor[Capturing|Analysing|Recording]=` -> `{"message":"Saved"}`.
- `GET /monitors/alarm/id:<id>/command:on|off|status.json` -> `{status, output}` or
  `{status:'false', code, error}`; status polled every 5 s per visible monitor.
- `GET /monitors/daemonStatus/id:<id>/daemon:zmc|zma.json` -> `{status:'ok', statustext}`.
- `GET /controls/<controlId>.json` -> `{control:{Control:{Can*/Has* flags, NumPresets}}}` (PTZ).
- PTZ: `GET <portal>/index.php?view=request&request=control&id=<mid>&control=<cmd>[&preset=N][&xge=0&yge=0]&token=`.
- `GET /zones.json?MonitorId=<id>` -> `{zones:[{Zone:{Id, MonitorId, Name, Type, Units, Coords, ...}}]}`
  (note: **not** `/zones/forMonitor/`). `Coords` "x,y x,y ..." in px or percent (<=100).
- `GET /groups.json` -> `{groups:[{Group:{Id,Name,ParentId}, Monitor:[{Id}]}]}`.

### 2c. Events (api/events.ts, api/types.ts:381-446)

- List: `GET /events/index[/<seg>...].json?page=N&limit=100&sort=StartDateTime&direction=desc`.
  Segments are URL-encoded path parts, one per condition, exactly these forms:
  `MonitorId:<id>` (repeated per monitor), `StartDateTime >=:YYYY-MM-DD HH:MM:SS`,
  `StartDateTime <=:...`, `StartDateTime >:...`, `StartDateTime <:...`, `EndDateTime <=:...`,
  `AlarmFrames >=:N`, `Notes REGEXP:<re>`, `Cause REGEXP:<re>`, `Cause NOT REGEXP:<re>`,
  `Archived:1`, `Id IN:1,2,3` (<=200 ids), `Tags.Id:<id>`. Datetimes are **server-local wall
  clock, space separator, no zone** (`lib/time.ts` `formatForServerInTz` renders the instant in
  the profile's `getTimeZone` value). Client fetches up to 10 pages x 100 to fill its own limit.
- Expected response: `{events:[{Event:{...}}], pagination:{page, current, count, pageCount,
  prevPage:bool, nextPage:bool, limit, totalCount?}}`. `count` = total matching rows (used for
  "N new events" badges via `limit=1&sort=StartDateTime&direction=desc`); `nextPage` drives paging.
- Event fields read: `Id, MonitorId, Name, Cause, Notes, StartDateTime, EndDateTime (nullable),
  Length (seconds, float string), Frames, AlarmFrames, AlarmFrameId, MaxScoreFrameId,
  MaxScore, AvgScore, Width, Height, Orientation, DefaultVideo, Videoed, SaveJPEGs, Archived,
  StorageId, DiskSpace`. Required by schema: `Id`, `Name`. StartDateTime is parsed with
  `new Date(s.replace(' ','T'))` (EventDetail.tsx:223) i.e. **as browser-local** unless the
  timeline converts it. `DefaultVideo` ending in `.m3u8` selects the HLS player.
- `GET /events/<id>.json` -> `{event:{Event:{...}}}`. ZM also embeds `Frame:[...]`; the client
  ignores it (schema has no Frame key; EventFrameCarousel uses fid = `snapshot|alarm|objdetect|N`).
- `PUT /events/<id>.json` form `Event[Archived]=1|0` -> `{"message":"Saved"}`. No notes editing.
- `DELETE /events/<id>.json`.
- `GET /events/consoleEvents/<interval>.json` (`"1 hour"`, `"1 day"`) -> `{results:{<mid>:count}}`
  (dashboard widget; marked "needs verifying against live ZM" in source).
- `GET /tags.json`, `GET /tags/index/Events.Id:1,2,3.json` -> `{tags:[{Tag:{Id,Name,...},
  Events_Tags:{EventId}}]}`; 404 = tags unsupported (fine to omit).
- Thumbnails: `GET <portal>/index.php?view=image&eid=<id>&fid=<snapshot|alarm|objdetect|N>&width=W[&height=H]&token=`
  (lib/zm/url-builder.ts `getEventImageUrl`); widths 200 (cards), 320x240, 600 (notifications).
  Default fid chain is configurable; `snapshot` first, then `alarm`/`objdetect`.
- MP4 playback: `GET <portal>/index.php?view=view_video&mode=mp4&format=h264&eid=<id>&token=`.
  Must be a complete file with `Content-Length` + `Accept-Ranges: bytes` (Chromium rejects an
  unbounded remux; comment in url-builder.ts:265). HLS variant:
  `index.php?view=view_event_hls&mode=hls&eid=`. Downloads reuse the same URL.
- MJPEG event playback (ZMS player, used when no video or "force ZMS"):
  `GET <cgi>/nph-zms?mode=jpeg&source=event&event=<id>&frame=1&rate=100&maxfps=30&replay=single&scale=100&connkey=<rand 0..999999>&token=`
  then control via `GET <portal>/index.php?view=request&request=stream&connkey=<k>&command=<n>[&offset=s][&rate=r]&token=`
  with `ZMS_COMMANDS` (lib/zm/zm-constants.ts): 1 pause, 2 play, 3 stop, 4/5/6/7 fwd/rev,
  11 scale, 12 prev, 13 next, **14 seek(offset)**, **15 varplay(rate)**, 16 getImage,
  **17 quit**, 18 maxfps, 19/20 analyse on/off, **99 query**. Query is polled every 3 s and the
  JSON reply must include a progress/frame position and duration (the player reads
  "streamDuration" and fraction played; a missing process is treated as "stream gone").

### 2d. Live view

- MJPEG stream: `GET <cgi>/nph-zms?monitor=<id>&mode=jpeg&scale=<pct>&maxfps=<n>&buffer=<n>&connkey=<int>&token=`
  (`getMonitorStreamUrl`). Scale 100/50/40 by view; `maxfps` from bandwidth profile; multi-port
  = `minStreamingPort + monitorId`. On unmount/hide the client sends command 17 (quit) for the
  connkey - a shim must accept and 200 that call even if it has no such process.
- Snapshot mode: `mode=single` (`&_t=<cacheBuster>`), or for ZM >= 1.37.61 with `Decoding=Ondemand`
  `mode=jpeg&frames=1` (one JPEG then EOF). Refresh every 3 s (10 s low bandwidth).
- go2rtc/WebRTC/MSE/HLS (hooks/useGo2RTCStream.ts, vendored go2rtc `video-rtc.js`): enabled only
  when `Monitor.Go2RTCEnabled` is true **and** `ZM_GO2RTC_PATH` is non-empty. Signalling URL =
  `ZM_GO2RTC_PATH` with http->ws, `+ /ws?src=<mid><suffix>[&token=]`; suffix from `StreamChannel`:
  `''`, `_CameraDirectPrimary`, `_CameraDirectSecondary`, `_ZoneMinderPrimary` (only if
  `RTSPServer` on). The element opens webrtc+mse(+hls) in parallel over one go2rtc websocket and
  keeps whichever wins. MJPEG is always the fallback. RTSP2Web/Janus flags are parsed but no
  player exists for them.
- No `auth=` hash handling anywhere: `ZM_OPT_USE_AUTH` off -> no token; on -> JWT `token=` only.
  `ZM_AUTH_RELAY` is irrelevant. `nph-zms` and `index.php?view=image|view_video` must therefore
  accept the same JWT `token=` query param as the API (ZM 1.34+ behaviour).

### 2e. Server / misc pages

`GET /host/daemonCheck.json` -> `{result:1}` (30 s poll); `/host/getLoad.json` -> `{load:[1,5,15]}`;
`/host/getDiskPercent.json` -> `{usage:{Total:{space,color},...}}` or `{usage:n}`;
`/storage.json` -> `{storage:[{Storage:{Id,Name,Path,Type,DiskSpace,...}}]}`;
`/states.json` -> `{states:[{State:{Id,Name,Definition,IsActive}}]}` and
`POST /states/change/<Name>.json`; `/configs.json` (bulk, rarely); `/logs.json?limit&page` and
`/logs/index/Component:<c>.json` -> `{logs:[{Log:{...}}], pagination}`;
`/notifications.json` (see section 3). Not used: `/api/host/logout.json`, `/frames`, `/filters`,
`/zones/forMonitor`, `/monitors/alarm` beyond on/off/status, `daemonControl`.

## 3. Event notification / push

The client has two mutually exclusive modes per profile (`types/notifications.ts` `NotificationMode`):

**ES mode** (services/notifications.ts) - the classic zmeventnotification websocket protocol,
unchanged in ES 7 ("zmesNg"). Client opens `ws[s]://<host>:<port=9000><path=/>` and sends:

```json
{"event":"auth","data":{"user":"u","password":"p","appversion":"2.6.0"},"category":"normal"}
```
expects `{"event":"auth","type":"","status":"Success","version":"7.0.x"}` within 20 s
(else `status:"Fail","reason":"BADAUTH|NOAUTH"`). Then:
- keepalive `{"event":"control","data":{"type":"version"}}` every 60 s (120 s low-bw); any
  `{"event":"control","type":"version",...}` reply counts as alive.
- filter `{"event":"control","data":{"type":"filter","monlist":"1,2","intlist":"0,60"}}`.
- push registration `{"event":"push","data":{"type":"token","token":"<fcm>","platform":"ios|android","monlist":"..","intlist":"..","state":"enabled|disabled","profile":"<profile name, <=128>"}}`
  and badge `{"event":"push","data":{"type":"badge","badge":0}}`.
- Server alarm (what the client parses, `_handleMessage`):
  `{"event":"alarm","type":"","status":"Success","events":[{"MonitorId":1,"MonitorName":"Front","EventId":123,"Cause":"Motion: All","Name":"Front","DetectionJson":[],"Picture":"<url>"}]}`;
  if `Picture` is absent the client builds `index.php?view=image&eid=&fid=snapshot&width=600&token=`.
  `mock-notification-server.js` in the repo is the exact reference implementation.
- FCM data payload the app reads (services/pushNotifications.ts `PushNotificationData`):
  ES form `mid, eid, monitorName, cause, notification_foreground, profile`; ZM-direct form
  `MonitorId, EventId, MonitorName, Cause`; plus `title`/`body`. ES sends via a Google-hosted
  FCM v1 cloud-function relay (`fcm.fcm_v1_url`/`fcm_v1_key` "managed zmNinjaNG defaults"); the
  app id/credentials are the store builds', so **a third-party server cannot push to the store
  app except through that relay**.

**Direct mode** (api/notifications.ts, ZM >= 1.39.2): no ES. Client checks
`GET /notifications.json` (404 = unsupported, greys out the option), then
`POST /notifications.json` form `Notification[Token]=`, `[Platform]=android|ios|web`,
`[MonitorList]=1,2`, `[Interval]=60`, `[PushState]=enabled`, `[AppVersion]=`, `[Profile]=`
-> `{notification:{Notification:{Id,UserId,Token,Platform,MonitorList,Interval,PushState,AppVersion,BadgeCount,LastNotifiedAt,CreatedOn,UpdatedOn}}}`;
`PUT /notifications/<id>.json` (`Token` is unique server-side; re-POST = HTTP 500), `DELETE`.
Push is then sent server-side (in ZM's ecosystem by `zm_detect`/pyzmNg via the same FCM relay).
On desktop, direct mode simply polls `GET /events/index.json?limit=5&sort=StartDateTime&direction=desc`
(optionally `/Notes REGEXP:detected:`) every 30/60 s and toasts new Ids (services/eventPoller.ts).

Net: zmNinjaNg still speaks the ES websocket protocol verbatim, and additionally the new ZM REST
registration API. Either one satisfies it; the websocket path is the only one that yields
*real-time desktop* notifications.

## 4. Version/config checks, timezone, timestamp formats

- Reads `version`/`apiversion` from login and `/host/getVersion.json`; parses `major.minor.patch`
  (lib/zm/zm-version.ts). Gates: `>=1.38.0` -> use `Capturing/Analysing/Recording` instead of
  `Function` (monitor-status.ts, MonitorSettingsDialog, MonitorControlsCard); `>=1.37.61` ->
  zms understands `frames=`; `Decoding` field present from 1.37. **Report `version` as
  `"1.38.x"` or higher** to get the modern code paths, `apiversion` e.g. `"2.0"`.
- Config keys fetched via `configs/viewByName`: `ZM_PATH_ZMS`, `ZM_GO2RTC_PATH`,
  `ZM_MIN_STREAMING_PORT` only. `ZM_OPT_USE_API`, `ZM_OPT_USE_AUTH`, `ZM_AUTH_HASH_LOGINS`
  are never queried; auth-on/off is inferred from whether login returned tokens.
- Timezone: `/host/getTimeZone.json` value is stored per profile and used *only* to render
  filter bounds for `StartDateTime`/`EndDateTime` segments (`yyyy-MM-dd HH:mm:ss`, no zone).
  Event timestamps coming back are treated as naive local strings; in aggregate ("All servers")
  mode they are re-interpreted in the profile timezone for sorting. So: **serve all datetimes
  as `YYYY-MM-DD HH:MM:SS` in one consistent zone and return that zone from getTimeZone**.
- `Length` seconds as string float; `Frames`/`AlarmFrames` integers as strings; booleans as
  `"0"/"1"` strings (`Archived`, `Enabled`, `Videoed`); `Go2RTCEnabled` is coerced boolean.
- Token expiry fields are **seconds relative to now**, not epoch.

## 5. Recommended minimal shim for zmng

Serve everything under one origin `https://zmng/zm/` so discovery lands on `.../zm/api`:

1. Auth: `POST /zm/api/host/login.json` (form user/pass or `token=` refresh) returning
   `{access_token, access_token_expires:3600, refresh_token, refresh_token_expires:86400*7,
   credentials:"auth=...", append_password:0, version:"1.38.5", apiversion:"2.0"}`. Accept the
   JWT as `?token=` on **every** path incl. cgi-bin and index.php. `GET /zm/api/host/getVersion.json`.
2. Host: `getTimeZone.json` (`{DateTime:{TimeZone:"<IANA>"}}`), `daemonCheck.json` (`{result:1}`),
   `getLoad.json`, `getDiskPercent.json`; `configs/viewByName/{ZM_PATH_ZMS,ZM_GO2RTC_PATH,
   ZM_MIN_STREAMING_PORT}.json` (`{config:{Value}}`); `servers.json` -> `{servers:[]}`;
   `users.json` (one row with `System:'Edit'` etc. or 401); `states.json` -> `{states:[]}`;
   `groups.json` -> `{groups:[]}`; `storage.json`; `tags.json` -> 404; `notifications.json` (see 4).
3. Monitors: `monitors.json` / `monitors/<id>.json` with `Monitor{Id,Name,Type:'Ffmpeg',
   Function:'Mocord',Capturing:'Always',Analysing:'Always',Recording:'Always',Decoding:'Always',
   Enabled:'1',Width,Height,Orientation:'ROTATE_0',Controllable:'0',ControlId:null,ServerId:null,
   StorageId:'1',Sequence,Deleted:false,StreamChannel:null,RTSPServer:'0',Go2RTCEnabled,
   Go2RTCType:null,RTSP2WebEnabled:false,JanusEnabled:false,DefaultPlayer:null,
   LinkedMonitors:null,Path,Options:null,...}` (fill remaining schema columns with `null`/`"0"`)
   and `Monitor_Status{MonitorId,Status:'Connected',CaptureFPS:'15.0',AnalysisFPS:'15.0',
   CaptureBandwidth:'0'}`. `monitors/alarm/id:X/command:status.json` -> `{status:0,output:0}`;
   `zones.json?MonitorId=` -> `{zones:[]}`; `POST monitors/<id>.json` -> `{"message":"Saved"}`.
4. Live: `cgi-bin/nph-zms` MJPEG (`multipart/x-mixed-replace`) honoring `monitor, scale, maxfps,
   mode=single|jpeg, frames=1`; `index.php?view=request&request=stream&command=17|99` -> 200 JSON.
   Better path: set `ZM_GO2RTC_PATH` to our own go2rtc-compatible WS endpoint (`/ws?src=<mid>`),
   set `Go2RTCEnabled:true`, and the app plays H.264/HEVC via WebRTC/MSE with MJPEG fallback -
   this is the only way to get full-res HEVC into the client without transcoding to JPEG.
5. Events - **the mapping point for our absolute-time segment model**. Synthesize one ZM
   "Event" per zmng event row (or per fixed-length chunk for continuous recording):
   `Id` = our event id, `MonitorId`, `Name:"Event-<id>"`, `Cause` (motion/person/...),
   `Notes` (detection labels, `"detected:person"` keeps the "only detected" filter working),
   `StartDateTime`/`EndDateTime` in the reported zone, `Length` seconds, `Frames` = fps*len,
   `AlarmFrames`, `MaxScoreFrameId`/`AlarmFrameId` = frame index of best thumbnail,
   `MaxScore/AvgScore/TotScore`, `Width/Height`, `DefaultVideo:"<id>-video.mp4"`, `Videoed:'1'`,
   `SaveJPEGs:'0'`, `Archived:'0'`, `StorageId:'1'`, `DiskSpace`. Implement the path-segment
   filter grammar (MonitorId, StartDateTime/EndDateTime with `>=,<=,>,<`, AlarmFrames, Notes/Cause
   REGEXP, Archived, Id IN) and `page/limit/sort/direction` with a full `pagination` object.
   `index.php?view=image&eid&fid=snapshot|alarm|objdetect|N&width` -> JPEG from our keyframe
   store. `index.php?view=view_video&eid&mode=mp4` -> **our range mp4 (start..end of that event),
   complete file with Content-Length + Accept-Ranges**; HEVC-in-MP4 plays natively on iOS/macOS;
   for Android/Chromium set `DefaultVideo` to an `.m3u8` and serve `view_event_hls` fMP4 HLS.
   `PUT` archive and `DELETE` map to our event flags. `nph-zms?source=event` can return 404;
   the app falls back to MP4 when `DefaultVideo` is set, and only needs ZMS when it is empty.
6. Push: implement `GET/POST/PUT/DELETE /zm/api/notifications.json` (ZM 1.39.2 shape) to collect
   FCM tokens, then deliver `data:{EventId,MonitorId,MonitorName,Cause,profile}` messages through
   the zmNinjaNg FCM relay (the store app's Firebase project is not ours; contact pliablepixels/ES
   maintainers for the relay key, or ship a rebuilt app with our own `google-services.json`).
   Optionally expose a websocket on :9000 speaking the ES JSON above for desktop real-time toasts.

Cost estimate: ~25 JSON routes with fixed shapes, one MJPEG/JPEG endpoint, one MP4/HLS range
endpoint, and the event filter grammar. Everything else can return empty lists or 404.

## Sources

- https://github.com/ZoneMinder/zmNinjaNg (README, docs/user-guide/faq.md, docs/user-guide/notifications.md, docs/developer-guide/07-api-and-data-fetching.rst, call-flows.rst, go2rtc-integration.rst, 13-network-endpoints.rst; app/src/api/*.ts, lib/zm/url-builder.ts, lib/zm/zm-constants.ts, services/notifications.ts, services/pushNotifications.ts, services/discovery.ts, app/mock-notification-server.js)
- https://github.com/ZoneMinder/zmNinjaNg/releases (2.2.1 - 2.6.0)
- https://github.com/ZoneMinder/zmNinja (archived 2026-06-14, "no longer being maintained")
- https://github.com/ZoneMinder/zmeventnotificationNg and https://zmeventnotificationng.readthedocs.io/en/latest/ (guides/developers.html, guides/config.html)
- https://github.com/ZoneMinder/zoneminder/pull/4685 (Notifications table + REST API, zm_update-1.39.2.sql)
- https://forums.zoneminder.com/viewtopic.php?t=34288 ([2026] zmNinjaNG, zmEventNotification 7.0+, pyZM 2.0+)
- https://zmninjang.readthedocs.io/en/latest/user-guide/getting-started.html
