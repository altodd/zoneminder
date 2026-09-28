# 02 — Web layer review: browsing and viewing events

Scope: the PHP/JS path a user takes when they "look for events" — the events
list, thumbnails, the event viewer, the frames list, the timeline, montage
review and the CakePHP API behind them. Fork under review:
`/Users/atodd/Zoneminder-redesign/zoneminder` (1.39.35). Live measurements
were taken read-only against the production box (ZoneMinder 1.38.4, Ubuntu
24.04, Apache 2.4 prefork + mod_php 8.3, MySQL 8.0, 23 cameras, HEVC
2688x1520 passthrough MP4, `Scheme=Medium`, both event volumes 94 % full,
load average ~18–20 on 48 threads while measuring). Every fork file examined
differs from its production counterpart (md5 mismatch on all 14 checked), but
the hot paths below are structurally identical in both; where production
differs in a way that matters it is called out.

The user's complaint — "when I'm looking for events it's almost unusable" — is
reproducible and has a small number of concrete causes. The single worst one
is a hard failure, not slowness: with the default sort and no date window the
events list request dies with a PHP memory fatal and the table sits on
"Loading…" forever.

---

## (a) Measured timings (live server, 2026-09-27 ~17:20–17:35 local)

All via `curl` from the LAN with a logged-in web session (form login, then
`ZMSESSID` cookie); API calls use the JWT `token=`. `ttfb` = time to first
byte, `total` = full transfer. Load average during the runs: 18.5–19.

| # | Request | Rows / size | ttfb | total | Notes |
|---|---------|-------------|------|-------|-------|
| 1 | `index.php?view=login` (bare bootstrap + 302) | 0 B | — | **0.07 s** | Floor: config from DB, DB session, auth, redirect |
| 2 | `view=request&request=status&entity=monitor` (bare AJAX) | 160 B | 0.28 | 0.28 | |
| 3 | `view=console` HTML | 39 KB | 0.45–0.48 | 0.50–0.53 | |
| 4 | `view=events` HTML | 39 KB | 0.45–0.53 | **0.50–0.59** | Navbar alone ≈ 0.2 s (see #5) |
| 5 | `view=events&navbar=0` | 34 KB | 0.30 | 0.33 | Confirms navbar cost |
| 6 | events AJAX, last **1 day**, limit 25 | 2,997 matched, 43 KB | 0.18–0.23 | **0.23–0.28** | |
| 7 | events AJAX, last 1 day, MonitorId=10 | 145 matched | 0.09–0.13 | 0.14–0.18 | |
| 8 | events AJAX, last **7 days** | 20,859 matched | 0.84–1.04 | **0.89–1.09** | still returns 25 rows |
| 9 | events AJAX, last **30 days** | 37,938 matched | 1.42–1.54 | **1.47–1.59** | |
| 10 | events AJAX, 30 days, offset=2500 (page 101) | 37,937 | 1.35–1.46 | 1.40–1.51 | same cost as page 1 |
| 11 | events AJAX, **no date term**, `sort=StartDateTime&order=asc` (ZM defaults) | 59,619 | — | **HTTP 500 @ 1.7 s** | `PHP Fatal: Allowed memory size of 134217728 bytes exhausted … database.php` |
| 12 | thumbnail 48 px via `:10010` (per-monitor port), first request | 2,206 B | 0.13 | 0.13 | body is the JPEG **twice** (see RC-3) |
| 13 | same thumbnail, cached scaled file | 1,103 B | 0.06–0.08 | 0.06–0.08 | 25 of these per page, over up to 23 ports |
| 14 | thumbnail via `:80` | 1,332 B | 0.08 (0.13 first) | | |
| 15 | full `snapshot.jpg` (no scale) | 640 KB | 0.07 | 0.23 | what the hover overlay background loads |
| 16 | thumbnail of a March-2026 orphan on the overflow volume | 1,244 B | 0.07–0.11 | | |
| 17 | `view=event&eid=…` HTML | 40 KB | 0.45–0.55 | **0.50–0.60** | |
| 18 | `request=status&entity=frames&id=…` (alarm-cue graph), 1,838 Frames rows | 124 KB | 0.08 | 0.16 | |
| 19 | same, 91-row event | 6 KB | 0.06 | 0.06 | |
| 20 | `view=frames&eid=…` HTML | 26 KB | 0.40–0.46 | 0.43–0.49 | |
| 21 | frames AJAX, limit 25 / all 91 | 13 KB / 45 KB | 0.07–0.10 | 0.10–0.13 | |
| 22 | `view=timeline` (no filter) | 0 B | — | **HTTP 500 @ 0.07–0.11 s** | `Call to a member function sql() on null` timeline.php:213 (prod) |
| 23 | `view=timeline`, 1-hour window (~145 events) | **399 KB HTML** | 1.05 | **1.20** | |
| 24 | `view=montagereview`, 1-hour window | 66 KB | 0.46–0.49 | 0.51–0.54 | page only; tiles are #25 |
| 25 | `nph-zms?mode=single&event=…&time=…&scale=50` (one montage tile, HEVC) | 207 KB JPEG | 0.21–0.25 | **0.75–1.37 s** | one zms process, one HEVC decode |
| 26 | `api/events/index/StartDateTime>=…json?page=1` (1 d / 30 d) | 20 events, 16 KB | 0.08–0.10 | 0.10–0.13 | proper SQL LIMIT |
| 27 | `api/events/<id>.json` (event with 1,838 Frames) | **288 KB** | 2.67–2.93 | **2.78–3.04 s** | `recursive=1` pulls every Frame |
| 28 | `api/frames/index/EventId:a/EventId:b/EventId:c.json` | 2,003 rows, 329 KB | 0.10–0.12 | 0.22–0.24 | unpaginated; montage review batches these |
| 29 | `api/monitors.json` | 81 KB | 0.08–0.11 | 0.14–0.17 | |
| 30 | `view=view_video` HEAD / 64 KB range | | 0.16 / 0.07 | 0.16 / 0.12 | `HTTP/1.0 206`, `Connection: close`, `Cache-Control: no-store` |
| 31 | `zmdc.pl check` (spawned by every HTML page for the navbar) | | | **0.20–0.22 s** | perl process per page view |

MySQL: `Slow_queries = 87` over 10.6 days (slow log off, `long_query_time=10`).
`innodb_buffer_pool_size = 128 MB` (MySQL default) against a 34 GB Frames
table + 8 GB Stats table; `Innodb_buffer_pool_pages_free = 1024/8192`.
`Created_tmp_disk_tables = 0`. InnoDB stats for Frames are stale
(`rows=74` per EventId estimate vs 91–1,838 real; last update 2026-09-26).

Data-shape facts that drive everything below (from the live DB):

- Events: 59,619 rows, 27 MB. Last 1 d = 2,983 events, 7 d = 20,844, 30 d = 37,921. Almost all are 600 s continuous-recording chunks (`Frames≈9,000`, `MaxScore=0`), ~145 per camera per day.
- **1,379 events have `EndDateTime IS NULL`; 1,356 of them are older than 1 h** (crash orphans, e.g. a cluster from 2026-03-18). All 1,379 also have `DiskSpace IS NULL`.
- Frames rows per event vary wildly: a 9,010-frame no-alarm event has **91** rows (bulk rows), a 3,725-frame event with 203 alarm frames has **1,838** rows. Per day the Frames table gains ~700 k alarm rows + bulk rows (`SUM(AlarmFrames)` for the last day = 693,691).
- `snapshot.jpg` (388–640 KB, full resolution) exists next to every recent event's MP4, written by zmc at event start — so list thumbnails are **not** generated by ffmpeg on this install (see RC-3 for when they are).
- Apache listens on `:80` and `:10000–10079`; thumbnails and streams go to `ZM_MIN_STREAMING_PORT + MonitorId` (`:10001…10023`).

---

## (b) The events-list SQL and its EXPLAIN

Built in `web/ajax/events.php:239-261` (identical shape in production
`ajax/events.php:234-245`). For the 1-day request the browser issues
(`events.js:40-55`, POST with `sort=StartDateTime&order=desc&limit=25&offset=0`
+ the filter terms as querystring) the exact statement is:

```sql
SELECT
  E.*,
  UNIX_TIMESTAMP(E.StartDateTime) AS StartTimeSecs,
  CASE WHEN E.EndDateTime IS NOT NULL THEN E.EndDateTime
       WHEN E.Length > 0 THEN DATE_ADD(E.StartDateTime, INTERVAL FLOOR(E.Length) SECOND)
       ELSE E.StartDateTime END AS EndDateTime,
  CASE WHEN E.EndDateTime IS NOT NULL THEN UNIX_TIMESTAMP(E.EndDateTime)
       WHEN E.Length > 0 THEN UNIX_TIMESTAMP(E.StartDateTime) + E.Length
       ELSE UNIX_TIMESTAMP(E.StartDateTime) END AS EndTimeSecs,
  M.Name AS Monitor,
  GROUP_CONCAT(T.Name SEPARATOR ", ") AS Tags
FROM `Events` AS E
INNER JOIN Monitors AS M ON E.MonitorId = M.Id
LEFT JOIN Events_Tags AS ET ON E.Id = ET.EventId
LEFT JOIN Tags AS T ON T.Id = ET.TagId
WHERE ( E.StartDateTime >= '2026-09-26 17:22:00' )
GROUP BY E.Id, Monitor
ORDER BY E.StartDateTime DESC
-- NO LIMIT, NO OFFSET (only a saved-filter ->limit() is ever appended, line 263-265)
```

`EXPLAIN` on production (1-day window):

```
id  table  type   key                       rows  Extra
1   E      range  Events_StartDateTime_idx  2984  Using index condition; Using temporary; Using filesort
1   M      eq_ref PRIMARY                   1
1   ET     ref    Events_Tags_ibfk_2        1     Using index
1   T      eq_ref PRIMARY                   1
```

`EXPLAIN FORMAT=TREE`: `Sort: E.StartDateTime DESC -> Stream results ->
Group aggregate: group_concat(...) -> Sort: E.Id, M.Name -> Nested loop left
join (cost=6714 rows=2984)`. Two sorts and a temporary table for a result of
which PHP keeps 25 rows.

With no date term (what `ZM_WEB_EVENT_SORT_FIELD=StartTime / asc` gives
without cookies, and what the table's "All" page-size option or a cleared
date field gives):

```
id  table  type  key   rows   Extra
1   E      ALL   NULL  57916  Using temporary; Using filesort
```

The query itself is not the slow part — MySQL streams 38 k rows in well under
a second. The cost is what `queryRequest()` does with them:

- `web/ajax/events.php:283-293` — `while ($row = dbFetchNext(...)) { $unfiltered_rows[] = $row; }` buffers **every** matching row (all 26 columns + 5 computed) into PHP.
- `:346-349` — `array_slice($filtered_rows, $offset, $limit)` is the pagination.
- `:395-400` — `total` is `count($unfiltered_rows)`, i.e. the row count is obtained by fetching the rows.
- `:403-408` — footer totals iterate the full set again.

At ~2 KB per row, 59 k rows exceed `memory_limit = 128M`
(`/etc/php/8.3/apache2/php.ini`) → the fatal in row #11 of the table. Even
when it fits, page 101 costs the same 1.5 s as page 1 (row #10).

Tags on this install: `Events_Tags` and `Tags` are both **empty**, yet every
list query pays the two LEFT JOINs and the `GROUP BY` they require.

---

## (c) Root causes, ranked by impact

### RC-1 — Events list fetches the whole result set into PHP; no SQL LIMIT/OFFSET/COUNT

Evidence: `web/ajax/events.php:255-299, 346-349, 395-408` (prod `:234-245, 279, 329`);
rows #8–#11 in the timings table; Apache error log
`PHP Fatal error: Allowed memory size of 134217728 bytes exhausted … includes/database.php` at 17:25:19 and 17:25:21 (my two runs of the unfiltered list).

Effect on the user: any window wider than ~7 days costs 1–1.6 s per page turn
and per bootstrap-table refresh (`events.js` refreshes on `pageshow`, on every
filter change and on every column sort); anything ≥ ~45 k rows is a silent
500 and the table's "Loading, please wait" never clears (the fork added an
alert at `events.js:75-84`; production shows nothing). The `data-page-list`
in `views/events.php:156` still offers **All**. Sorting server-side on a
column other than the indexed `StartDateTime` (e.g. `DiskSpace`, `Length`)
adds a filesort over the full window.

Secondary: `GROUP BY E.Id, Monitor` + two LEFT JOINs for a tag feature that
has zero rows here (`Events_Tags`=0, `Tags`=0) — "Using temporary; Using
filesort" on every list query.

### RC-2 — Per-row work in the list: `DiskSpace()` walks the filesystem for every NULL-DiskSpace event, every render

Evidence: `web/ajax/events.php:388` calls `$event->DiskSpace()` unconditionally
(`ZM_WEB_EVENT_DISK_SPACE` is never consulted here); `web/includes/Event.php:352-364`:

```php
if ( (!property_exists($this, 'DiskSpace')) or (null === $this->{'DiskSpace'}) ) {
  $this->{'DiskSpace'} = folder_size($this->Path());
  ...
  #dbQuery('UPDATE Events SET DiskSpace=? WHERE Id=?', ...);   // commented out
```

`folder_size()` (`web/includes/functions.php`) is a recursive `glob()` +
`filesize()`. 1,379 events on this box have `DiskSpace IS NULL` (the 1,356
crash orphans plus the 23 in-progress ones); each time one lands in a page the
directory is walked again because the result is never persisted. With
`ZM_WEB_EVENT_SORT_ORDER=asc` (production default) the **oldest** events —
where the orphans cluster — are exactly what the first pages show.

Other per-row cost (25×): `new ZM\Event`, `Monitor::find_one` (cached per Id,
`Object.php:147-158`, so ≤23 queries), `getThumbnailSrc()` →
`file_exists(<deep path>/objdetect.jpg)` (`Event.php:444`), three
`getStreamSrc()` calls each calling `generateAuthHash()` (session-cached), and
for the fork, an extra `Duration()` when `Length` is 0 which runs
**`ffprobe`** on the file (`Event.php:112-126, 90-110`) — that fires for every
orphan/in-progress event in the page.

### RC-3 — Thumbnails: one full PHP bootstrap per image, GD decode of a 4 MP JPEG on first view, output emitted twice, per-monitor ports, hourly cache-busting

Evidence: `web/views/image.php`. Each `<img>` in the list is
`http://host:100NN/zm/index.php?view=image&eid=…&fid=snapshot&width=48&height=27&auth=…&user=…`
(`Event.php:432-468`).

- Per image: full `index.php` bootstrap (`loadConfig()` reads all 246 Config rows from the DB, `Server::find`, DB-backed session read+write, `getAuthUser()` md5 loop, CSRF include) ≈ 70 ms floor (row #1), then `Event::find_one`, `canView()`, several `file_exists()` on the deep path, then either `readfile()` or GD.
- First view of a thumbnail size: `imagecreatefromjpeg()` of the 388–640 KB full-res `snapshot.jpg` + `imagescale()` + `imagejpeg()` and `file_put_contents()` of `snapshot-48x27.jpg` **next to the recording on the 94 %-full volume** (`image.php:660-700`).
- `image.php:680-690`: `ob_start(); imagejpeg($iScale); $scaled_jpeg_data = ob_get_contents(); … echo $scaled_jpeg_data;` — the buffer is never cleaned, so the first response contains the JPEG twice (row #12: 2,206 B = 2 × 1,103 B). Harmless to browsers, but it is real.
- The `auth=` hash in every thumbnail URL rotates: `generateAuthHash()` regenerates when older than `ZM_AUTH_HASH_TTL*1800` s (`auth.php:333-340`, TTL=2 → every hour). Every thumbnail URL therefore changes hourly and the browser's `max-age=86400` cache (`image.php:624-626`) is defeated; no `ETag`/`Last-Modified`, so no 304s either.
- URLs are spread across ports 10001–10023 (`ZM_MIN_STREAMING_PORT=10000`, `Event.php:437-440`). Each port is a distinct origin: separate TCP/keep-alive pool, no HTTP/2 multiplexing, and each concurrent image ties up a prefork slot (`MaxRequestWorkers 150`). `loading="lazy"` limits this to the visible rows.
- **When `snapshot.jpg` is missing** (events recorded before zmc wrote snapshots, or deleted, or orphans whose only file is `incomplete.*.mp4`), `image.php:450` and `:547` run `ffmpeg -ss <delta> -i <hevc.mp4> -frames:v 1` synchronously inside the request — a full HEVC open+seek+decode per thumbnail — and log an Error on every failure. Not the common path on this install today, but it is the path for every orphan.

Measured: 60–130 ms per thumbnail; 25 per page → 1.5–3 s of serialized-ish work per page on top of the list AJAX, repeated hourly.

### RC-4 — Hover preview spawns a zms process that software-decodes HEVC at 4× speed

Evidence: `ajax/events.php:360-361` builds
`nph-zms?mode=jpeg&scale=8&maxfps=30&replay=single&rate=400&source=event&event=…`
and puts it in `stream_src`; `skin.js:2034-2045 initThumbAnimation()` attaches
`mouseenter` handlers when `ZM_WEB_ANIMATE_THUMBS` is on; `skin.js:1388-1531`
(`thumbnail_onmouseover` → `createThumbnailOverlay`) prefers `video_src` (the fork's MP4/HLS path) but falls back to the MJPEG
stream. In zms, `src/zm_eventstream.cpp:863-1010 sendFrame()` decodes via
`FFmpeg_Input::get_frame()` when no per-frame JPEGs exist — i.e. always here
(`SaveJPEGs=0`). A 2688×1520 HEVC stream at 15 fps × rate 400 = 60 decoded
frames/s per hover, in software, on a box already at load 20. Moving the mouse
down the list starts and abandons one zms per row.

### RC-5 — Montage review: one zms process + one HEVC decode per tile per tick, plus every Frames row for every event in range

Evidence: `montagereview.js:327-386 getImageSource()` returns
`nph-zms?mode=single&event=<id>&time=<t>&scale=<s>&monitor=<m>&frames=1&rate=…`
for each monitor canvas; `montagereview.js:444-461 loadImage2Monitor()` sets
`img.src` on every timer tick (`:477-500`). Measured cost of one such request:
**0.75–1.37 s and 207 KB** (row #25) — zms opens the MP4, seeks, decodes one
HEVC frame, JPEG-encodes it. With 23 monitors that is 23 CGI processes and 23
HEVC decodes per tick; scrubbing the timeline re-issues all of them. This is
the view most likely behind "unusable": it cannot keep up at any speed and it
competes for CPU with the 23 zmc capture processes (each already at 80–150 % CPU).

Frames: `montagereview.js:1296-1465 loadEventData()` fetches
`api/events/index/<filter>/MonitorId:N.json` per monitor, then
`loadFrames()` (`:1713-1790`) fetches `api/frames/index/EventId:a/EventId:b/….json`
in ~1,000-char URL batches — unpaginated (`FramesController.php:67-95`,
`find('all')`), 329 KB for three events (row #28). A 1-hour window (~145
events) is dozens of these calls; a day is ~700 k Frames rows into the
browser.

### RC-6 — Timeline: fatal without a filter; with one, it pulls every alarm Frame in the range into PHP plus N+1 per-event Frames queries

Evidence: production `skins/classic/views/timeline.php:205,213` — `$filter` is
only assigned inside `if (isset($_REQUEST['filter']))`, so `view=timeline` from
the navbar is a guaranteed 500 (row #22, Apache log
`Call to a member function sql() on null in …/timeline.php:213`). The fork's
`timeline.php:204-213` has the same structure (`$filter` is first assigned at
:205, inside the `isset($_REQUEST['filter'])` block, and used at :213).

With a filter: `timeline.php:287`

```sql
SELECT EventId,FrameId,Delta,Score FROM Frames
WHERE EventId IN (SELECT E.Id FROM Events AS E … WHERE EndDateTime >= '…' AND StartDateTime <= '…' GROUP BY E.Id)
  AND Score > 0 ORDER BY Score DESC
```

EXPLAIN (1-day window): materialized subquery of 3,053 events, then
`Frames ref EventId_idx rows=700 (est.)` per event, `Using temporary; Using
filesort` on the outer sort — with the real ~700 k alarm rows/day this is a
multi-second, buffer-pool-thrashing query whose entire result is then
`fetch()`ed into a PHP array (`:288-296`). Then `:365-366` and `:425-426` run
one more `SELECT … FROM Frames WHERE EventId=? … LIMIT 1` per event inside
loops. A 1-hour window already costs 1.2 s and produces a 399 KB HTML page
(row #23); the "last day" the events page's timeline button passes would
exhaust memory the same way RC-1 does.

### RC-7 — Every HTML page spawns `zmdc.pl` and computes storage/DB stats for the navbar

Evidence: `skins/classic/includes/functions.php:651-652, 771` →
`runtimeStatus()` (`:1726-1730`) → `daemonCheck()`
(`web/includes/functions.php`) → `exec('/usr/bin/zmdc.pl check')`. Measured
0.20–0.22 s per invocation (row #31). Also per page: `getStorageHTML()`
(`:897-950`) calls `disk_total_space()/disk_free_space()` per storage area
and `Storage::event_disk_space()` → `SELECT SUM(DiskSpace) FROM Events WHERE
StorageId=? AND DiskSpace IS NOT NULL` (EXPLAIN: `ref Events_StorageId_idx
rows=28958`) per storage area; `getDbConHTML()` runs two `SHOW` statements;
`getSysLoadHTML()`/`getCpuUsageHTML()` read `Server_Stats`; `MenuItem::find`,
`Group::find_one`, `Monitor::find_one`; the filter widget
(`Filter.php:1186-1331 simple_widget()`) queries `States`, `Tags`, `Monitors`.
Result: 0.45–0.6 s for a page whose bare bootstrap is 0.07 s and which, with
`navbar=0`, is 0.33 s (rows #1, #4, #5). The status is polled by JS anyway.

### RC-8 — Per-request bootstrap cost (applies to every list AJAX, thumbnail, stream command and video range request)

Evidence: `web/includes/config.php.in loadConfig()` — `SELECT Name,Value,Type,
Private FROM Config` (246 rows) and `define()` of each on **every** request;
then `Server::find()`, `zm_session_start()` with the DB session handler
(`session.php ZMSessionHandler::read/write` → `Sessions` table read and write
per request), `zm_authenticate_request()` → `userFromSession()` →
`getAuthUser()` (`auth.php:233-314`: `SELECT * FROM Users` + md5 over
TTL-hours × candidate addresses), three `IntlDateFormatter` constructions,
`csrf-magic` include. Floor ≈ 70 ms (row #1). A list page = 1 HTML + 1 AJAX +
25 thumbnails + N hover streams ⇒ ≥ 27 bootstraps ≈ 2 s of pure overhead.
`opcache.validate_timestamps=On` adds a stat per included file.

### RC-9 — Video is served through PHP with `Connection: close`, `no-store`, HTTP/1.0 and 16 KB `fread` loops

Evidence: `web/views/view_video.php:100-157` — every `Range:` request from the
`<video>` element is a fresh Apache prefork child + full RC-8 bootstrap +
`fopen`/`fseek`/16 KB-chunk `echo`/`flush()` loop, answered as `HTTP/1.0 206`
with `Connection: close` (row #30 headers). `index.php` sends
`Cache-Control: no-store, no-cache, must-revalidate` for it. Seeking in a
200 MB HEVC file means dozens of these. No `X-Sendfile`, no `ETag`, no
`Last-Modified`. The fork's HLS path (`view=view_hls`, `data-video-hls-src`)
also goes through PHP per segment.

### RC-10 — API `events/<id>.json` embeds every Frame row and stat-calls the filesystem

Evidence: `web/api/app/Controller/EventsController.php:256-313` —
`recursive = 1` (hasMany Frame) unless `noframes=true`; then two
`find('neighbors')`, `fileExists()`, `fileSize()` (`Model/Event.php:191-192`
`filesize()` on the deep path), `findByEventid(... Score desc)` and
`findByEventidAndType(... 'Alarm')` — two more Frames queries. Measured
2.8–3.0 s / 288 KB for an 1,838-row event (row #27). `index()` is fine
(0.1 s, real SQL LIMIT via `Paginator`).

### RC-11 — Client-side: bootstrap-table + server pagination that needs the whole set; 'All' page size; refresh storms

Evidence: `views/events.php:148-182` (`data-side-pagination="server"`,
`data-page-list="[…,1000, All]"`, `data-show-footer` with totals that require
the full set, `data-cookie` persisting page size); `events.js:40-88`
(`ajaxRequest` aborts and reissues on every change; `window.onpageshow`
refresh at `:540`; `all.bs.table` handler re-binding hover on every table
event at `:368`). 25 thumbnails re-request on every refresh because of RC-3's
rotating URLs.

### RC-12 — Database sizing and data quality (infrastructure, but it multiplies everything above)

- `innodb_buffer_pool_size = 128 MB` (never tuned) with 34 GB Frames / 8 GB Stats: every Frames access (RC-5, RC-6, RC-10, event alarm cues) is disk-bound and evicts the Events/Monitors/Config pages the web layer needs on every request. `mysqld` at ~38 % CPU is largely the 23 cameras inserting ~1 M Frames rows/day.
- InnoDB persistent stats for Frames are stale (est. 74 rows/EventId vs 91–1,838 real), so the optimizer under-costs every Frames join.
- 1,356 orphan events with `EndDateTime IS NULL` from earlier crashes: RC-2's `folder_size()`, `incomplete.*.mp4` fallbacks in `image.php` and `view_video.php`, the `ffprobe` call in the fork's `Length()`, and the CASE expressions in every list query exist because of them.
- Production-only bug: `views/events.php:54` default window is
  `date('Y-m-d h:i:s', time()-3600)` — lowercase `h` (12-hour clock) — so from
  noon onward the default "last hour" is actually the last 13 hours (fork uses
  `H`).

### DB round trips per typical request (counted from code)

- events list AJAX: Config(1) + Servers(1) + session read(1) + Users(1) + Storage::find(1) + main query(1) + `Monitor::find_one` per distinct monitor (≤23) + session write(1) ≈ **10–30**.
- events HTML page: the above + States(1), Server_Stats(1), `SHOW` ×2, Storage(1), `SUM(DiskSpace)` ×2, MenuItem(1), Groups(1), filter widget Monitors/Tags/States(3), `disk_free_space` ×4 syscalls, `exec zmdc.pl` ≈ **20–25 queries + 1 process spawn**.
- one thumbnail: Config(1) + Servers(1) + session r/w(2) + Users(1) + Events(1) ≈ **6 queries + 3–6 stat() calls**, ×25 per page.

---

## (d) What the PHP/JS layer fundamentally needs from the backend

None of the patches in (e) change the fact that the web tier is doing work at
request time that should have been done once at recording time. To be fast
for "23 cameras, 4 MP HEVC, continuous recording" the backend has to provide:

1. **Precomputed per-event visual assets, written at event close (or by a background worker), never at request time.** At minimum a list thumbnail (e.g. 320 px and 64 px), a poster frame, and a preview strip/animated WebP or sprite sheet for hover. Content-addressed or event-id-addressed filenames so they can be served with `ETag` + `Cache-Control: public, max-age=1y, immutable`. zmc already writes `snapshot.jpg`; that is the hook.
2. **An events index that never touches `Frames`.** `Events` should carry everything a list/timeline/cue graph needs: `MaxScoreFrameId`, a downsampled score/audio series (e.g. 100–200 buckets as a JSON/blob column or side table), alarm segments (start/end offsets), `DiskSpace` and `EndDateTime` always populated. `Frames` (243 M rows) then becomes an archival/analytics table, partitionable by day and droppable by retention, and the buffer pool stops being thrashed by the UI.
3. **Range-indexed continuous recording.** Continuous recording should be a `segments` table (monitor, start, end, path, size, keyframe/byte index) with events as *annotations over the timeline*, not 3,300 pseudo-events per day. Montage review and timeline then ask "what segment covers monitor M at time T and which byte offset is the nearest keyframe" — a single indexed lookup, no zms.
4. **Media served by the web server, not by PHP or a CGI decoder.** fMP4/CMAF or HLS written by zmc (the fork already has progressive HLS), served directly (Apache/nginx range requests, `X-Sendfile`/`X-Accel-Redirect`, HTTP/2, `ETag`/`Last-Modified`), with a signed, long-lived per-event URL or a same-origin cookie for auth. Scrubbing = seeking in a `<video>` element over range requests, never `zms mode=single`.
5. **A cheap per-request auth path.** A signed cookie/JWT validated with an HMAC (no `Users` query, no hourly-rotating `auth=` in image URLs), config cached in APCu/opcache rather than re-read from `Config` per request, sessions in Redis/APCu or at least not read+written on image requests.
6. **A single origin.** Drop the per-monitor port scheme (`ZM_MIN_STREAMING_PORT`) for anything that is not a long-lived MJPEG stream; HTTP/2 makes the original reason obsolete.
7. **Real server-side pagination and counts** — `LIMIT/OFFSET` (or keyset on `(StartDateTime, Id)`) and `COUNT(*)` at the SQL layer, exposed by the API the SPA will use.
8. **Housekeeping the backend must own:** close orphan events on startup (set `EndDateTime`, `Length`, `DiskSpace`), persist `DiskSpace` at close, keep `ANALYZE`/stats fresh, size `innodb_buffer_pool_size` for the working set.

---

## (e) Verdict: keep/patch the PHP vs. replace with a new API + SPA

**Honest answer: do both, in that order.** The "unusable" experience today is
caused by five or six concrete defects that are fixable in days with low risk
on the current install. The architecture underneath (per-request config/
session/auth from the DB, media pushed through PHP and CGI decoders, `Frames`
as the timeline's source of truth, per-monitor origins) cannot be made snappy
for this camera fleet by patching, and a new SPA written against the *same*
backend would inherit most of the same slowness. Patch now to make it usable;
build the replacement against the backend contract in (d).

### Patches that give the biggest wins on the current install (ranked)

| # | Patch | Files | Expected effect |
|---|-------|-------|-----------------|
| P1 | Push `LIMIT ? OFFSET ?` into the events SQL and get `total` from a separate `SELECT COUNT(*)` (or `SQL_CALC_FOUND_ROWS`); only fall back to the buffered path when post-SQL filter terms exist. Drop the `Events_Tags`/`Tags` LEFT JOINs and the `GROUP BY` when `Tags` is empty (or move tags to a second query keyed by the 25 ids). Remove `All` from `data-page-list`. | `web/ajax/events.php:239-299,346-349,395-408`, `views/events.php:156` | 30-day list 1.5 s → ~0.1 s; no more 500; page 101 as cheap as page 1 |
| P2 | Never call `folder_size()` from the list: honour `ZM_WEB_EVENT_DISK_SPACE`, show `—` for NULL, and let a maintenance job (or `zmaudit`) fill `DiskSpace`/`EndDateTime` for the 1,356 orphans once. Drop the fork's `ffprobe` in `Length()` on the list path. | `ajax/events.php:388`, `includes/Event.php:112-126,352-364` | removes unbounded filesystem walks from every page |
| P3 | Thumbnails: `ob_end_clean()` fix; add `Last-Modified`/`ETag` + `Cache-Control: public, max-age=…, immutable`; stop putting the rotating `auth=` hash in same-origin image URLs (session cookie already authenticates — or lengthen `ZM_AUTH_HASH_TTL`); serve `view=image` from `:80` (leave `ZM_MIN_STREAMING_PORT` for MJPEG streams only); have zmc write a small `snapshot-thumb.jpg` at event start so GD never decodes the 4 MP JPEG. | `views/image.php:640-700`, `includes/Event.php:432-468`, `zm_event.cpp` (snapshot writer) | 25 × 70–130 ms → 25 × ~5 ms and mostly 304/cached across page loads |
| P4 | Navbar: cache `daemonCheck()` (APCu or a 10 s file) or drop it from page render and rely on the existing JS status poll; cache `getStorageHTML()`/`SUM(DiskSpace)` per storage for 60 s. | `skins/classic/includes/functions.php:651,771,897-950,1726-1730` | −0.2 to −0.3 s on every HTML page |
| P5 | Timeline: initialise `$filter = new ZM\Filter()` before line 205 (fixes the 500); replace the range-wide `Frames` query with `MaxScore`/`AlarmFrames` from `Events` (already there) or a per-event `LIMIT 1` via a lateral join, and delete the N+1 loops. | `skins/classic/views/timeline.php:205-213,287-296,365,425` | timeline works at all; 1-day window stops touching 700 k Frames rows |
| P6 | Montage review: replace `zms mode=single` tiles with a `<video>` per tile over the fork's HLS/MP4 range path (seek instead of re-decode), or at least a `view=image&fid=snapshot` poster while paused; page `api/frames/index` or fetch only alarm frames. | `montagereview.js:327-386,444-461,1713-1790`, `FramesController.php:67-95` | eliminates 23 HEVC decodes per tick |
| P7 | Infra (no code): `innodb_buffer_pool_size` 8–16 GB; `ANALYZE TABLE Frames` in a quiet window; `opcache.validate_timestamps=0`; `memory_limit` sane; disable `ZM_WEB_ANIMATE_THUMBS` until P3/P6 land; `KeepAlive` is already on. | `my.cnf`, `php.ini`, Options→Web | removes the disk-bound floor under every Frames access |
| P8 | Video: `X-Sendfile`/`mod_xsendfile` for `view_video`, `HTTP/1.1 206`, drop `Connection: close`, add `Last-Modified`/`ETag`, allow caching with `private`. | `views/view_video.php:100-157`, `index.php` cache headers | seeking no longer costs a PHP bootstrap per range |
| P9 | API: default `noframes=true` behaviour for `events/<id>.json` (or paginate the `Frame` association); paginate `frames/index`. | `EventsController.php:256-313`, `FramesController.php:67-95` | 2.8 s → ~0.1 s |

P1 + P2 + P3 + P4 + P7 are a few hundred lines and would take the events list
from "sometimes fatal, 1.5 s per page + 2–3 s of thumbnails" to roughly
0.1–0.2 s per page with cached thumbnails. P5 and P6 stop the two views that
currently either crash or bring the box to its knees.

### Why patching is not the end state

- The list/timeline/montage views are built on `Frames` as the timeline model and on zms as the frame source; both are wrong for HEVC passthrough recording (no JPEGs on disk, a decoder is needed for every still) and cannot be tuned around.
- Every request re-derives config, session and identity from MySQL; every image and every video range request is a full PHP process. The fixed cost floor (70 ms + prefork slot) multiplied by 25–50 requests per page is what makes the page *feel* slow even after P1–P4.
- The classic skin's page-render coupling (navbar status, storage stats, filter widgets rendered server-side into every page) means each view pays for things it does not display.
- A new API + SPA is the right vehicle, **but only against a backend that ships precomputed thumbnails/posters/preview strips, an events index that never joins `Frames`, a segment index for continuous recording, and media served statically with proper caching.** Building the SPA first against CakePHP + zms would reproduce RC-3, RC-5, RC-8 and RC-9 in a nicer-looking shell.

Recommended sequencing: land P1–P4 and P7 on the production 1.38.4 box now
(they are small and backport cleanly); fix P5 so the timeline stops
500-ing; then design the replacement backend contract from (d) and build the
new events API + SPA on it, replacing `events`, `event`, `timeline` and
`montagereview` first and leaving console/options in PHP until last.

---

## Appendix — reproduction commands (read-only)

```sh
# web-form login (API login sets a cookie but not one the web views accept)
curl -s -c cj --data-urlencode action=login --data-urlencode view=login \
     --data-urlencode "username=$U" --data-urlencode "password=$P" http://HOST/zm/index.php
# the list AJAX events.js issues (last day, page 1)
curl -s -b cj -o /dev/null -w '%{time_total}\n' \
 'http://HOST/zm/index.php?view=request&request=events&task=query&limit=25&offset=0&sort=StartDateTime&order=desc&filter[Query][terms][0][attr]=StartDateTime&filter[Query][terms][0][op]=>=&filter[Query][terms][0][val]=-1 day'
# the fatal (no date term, ZM default sort)
curl -s -b cj -o /dev/null -w '%{http_code} %{time_total}\n' \
 'http://HOST/zm/index.php?view=request&request=events&task=query&limit=25&offset=0&sort=StartDateTime&order=asc'
# one montage-review tile
curl -s -b cj -o /dev/null -w '%{time_total}\n' \
 'http://HOST/zm/cgi-bin/nph-zms?mode=single&event=EID&time=UNIXSECS&scale=50&monitor=MID&frames=1&auth=…&user=…'
```
