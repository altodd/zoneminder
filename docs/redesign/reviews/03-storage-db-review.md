# 03 - Storage, database schema and Perl management layer review

Scope: the fork at `zoneminder/` (1.39.35) as it relates to the production install profiled in
`docs/research/00-live-server-profile.md` (23 x HEVC 2688x1520 passthrough, SectionLength 600, SaveJPEGs=0, Frames
243.7M rows, Stats 52.4M rows, 14.3 TB across two 94%-full volumes, mysqld at 38% CPU). All paths below are relative
to `zoneminder/` unless absolute.

A caveat that matters for every section: the production DB reports Events indexes `PK, MonitorId, StorageId,
StartDateTime, (EndDateTime,DiskSpace)`. That is the upstream 1.36/1.37 index set. The fork's
`db/zm_update-1.39.22.sql` replaces `MonitorId` with `(MonitorId,StartDateTime)` and `(EndDateTime,DiskSpace)` with
`(EndDateTime,MonitorId)`, `1.39.27` collapses the three Frames indexes into `(EventId,FrameId)`, `1.39.26.sql.in`
rewrites the triggers, and `1.39.28` drops `Stats.MonitorId/ZoneId`. So production is either upstream, or the fork
with its 1.39.2x migrations not yet applied. Where the two differ I describe both and mark them **[upstream]** /
**[fork]**. Verify with `SHOW TRIGGERS FROM zm; SHOW INDEX FROM Frames; SHOW PROCEDURE STATUS WHERE Db='zm';`.

---

## 1. Per-frame rows: what is written to Frames and Stats, when, and who reads it

### What a frame row is and when one is written

`Event::AddFrame` (`src/zm_event.cpp:581-718`) runs once per video packet that reaches the event (`AddPacket_`,
`zm_event.cpp:483` calls it for every video packet, image or not), so `frames++` counts **capture** packets: 747 s x
18 fps = 13.5k, which is the `Events.Frames` average you see. A `Frames` row is queued only when `db_frame` is true
(`zm_event.cpp:660-666`):

```
frame_type == BULK or ALARM or frames == 1 or score > max_score
  or monitor_state in (ALERT, ALARM, PREALARM)
```

Frame `Type` (`zm_event.cpp:592-599`, enum at `db/zm_create.sql.in:450`):

- `Alarm`: `packet->score > 0` (the frame was analysed and a zone fired).
- `Bulk`: monitor state is `IDLE` and `frames % ZM_BULK_FRAME_INTERVAL == 0` (default 100,
  `ConfigData.pm.in:2592-2610`). It is the "I am still recording, nothing happened" marker; the help text explicitly
  describes it as the DVR (Record/Mocord) economy mode.
- `Normal`: everything else. A `Normal` frame only gets a row when the state is ALERT/ALARM/PREALARM or its score set
  a new event maximum.

Rows are buffered in `frame_data` and flushed by `WriteDbFrames` (`zm_event.cpp:531-580`) as one multi-row `INSERT
INTO Frames ... VALUES (...),(...)` when any of: a snapshot/alarm image was written, `frame_data.size() >= 100`
(`MAX_DB_FRAMES`, `zm_event.cpp:41`), the frame is `Bulk`, or `frame_data.size() > 5*capture_fps`
(`zm_event.cpp:689-696`). At 18 fps that is a flush every ~90 rows. Every flush also queues `UPDATE Events SET
Length,Frames,AlarmFrames,TotScore,AvgScore,MaxScore,MaxScoreFrameId` (`zm_event.cpp:700-713`), which fires
`event_update_trigger` (no-op body because DiskSpace is unchanged, but still a trigger invocation and an X-lock on the
Events row). All of this goes through `dbQueue`, a single writer thread executing autocommitted statements one at a
time (`src/zm_db.cpp:389-413`).

On the profiled fleet: 243.66M rows / 59.6k events = ~4,090 Frames rows per event = ~5.5 rows/s per camera, i.e.
essentially every **analysed** frame (AnalysisFPSLimit=5) plus the alarm/score extras, not every captured frame.
Fleet-wide that is ~126 Frames inserts/s and ~45 `UPDATE Events` per event (~1.4/s), not the 414/s the profile
assumed.

`Stats` rows: one per `(frame, zone)` in `frame->zone_stats`, appended to the same flush (`zm_event.cpp:544-566`),
only when `ZM_RECORD_EVENT_STATS=1` (`ConfigData.pm.in:1624-1639`). `zone_stats` is filled in `Monitor::Analyse` for
every analysed frame on every zone (`src/zm_monitor.cpp:2404-2408`), so with 1 zone you would expect ~1 Stats row per
analysed frame; the observed ratio (52.37M / 59.6k = 878 = avg AlarmFrames) says only scoring frames carry stats on
this install. Either way the column set (`zm_create.sql.in:931-953`: PixelDiff, AlarmPixels, FilterPixels, BlobPixels,
Blobs, Min/MaxBlobSize, bbox, Score) is per-zone tuning telemetry.

Also written per event, not per frame: `Event_Data` rows when a packet carries object detections
(`zm_event.cpp:487`), and `Events_Tags`.

### Who reads Frames / Stats

- Event view cue/level graph: `web/ajax/status.php:193-205` (`entity=frames`, whole `Frames` set for the event,
  columns EventId/FrameId/Type/Delta/Score/ AudioLevel) consumed by `web/skins/classic/views/js/event.js:163-207`
  (`setAlarmCues`, `levelGraphSeries`). This is the alarm-score/audio graph.
- "Frames" button / frames list view: `web/skins/classic/views/frames.php` via `web/ajax/frames.php:111,155` (`SELECT
  * FROM Frames WHERE EventId=?`).
- Single frame view / prev-next: `views/frame.php:36-48`, `web/includes/Event.php:776-780`,
  `web/ajax/status.php:500-519` (`getFrameImage`, `getNearFrame`). Only meaningful with SaveJPEGs.
- Thumbnail of the max-score frame: `web/includes/functions.php:841`, `web/includes/Event.php:374` (`WHERE EventId=?
  AND Score=MaxScore`).
- Timeline view: `views/timeline.php:287,365,425` (Score>0 frames).
- Export: `skins/classic/includes/export_functions.php:105`.
- Stats: only the per-frame zone-stats popup `web/ajax/stats.php:15`, `skins/classic/includes/functions.php:1742`, and
  the `AlarmedZoneId` filter term (`Filter.pm:329`, `web/includes/FilterTerm.php:144`).
- zms playback (`src/zm_eventstream.cpp:281-345`): loads every Frames row of the event to build a per-frame timestamp
  table, interpolating between Bulk rows (`zm_eventstream.cpp:316-330`). That table drives **JPEG** playback
  (`SaveJPEGs`) and the `Delta`-based seek for zms; the mp4 path (`view=view_video`, HLS via `index.m3u8`) does not
  use it.
- Perl: `Event::Close` (`Event.pm:1077-1110`) and zmaudit's "close stale events" (`zmaudit.pl.in:765-821`) recompute
  `Frames/AlarmFrames/TotScore` from `max(FrameId)`, `count(Score>0)`, `sum(Score)`.
- `Event.pm:313` uses `Frames/FullLength` for `GenerateVideo` fps (jpeg->mp4).

### What breaks if Frames were not written for passthrough events

1. **zmaudit deletes the event.** `zmaudit.pl.in:683-706` treats any event older than `ZM_AUDIT_MIN_AGE` with no
   Frames row as "empty" and deletes it (`DELETE FROM Events`, and the files on the next fs/db reconciliation pass).
   This is the hard blocker; the check would have to be changed to "no Frames AND no DefaultVideo file".
2. The event-view score graph and the Frames button go empty (cosmetic; the graph could be driven from
   `Events.MaxScore/AlarmFrames` or from a compact alarm-interval list instead).
3. zms JPEG playback / frame stepping break, but with SaveJPEGs=0 there are no JPEGs to step through; mp4/HLS playback
   is unaffected.
4. `MaxScoreFrameId` thumbnail lookup (`functions.php:841`) fails over to the snapshot.jpg path.
5. Stale-event repair in zmaudit (`765-821`) cannot recompute counts; it would fall back to
   `mp4_duration`/`guess_EndDateTime` (`Event.pm:974-1005`).

There is **already** an economy mode: `Bulk` frames. But it only kicks in while the monitor state is `IDLE`; on a
Mocord camera with a full-frame "All" zone at the profiled sensitivity, the state is ALERT/ALARM/PREALARM for most of
the section, so the `monitor_state` term at `zm_event.cpp:664-666` defeats it. The minimum change to make Frames small
on this fleet is to make `db_frame` = `Alarm` transitions + Bulk only (drop the state term and the `score > max_score`
term for passthrough events) and to keep `AlarmFrames` exact by counting in C++ (it already is: `alarm_frames++` at
`zm_event.cpp:602`).

---

## 2. Deletion cost of one 10-minute event

Path: `zmfilter.pl.in:367-369` -> `ZoneMinder::Event::delete` (`scripts/ZoneMinder/lib/ZoneMinder/Event.pm:395-507`)
-> `delete_files` (`Event.pm:510-583`) -> `Storage::delete_path` (`Storage.pm`, `rm -rf` via `executeShellCommand`).

Per event, in one short transaction (`Event.pm:456-478`, READ COMMITTED):

| statement | rows | index work |
|---|---|---|
| `DELETE FROM Stats WHERE EventId=?` | ~878 | PK + `EventId_ZoneId` (+ `MonitorId`, `ZoneId` **[upstream]**) |
| `DELETE FROM Event_Data WHERE EventId=?` | 0 | - |
| `DELETE FROM Frames WHERE EventId=?` | ~4,090 | PK + `EventId_FrameId_idx` **[fork]**; PK + `EventId`, `Type`, `TimeStamp` **[upstream]** = 4 B-trees |
| `DELETE FROM Events WHERE Id=?` | 1 | PK + 4 secondary indexes, plus the trigger below |

No FK cascades: the Frames/Stats FKs are commented out (`zm_create.sql.in:448, 934-938`) and `Events_Lock`
deliberately has none (`zm_create.sql.in:373-381`). The only real FKs are `Snapshots_Events` and `Events_Tags`
(`zm_create.sql.in:1402-1404, 1436-1437`), which cascade on the Events delete.

`event_delete_trigger` (`db/triggers.sql:116-159`, BEFORE DELETE): 4 single-row DELETEs on
`Events_Hour/Day/Week/Month` (PK lookups; the rows are usually already pruned by zmstats for anything older than an
hour/day/week, so 1-3 of them hit nothing) and **one** `UPDATE Event_Summaries ... WHERE MonitorId=?`. **[upstream]**
(`db/zm_update-1.33.8.sql:1-30, 201-215`): the same 4 DELETEs, but each bucket table has its own BEFORE DELETE trigger
that does `UPDATE Monitors SET HourEvents=...`, so one event delete = up to 5 X-locks on the Monitors row, plus
`UPDATE Storage SET DiskSpace=...`.

Then, outside the transaction: `Storage::adjust_diskspace` (one `UPDATE Storage`, one `SELECT`), and `delete_files`:
`rm -rf <storage>/<mid>/<date>/<eid>` (1 dir + 3-4 files: `<id>-video.<codec>.mp4` of ~240 MB, `snapshot.jpg`,
`alarm.jpg`, `index.m3u8`), then `opendir/readdir/rmdir` up the parents (`Event.pm:555-579`) to reap an empty day
directory.

Unlinking a 240 MB ext4 file frees ~60k 4 KiB blocks worth of extent metadata; on a fragmented volume that is a few ms
to tens of ms of journal work, and with 100 of them in a burst the `rm -rf` calls serialise on the volume's journal
while 23 writers are appending.

**Steady-state numbers.** 23 monitors x 86,400 s / 747 s avg = ~2,660 events/day (3,312 if every section ran the full
600 s). To hold the volumes at 94-95% the filters must delete the same number:

- Events rows: ~2,660/day
- Frames rows: 2,660 x 4,090 = ~10.9M/day (~126/s), 4 B-tree deletes each **[upstream]**, 2 **[fork]**
- Stats rows: 2,660 x 878 = ~2.3M/day (~27/s)
- Events_* bucket rows: up to 4 per event, trigger-driven
- files: ~10k unlinks/day, 2,660 directories
- and, because MoveToDiskWhenFull sits in front of the purge, the same 2,660 events/day are first **copied**
  volume-to-volume (section 4).

That is ~13M row deletes/day against tables whose secondary indexes are 18.5 + 2.8 GB. With `innodb_buffer_pool_size`
smaller than the ~42 GB of Frames+Stats data+index, each `DELETE FROM Frames WHERE EventId=?` touches index pages for
an event that was written ~20 days ago and long since evicted, so it is random I/O on the DB volume (sda at 55-60%
util, 3.7k r/s in the profile). This, not CPU, is the dominant per-event delete cost.

---

## 3. The DB triggers

**[fork]** `db/triggers.sql` (installed by `zm_update-1.39.26.sql.in`):

| trigger | when | body | cost per firing |
|---|---|---|---|
| `event_update_trigger` (`triggers.sql:47-99`) | AFTER UPDATE ON Events | if DiskSpace changed: 4 bucket UPDATEs by PK + 1 `UPDATE Event_Summaries WHERE MonitorId`; if Archived flipped: Events_Archived insert/delete + ES update | 0 statements when DiskSpace unchanged (the common case: ~45 flush UPDATEs per event, Notes updates, DefaultVideo updates); 5 statements once at event close |
| `event_delete_trigger` (`triggers.sql:116-159`) | BEFORE DELETE ON Events | 4 bucket DELETEs + 1 ES UPDATE | 5 statements per delete |
| `event_insert_trigger` | **dropped** (`triggers.sql:106`) | zmc does it inline: 4 bucket INSERTs + `INSERT ... ON DUPLICATE KEY UPDATE Event_Summaries` (`zm_event.cpp:160-192`) via dbQueue | 5 autocommitted statements per event open |
| `update_storage_stats` | **dropped** (`triggers.sql:174`) | Storage.DiskSpace is now adjusted relatively: `UPDATE Storage SET DiskSpace=DiskSpace+size` at close (`zm_event.cpp:353-354`), `Storage::adjust_diskspace` (`Storage.pm`) on delete/move | 1 statement |
| `Zone_Insert/Delete_Trigger` | Zones | `UPDATE Monitors SET ZoneCount` | irrelevant |

**[upstream]** (`db/zm_update-1.33.8.sql`, still what an un-migrated production DB runs): `event_insert_trigger` does
4 bucket INSERTs and `UPDATE Monitors SET HourEvents=..,DayEvents=..,..,TotalEvents=..` (X-lock on the Monitors row);
`event_update_trigger` does `UPDATE Storage`, 4 bucket UPDATEs (each of which fires `Events_<b>_update_trigger` ->
`UPDATE Monitors`), and `UPDATE Monitors SET TotalEventDiskSpace`; the bucket delete triggers each do `UPDATE
Monitors`. Older still, `update_storage_stats` (`zm_update-1.31.20/21.sql`) recomputed `SUM(DiskSpace) FROM Events`
per call.

**Do they lock the Monitors row / contend with 23 zmc?** In the fork, no: the per-monitor counters live in
`Event_Summaries` (`zm_create.sql.in:834-848`), so the X-lock is on an `Event_Summaries` row, and `Monitors` is never
touched by event traffic. That was the point of the change: zmc reads `Monitors` at startup and on reload, and the web
UI reads it constantly. In upstream, every event open/close/delete takes an X-lock on the `Monitors` row of that
camera, which blocks any concurrent `UPDATE Monitors` (settings save, `zmwatch`, `Monitor_Status` is a separate table)
and, worse, any long-running reader in REPEATABLE READ that touched it.

Contention frequency is low in absolute terms: per camera one event open (5 statements) and one close (1 UPDATE Events
+ trigger's 5 statements) every 600 s; fleet-wide ~0.4 trigger firings/s with a non-trivial body. What made it hurt
was lock ordering, not rate: a filter deleting event N on monitor M (Events[N] -> buckets[N] -> ES[M]) while zmstats
pruned bucket rows and the cascade triggers went buckets -> ES from the other side, or a `LockRows` filter holding
`SELECT ... FOR UPDATE` over its whole result set. The fork's `1.39.21` (Events_Lock), `1.39.26` (consolidated
triggers), `Event.pm:427-455` (documented lock order + deadlock retry) and `zmstats.pl.in` (snapshot then per-monitor
UPDATE, chunked PK deletes) are all patches on that. The remaining cost is the ~45 no-op `event_update_trigger`
invocations per event from the `UPDATE Events SET Length,Frames,...` flushes, which is CPU, not locks.

---

## 4. Directory layout, inode count, zmaudit walk, AutoMove

### Layout

`Event::SetPath` (`src/zm_event.cpp:720-794`) and `Event.pm:150-206`:

- **Medium** (production: `Storage.Scheme='Medium'`, the default at `zm_create.sql.in:1095`):
  `<StoragePath>/<MonitorId>/<YYYY-MM-DD>/<EventId>/`. Note `USE_DEEP_STORAGE=1` in Config is a legacy option
  (`ConfigData.pm.in:680-690`); the scheme is per Storage row and the C++ only looks at `Storage.Scheme`
  (`src/zm_storage.cpp`).
- **Deep**: `<mid>/<yy>/<mm>/<dd>/<HH>/<MM>/<SS>/` plus a symlink `<mid>/<yy>/<mm>/<dd>/.<EventId> -> HH/MM/SS`
  (`zm_event.cpp:748-762`): 6 directories and a symlink per event.
- **Shallow**: `<mid>/<EventId>/` plus an empty `.<EventId>` file.

Files per event with `VideoWriter=2, SaveJPEGs=0` (`zm_event.cpp:850-851, 876, 632-651`):

1. `incomplete.<codec>.<container>` while recording, hard-linked to `<EventId>-video.<codec>.mp4` at close and then
   unlinked (`zm_event.cpp:232-270`). Upstream names it `<EventId>-video.mp4`.
2. `snapshot.jpg` (first frame, rewritten whenever score exceeds max).
3. `alarm.jpg` (first alarm frame; absent for never-alarmed events).
4. `index.m3u8` **[fork]**: an HLS byte-range playlist listing every keyframe-aligned fragment `(offset, size,
   duration)` (`src/zm_videostore.cpp:1761-1799`, fragments tracked at `1616-1651`). This is, incidentally, a keyframe
   index already on disk.
5. `<frames>-capture.jpg` / `-analyse.jpg` only with SaveJPEGs bits set.

So one event = 1 directory + 3-4 files ~= 4-5 inodes, ~240 MB. Day directories: 23 monitors x ~22 days = ~500. 60k
events ~= 300k inodes across two volumes; ext4 has no problem with that. The per-day directory holds ~144 event dirs,
which is fine for readdir. Deep would be ~360k directories plus 60k symlinks and a `readdir` of every `HH/MM/SS`
level.

### How zmaudit walks it, and cost at 17 TB

`scripts/zmaudit.pl.in`, run continuously (`zmpkg.pl.in:255`, `zmaudit.pl -c`) every `ZM_AUDIT_CHECK_INTERVAL` = 900 s
(`ConfigData.pm.in:2544`):

1. Per storage, `chdir` and `glob('[0-9]*')` for monitor dirs (`zmaudit.pl.in:232-238`).
2. Deep check: `glob("$mid/[0-9][0-9]/[0-9][0-9]/[0-9][0-9]")` (`256`), harmless here.
3. Medium: `glob("$mid/YYYY-MM-DD/*")` (`419`), then per event dir: `-d` stat, `new Event`, `Path()`, `age()` (a `-M`
   stat, `Event.pm:664-675`) and `time_of_youngest_file()` (`1295-1313`: opendir + readdir + a stat per entry; note it
   stats `$dir` instead of `$file` at line `1306`, so it is cheap but returns the directory mtime).
4. Shallow check: `readdir` of every monitor dir again (`444-466`).
5. `delete_empty_subdirs` recursion over the monitor tree (`470, 1249-1290`).
6. DB side: one `SELECT Id, age FROM Events WHERE MonitorId=? AND StorageId IN (..)` per monitor (`208-220`), then for
   **every** db event found in the fs a `ZoneMinder::Event->find_one(Id=>..)` (a SELECT, `zmaudit.pl.in:629`) and
   `check_for_in_filesystem()` (a `glob "$path/*"`, `Event.pm:649-663`).
7. Then the table-level audits every pass:
   - `SELECT E.Id .. FROM Events E LEFT JOIN Frames F .. WHERE isnull(F.EventId)` (`684-685`): 60k index probes, fine.
   - `SELECT DISTINCT EventId FROM Frames WHERE (SELECT COUNT(*) FROM Events WHERE Id=EventId)=0` (`713-714`): at best
     a loose index scan over ~60k distinct EventIds, at worst 243M correlated PK lookups. Run `EXPLAIN` on it; on
     MySQL 8 with a correlated aggregate subquery in WHERE it is usually the bad plan.
   - `SELECT DISTINCT EventId FROM Stats WHERE EventId NOT IN (SELECT Id FROM Events)` (`733-734`): full scan of 52M
     rows (5 GB) every 15 minutes.
   - stale-event close (`765`, uses `Events_EndDateTime_*` index), swap-image `File::Find` (`844`), Logs pruning
     (`852-925`), Event_Summaries and Storage resync (`938-1140`, `SUM(DiskSpace) FROM Events GROUP BY ...`).
8. `redo MAIN if $cleaned` (`535, 706`): any deletion restarts the whole pass.

Cost estimate for this fleet: ~60k event dirs x (1 readdir + ~6 stats + 1 SELECT + 1 glob) ~= 600k metadata syscalls
and 60k SELECTs per pass. With a warm dentry cache this is 20-60 s of Perl; cold (after a reboot, or with the page
cache churned by MoveTo copies, see below) it is dominated by random metadata reads on the USB SATA volume and can
take many minutes. The two orphan queries at step 7 are the likelier source of the steady 38% mysqld: they read every
Frames index page and every Stats row every 15 minutes regardless of whether anything changed. Nothing in the walk
touches file contents, so "17 TB" itself is irrelevant; it is the 300k inodes and the 243M+52M rows that cost.

### AutoMove (`MoveToDiskWhenFull`)

`zmfilter.pl.in:376-381` -> `Event::MoveTo` (`Event.pm:814-871`) -> `Event::CopyTo` (`Event.pm:693-812`):

1. `begin_work` + `lock_and_load()` = `SELECT * FROM Events WHERE Id=? FOR UPDATE` (`Event.pm:822-825`,
   `Object.pm:173`). The Events row X-lock is held for the whole copy.
2. `File::Path::make_path` of `<dst>/<mid>/<date>/<eid>`, `readdir` the source, and `File::Copy::copy` each regular
   file (`Event.pm:777-811`), i.e. Perl userspace read/write in 2 MiB chunks through the page cache, no
   `copy_file_range`/`sendfile`, no `fadvise`, no fsync.
3. `DiskSpace(undef)` recomputes the size with `File::Find` (`Event.pm:676-691`).
4. `save()` (UPDATE Events SET StorageId=.. and every other column) fires `event_update_trigger`; commit; two
   `adjust_diskspace` UPDATEs on Storage.
5. `delete_files($OldStorage)` = `rm -rf` on the source.

Limit 200 per pass, so one pass moves up to 200 x 240 MB = 48 GB from the MegaRAID to a single USB SATA disk that is
itself at 94-95%. At a realistic 100-150 MB/s that is 5-8 minutes per pass in which: 48 GB of source data is pulled
through the page cache (evicting the write-behind pages of the 23 live recordings and the dentries zmaudit relies on),
48 GB is written to the destination whose free space is 5-6% (ext4 allocator working in the fragmented tail of the
disk), and each Events row is X-locked for its ~2-3 s copy. Because the destination is also the purge target, the
purge filter (limit 100, 60 s interval) is what makes room: net effect is that every byte recorded is written twice
and read once more before it is finally deleted, and the USB disk sees ~9 MB/s of continuous copy-in on top of the
purge's unlink churn. `Concurrent`/`Background` flags (`zm_create.sql.in:433-434`) are per-filter; production runs the
three filters as three `zmfilter.pl --filter_id=N` daemons (`zmpkg.pl.in:246-248`), so the move and the purge run in
parallel against the same disk.

---

## 5. Steady-state storage math and failure modes

Inflow: 23 cams x ~3.4 Mbit/s average ~= 70-80 Mbit/s ~= 750-860 GB/day ~= 9-10 MB/s sustained, ~2,660 events/day of
~240-300 MB.

Capacity: 6.3 TB + 11 TB = 17.3 TB, held at 94% = ~16.3 TB of events -> 16.3 TB / 0.8 TB/day ~= **20 days of
retention**, which matches the observed 59.6k events / 2,660 per day ~= 22 days. The 2024-09-29 oldest event is either
archived or an orphan; the working set is ~3 weeks old.

Why the volumes sit at 94-95% permanently (this is by construction):

- `PurgeWhenFull` = `Archived=0 AND DiskPercent>=95 AND StorageId=2 AND EndDateTime IS NOT NULL`, sort Id asc, limit
  100, AutoDelete. `Filter.pm:307-309` turns `DiskPercent` into the literal `zmDiskPercent`, and `Filter::Execute`
  (`Filter.pm:112-116`) substitutes the result of **one** `df <storage path>` shell call per pass (`getDiskPercent`,
  `Filter.pm:617-625`; the path is the storage named by the `StorageId` term, `Filter.pm:351-354`, else the first
  Storage row). So the SQL is `... AND 94 >= 95 ...` = constant FALSE, returns nothing, costs one fork+`df` per 60 s.
  It is not a `du`, and it is not per event. Once `df` says 95, one pass deletes the 100 oldest events on storage 2
  (~24-30 GB, ~1/4 of 1% of 11 TB), and it keeps doing that every 60 s until `df` reads 94 again. The disk therefore
  oscillates in [94, 95].
- `MoveToDiskWhenFull` does the same for storage 1 with AutoMove, so storage 1 also oscillates in [94, 95], and its
  overflow is what fills storage 2.
- `Delete after 7 days low motion` (`StartDateTime < -7d AND AlarmFrames < 2`, no limit) is a range scan of ~2/3 of
  Events every 60 s; on a Mocord fleet with a full-frame zone almost nothing has `AlarmFrames < 2`, so it is mostly a
  60k-row scan that returns few rows. It does not change the steady state, it only shaves the least useful events a
  bit earlier.
- `Update DiskSpace` (`DiskSpace IS NULL AND EndDateTime IS NOT NULL`) is a no-op on any 1.36+ install because
  `~Event` writes `DiskSpace` synchronously at close (`zm_event.cpp:325-345`); it still runs its scan every 60 s (the
  `(EndDateTime,DiskSpace)` index does not help a `DiskSpace IS NULL` predicate behind an `EndDateTime IS NOT NULL`
  range).

Failure modes:

1. **Hitting 100% between filter runs.** Time to fill 1% of storage 1 at 9.3 MB/s is 63 GB / 9.3 MB/s ~= 113 minutes,
   so a 60 s interval has ample margin *while zmfilter is alive*. If the MoveToDisk daemon dies or stalls (DB down,
   zmdc restart storm, a 5-8 minute pass overrunning while the purge on storage 2 lags), storage 1 has 5% = 315 GB ~=
   9.5 hours of headroom, storage 2 has 550 GB but is only fed by moves. When storage 1 does fill: `mkdir` for a new
   event fails with ENOSPC, `Event::Run` (`zm_event.cpp:796-846`) falls back to "any other Storage row" and silently
   starts writing new events to storage 2 with `StorageId=2`; an in-progress `incomplete.mp4` gets
   `av_interleaved_write_frame` errors and the event closes truncated. There is no back-pressure to the purge and no
   alarm; the only symptom is Events with `StorageId` flipping.
2. **Copy into a full destination.** `CopyTo` checks nothing about free space (`Event.pm:706-724`); if storage 2 is at
   95% and the purge is slower than the move (purge 100/min vs move 200/pass), `File::Copy` fails mid-file, `MoveTo`
   returns the error and leaves the partial destination directory behind for zmaudit to find as an "fs event not in
   db" (`zmaudit.pl.in:495-506`) after `ZM_AUDIT_MIN_AGE` (86,400 s).
3. **ext4 fragmentation.** 23 files grow concurrently on the same volume via 32 KiB avio writes flushed at every
   keyframe (`zm_videostore.cpp:1633`), with no `fallocate`, while 100-file unlink bursts free 240 MB holes at random
   positions (oldest-by-Id is interleaved across cameras). Delayed allocation and extents keep individual files
   reasonably contiguous, but the free-space map at 94-95% full is a set of ~240 MB holes and the allocator has to fit
   new files into them. Over months the per-file extent count grows and both playback seeks and `rm` get slower;
   `e4defrag -c` on a sample of old events will show the score. A volume kept at 95% for a year is the worst case for
   ext4's allocator. The Storage-2 USB disk also suffers the 48 GB copy bursts landing in that same fragmented tail.
4. **MoveTo churn while recording.** Each pass streams 48 GB through the page cache. The 23 `incomplete.mp4` writers
   depend on write-behind; when their dirty pages are evicted early the writeback becomes synchronous with the
   fragment flushes and the RAID sees the copy's reads interleaved with 23 append streams. In the profile sda shows
   3.7k r/s vs 400-570 w/s: the reads are the DB and these copies, not playback.
5. **Retention is not a policy, it is an emergent property** of the two thresholds and the two limits. There is no
   per-camera retention, no "keep alarm events longer" (the 7-day filter is the only such rule and it barely matches),
   and no way to say "keep 14 days" - the number falls out of bitrate and disk size.

---

## 6. Replacement index design: segment-based recorder

Principles, each driven by a cost above:

- The unit of storage is a fixed-length **segment** per camera (10-60 s fMP4 or MKV), not an event. Retention deletes
  whole segments: one unlink + one row. No per-frame rows in the DB at all.
- The per-frame timing that zms needs (`zm_eventstream.cpp:281-345`) and the keyframe positions that HLS/seeking need
  already exist in the container (`moof`/`sidx`, or the `index.m3u8` the fork writes). Store them once as a small blob
  per segment (moonfire-nvr's `sample_index`), never as rows.
- Motion/object **detections** are intervals with a score and a bbox, tied to a camera and a time, not to a segment;
  they survive segment deletion as long as their own retention says so, and their thumbnails are their own tiny files.
- Retention is a **policy row**, evaluated by a single daemon with a min-heap of `(camera, start_ts)`, deleting oldest
  segments first, per volume, until the volume's target is met; the same daemon writes new segments round-robin (or by
  free-space weight) across volumes, so no volume-to-volume copying ever happens.
- Counters (`Event_Summaries`, `Storage.DiskSpace`) are derived by `SUM()` over indexed ranges when a UI asks; with
  ~2.9M segment rows for 20 days at 30 s segments (23 x 86,400/30 x 20) that is a covering-index range sum in the low
  milliseconds, so there is nothing for a trigger to maintain.

### Proposed schema (SQLite; PostgreSQL notes inline)

SQLite in WAL mode is enough for this write rate (23 inserts/30 s + a few detections/s) and removes the mysqld process
entirely; the same DDL runs on PostgreSQL with `INTEGER PRIMARY KEY` -> `BIGSERIAL`, `BLOB` -> `BYTEA`, and `STRICT`
dropped.

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;          -- segment files are fsync'd; the index can lag a WAL checkpoint

CREATE TABLE cameras (
  id            INTEGER PRIMARY KEY,
  uuid          BLOB    NOT NULL UNIQUE,   -- stable across DB rebuilds
  name          TEXT    NOT NULL,
  zm_monitor_id INTEGER,                   -- migration: Monitors.Id
  retention_id  INTEGER NOT NULL REFERENCES retention_policies(id)
) STRICT;

CREATE TABLE volumes (
  id         INTEGER PRIMARY KEY,
  path       TEXT    NOT NULL UNIQUE,      -- /var/cache/zoneminder/events, /media/zmoverflow
  capacity   INTEGER NOT NULL,             -- bytes, from statvfs at mount
  target_pct INTEGER NOT NULL DEFAULT 90,  -- retention keeps used <= this
  weight     INTEGER NOT NULL DEFAULT 1,   -- round-robin share for new segments
  enabled    INTEGER NOT NULL DEFAULT 1
) STRICT;

CREATE TABLE retention_policies (
  id             INTEGER PRIMARY KEY,
  name           TEXT NOT NULL,
  min_seconds    INTEGER NOT NULL,         -- never delete younger than this (e.g. 24h)
  max_seconds    INTEGER,                  -- delete once older than this (NULL = disk-driven only)
  keep_detected  INTEGER,                  -- seconds to keep segments overlapping a detection (NULL = same as others)
  thumb_seconds  INTEGER NOT NULL DEFAULT 2592000  -- detection thumbnails outlive video
) STRICT;

-- One row per recorded file. Fixed-length, keyframe-aligned, fMP4/MKV.
CREATE TABLE segments (
  id             INTEGER PRIMARY KEY,
  camera_id      INTEGER NOT NULL REFERENCES cameras(id),
  volume_id      INTEGER NOT NULL REFERENCES volumes(id),
  start_ts       INTEGER NOT NULL,         -- wall clock, microseconds UTC (first frame pts)
  end_ts         INTEGER NOT NULL,         -- last frame pts + duration
  path           TEXT    NOT NULL,         -- relative to volume: cam/<id>/2026/09/27/<start_ts>.mp4
  bytes          INTEGER NOT NULL,
  video_codec    TEXT    NOT NULL,         -- 'hevc' / 'h264'
  width          INTEGER NOT NULL,
  height         INTEGER NOT NULL,
  frames         INTEGER NOT NULL,
  keyframes      INTEGER NOT NULL,
  -- Keyframe index: varint-packed (pts_delta_us, byte_offset_delta, size) per
  -- keyframe fragment, like moonfire's sample_index but keyframes only. ~12 bytes
  -- per GOP -> a 30 s segment at GOP 1 s is ~360 bytes. Enough to build an HLS
  -- byte-range playlist, answer "keyframe at or before t", and seek without
  -- opening the file.
  keyframe_index BLOB    NOT NULL,
  zm_event_id    INTEGER,                  -- migration provenance; NULL for native segments
  flags          INTEGER NOT NULL DEFAULT 0, -- bit0 = closed cleanly, bit1 = has audio, bit2 = pinned (archive)
  UNIQUE (camera_id, start_ts)
) STRICT;
-- Timeline query and retention scan are both range scans on this key.
-- Covering: everything the timeline needs without touching the row.
CREATE INDEX segments_cam_start ON segments (camera_id, start_ts, end_ts, volume_id, bytes);
-- Per-volume oldest-first for retention; per-volume SUM(bytes) for usage.
CREATE INDEX segments_vol_start ON segments (volume_id, start_ts, bytes);

-- Motion / object / audio detections. An interval, not a frame.
CREATE TABLE detections (
  id          INTEGER PRIMARY KEY,
  camera_id   INTEGER NOT NULL REFERENCES cameras(id),
  start_ts    INTEGER NOT NULL,
  end_ts      INTEGER NOT NULL,
  kind        TEXT    NOT NULL,            -- 'motion' | 'object' | 'audio' | 'external'
  label       TEXT,                        -- 'person', 'car', zone name, ...
  score       INTEGER NOT NULL,            -- 0-255 peak, ZM semantics
  bbox        BLOB,                        -- 4 x uint16 normalised 0..10000, of the peak frame
  peak_ts     INTEGER,                     -- pts of the thumbnail frame
  thumb_path  TEXT,                        -- relative to volume: cam/<id>/thumbs/<start_ts>.jpg (~30 KB)
  zm_event_id INTEGER,                     -- migration provenance
  reviewed    INTEGER NOT NULL DEFAULT 0,
  UNIQUE (camera_id, start_ts, kind, label)
) STRICT;
CREATE INDEX detections_cam_start ON detections (camera_id, start_ts, end_ts, kind, score);
CREATE INDEX detections_start     ON detections (start_ts, camera_id, kind, score); -- "last week, all cams"
CREATE INDEX detections_label     ON detections (label, start_ts) WHERE label IS NOT NULL;

-- Coarse review buckets (Frigate review_segments): one row per camera per
-- contiguous run of activity, so the review UI lists hundreds of rows, not
-- tens of thousands of motion blips. Maintained by the detector as it closes
-- a detection; merge if the gap to the previous bucket < N seconds.
CREATE TABLE review_items (
  id          INTEGER PRIMARY KEY,
  camera_id   INTEGER NOT NULL REFERENCES cameras(id),
  start_ts    INTEGER NOT NULL,
  end_ts      INTEGER NOT NULL,
  severity    INTEGER NOT NULL,            -- 0 motion, 1 detection, 2 alert
  labels      TEXT,                        -- JSON array
  thumb_path  TEXT,
  reviewed    INTEGER NOT NULL DEFAULT 0
) STRICT;
CREATE INDEX review_cam_start ON review_items (camera_id, start_ts);
CREATE INDEX review_unreviewed ON review_items (reviewed, start_ts) WHERE reviewed = 0;

-- Optional: user-pinned clips (ZM "Archived"). A pin is a range; the retention
-- daemon skips any segment overlapping an active pin.
CREATE TABLE pins (
  id        INTEGER PRIMARY KEY,
  camera_id INTEGER NOT NULL REFERENCES cameras(id),
  start_ts  INTEGER NOT NULL,
  end_ts    INTEGER NOT NULL,
  note      TEXT
) STRICT;
CREATE INDEX pins_cam_start ON pins (camera_id, start_ts, end_ts);
```

### The two queries

Timeline for camera X between t1 and t2 (what montage review and the event list both are):

```sql
SELECT start_ts, end_ts, volume_id, bytes
  FROM segments
 WHERE camera_id = ?1 AND start_ts < ?3 AND end_ts > ?2
 ORDER BY start_ts;
```

`end_ts > t2` cannot narrow the B-tree, but segments are fixed length, so `start_ts >= t1 - max_segment_len` makes it
a pure range on `segments_cam_start`: cost = rows returned (a day of one camera at 30 s segments is 2,880 rows, ~100
KB from the covering index). The `Events_MonitorId_StartDateTime_idx` comment in the fork (`zm_create.sql.in:294-307`)
is solving exactly this problem for variable-length events; fixed length removes the need for two mirror indexes.

Motion events in the last week with thumbnails:

```sql
SELECT d.camera_id, d.start_ts, d.end_ts, d.label, d.score, d.thumb_path
  FROM detections d
 WHERE d.start_ts >= ?now_minus_7d AND d.kind = 'motion' AND d.score >= ?min
 ORDER BY d.start_ts DESC
 LIMIT 200;
```

One descending range on `detections_start`; `LIMIT` stops it after 200 index entries. Thumbnails are files named in
the row, so there is no `WHERE EventId=? AND Score=MaxScore` second query per row (`web/includes/functions.php:841`)
and no `Frames` join. Playback of a detection is `segments WHERE camera_id=? AND start_ts <= d.start_ts ORDER BY
start_ts DESC LIMIT 1`, then the keyframe blob gives the byte offset to seek to.

### Retention as segment deletes and two-volume round-robin

```
loop every 30 s:
  for v in volumes:
    used = statvfs(v.path)                      -- one syscall, not df, not du
    while used > v.capacity * v.target_pct/100:
      s = SELECT id, path, bytes, camera_id, start_ts FROM segments
           WHERE volume_id = v.id AND start_ts < now - policy.min_seconds
             AND flags & PINNED = 0
           ORDER BY start_ts LIMIT 1           -- segments_vol_start
      unlink(v.path + s.path); DELETE FROM segments WHERE id = s.id
      used -= s.bytes
  also: DELETE detections WHERE end_ts < now - policy.thumb_seconds (and unlink thumbs)
        DELETE segments older than policy.max_seconds regardless of space
```

One unlink and one single-row delete per 240 MB (or per 30 s of one camera) instead of ~5,000 row deletes, 5 trigger
statements, an `rm -rf` and a directory prune. Because `min_seconds`/pins are the only exceptions, the oldest segment
is always the next to go and the free-space oscillation is one segment wide, not 100 events wide. New segments are
placed by the recorder with `volume = argmin(used/capacity/weight)` over enabled volumes, so both volumes fill and
drain at the same rate and nothing is ever copied between them; a volume that goes read-only or full is simply
disabled and its segments age out in place. Day directories are created by the recorder (`cam/<id>/YYYY/MM/DD/`) and
removed by the retention loop when a `readdir` comes back empty, exactly as `Event.pm:555-579` does today but once per
day per camera rather than once per event.

### Comparison

- **moonfire-nvr**: `recording (composite_id, run_offset, flags, sample_file_bytes, start_time_90k, duration_90k,
  video_samples, video_sync_samples, video_sample_entry_id)` plus `recording_playback (composite_id, video_index
  BLOB)` and a `garbage`/`recording_integrity` pair, all in SQLite. The `video_index` is a varint-delta blob of every
  sample's duration and size, which lets it build `moov` for an arbitrary time range on the fly from raw `.mp4`
  sample-file fragments. The design above keeps only keyframes in the blob and keeps segments self-contained fMP4
  files, trading moonfire's arbitrary-range mp4 synthesis for zero custom muxing (the file is playable by anything,
  and by `view_video`/HLS as the fork already does). Moonfire's retention is also per-stream byte budgets with
  oldest-first deletion and a `garbage` table so an unlink can be retried after a crash; worth copying (`flags` bit
  "pending delete" here).
- **Frigate**: `recordings (id, camera, path, start_time, end_time, duration, motion, objects, dBFS, segment_size,
  regions)` at 10 s segments in SQLite, `event (id, label, camera, start_time, end_time, top_score, zones, thumbnail,
  has_clip, has_snapshot, box, area, ...)`, and `review_segment (camera, start_time, end_time, severity, thumb_path,
  data)`. Its retention is per-camera `days` with `mode: all | motion | active_objects`, implemented by deleting
  `recordings` rows/files whose interval has no overlapping event when mode != all. The schema above is closest to
  Frigate's; the additions are the keyframe blob (Frigate re-probes files with ffprobe), an explicit `volumes` table
  with per-volume targets (Frigate has one path), and pins.

---

## 7. Migrating the existing 17 TB of ZM events without re-encoding

Each ZM event is already a single keyframe-aligned fragmented MP4
(`movflags=frag_keyframe+empty_moov+default_base_moof`, `src/zm_videostore.cpp:608-612`, upstream identical) of ~600
s. Treat it as one oversized segment; nothing needs to be rewritten or moved.

Per Events row (one pass over 60k rows, `SELECT Id, MonitorId, StorageId, StartDateTime, EndDateTime, Length, Frames,
AlarmFrames, MaxScore, MaxScoreFrameId, DefaultVideo, DiskSpace, Scheme, Archived FROM Events`):

1. Resolve the path with the same rule as `Event.pm:167-206`: `<Storage.Path>/<MonitorId>/<YYYY-MM-DD of
   StartDateTime, local time>/<Id>/<DefaultVideo>`. Skip rows whose file is missing (zmaudit's job today) and rows
   with `EndDateTime IS NULL`.
2. Keyframe index, in order of cheapness:
   - **[fork]** parse `index.m3u8` if present: `EXT-X-MAP` gives the init segment length, each
     `EXTINF`/`EXT-X-BYTERANGE size@offset` pair is one keyframe fragment with its duration
     (`zm_videostore.cpp:1787-1788`). Text parse, ~1 ms, no video I/O beyond a 20 KB file.
   - otherwise walk the MP4 box tree: read `ftyp`+`moov` (a few KB at offset 0, `empty_moov` so no `stss`/`stco` to
     speak of), then iterate top-level `moof`/`mdat` pairs reading only each `moof` header (`mfhd`+`traf`/`tfdt` for
     the base decode time, `trun` for sample count/durations/sizes) and seeking past `mdat` by its size. ~600 boxes of
     ~200 bytes for a 10-minute event; with `posix_fadvise(RANDOM)` this is ~600 small reads per file, ~1-2 s per
     event on the USB disk cold, seconds of CPU total. The `mfra`/`tfra` trailer that ffmpeg writes
     (`zm_videostore.cpp:1707-1731`) is an even faster path: one read at EOF gives `(time, moof_offset)` for every
     fragment.
   - non-fragmented legacy files (pre-1.36, `stss`+`stco`/`co64` in `moov`): parse `moov` only; if `moov` is at the
     end (no faststart) it is one seek.
   - `ffprobe -show_packets` is the fallback and is 100x slower; do not use it for the bulk pass.
3. `INSERT INTO segments (camera_id, volume_id, start_ts, end_ts, path, bytes, video_codec, width, height, frames,
   keyframes, keyframe_index, zm_event_id, flags)` with `start_ts = StartDateTime` (local -> UTC), `end_ts` from the
   last fragment (`EndDateTime` is only 1 s precision and can be NULL/late), `bytes = stat().st_size` (not
   `DiskSpace`, which counts snapshot/alarm jpgs too), `flags |= PINNED if Archived`.
4. Detections: one row per alarm run, **not** per Frames row. Pull only `SELECT FrameId, Delta, Score FROM Frames
   WHERE EventId=? AND Type='Alarm' ORDER BY FrameId` (~880 rows per event, served by `EventId_FrameId_idx` **[fork]**
   or the `EventId` index **[upstream]**; do not touch `Stats`), coalesce consecutive alarm frames with gaps < ~2 s
   into intervals `(start_ts + Delta_first, start_ts + Delta_last, max Score, peak Delta)`, and insert each as
   `kind='motion'`, `zm_event_id=Id`. Events with `AlarmFrames=0` produce nothing. Thumbnail: copy/hard-link
   `alarm.jpg` (first alarm frame, `zm_event.cpp:632-640`) for the first interval, and `snapshot.jpg` (max-score
   frame) for the interval containing `MaxScoreFrameId`; for the rest leave `thumb_path` NULL and let a lazy
   thumbnailer extract from the keyframe nearest `peak_ts`. Bounding boxes for alarm frames exist only in `Stats`
   (`MinX..MaxY`, one row per zone); if bboxes are wanted, fetch `Stats WHERE EventId=? AND FrameId=?` for the peak
   frame only.
5. Review buckets: one `review_items` row per event that had >= 1 detection, spanning the union of its intervals,
   severity 0.
6. Do not delete anything from MySQL during the pass; run the importer idempotently (`UNIQUE(camera_id,start_ts)`),
   then point the new retention daemon at both volumes with `target_pct` = the current 94, and stop zmfilter/zmaudit.
   The old `Events` directories are left in place under their existing paths (`segments.path` is
   absolute-within-volume so the ZM layout and the new `cam/<id>/YYYY/MM/DD` layout coexist); as the retention loop
   unlinks imported segments it also removes `snapshot.jpg`, `alarm.jpg`, `index.m3u8` and the directory (a
   `zm_event_id IS NOT NULL` branch that does what `delete_files` does).
7. Playback of an imported 10-minute segment is the same code path as a native 30 s one; the HLS/byte-range serving
   the fork already has for `index.m3u8` is exactly what `keyframe_index` reconstructs.

Time budget: 60k files x (1 `stat` + 1 `m3u8` read or ~600 box reads) + 60k x 1 indexed Frames query of ~880 rows = an
hour or two, dominated by cold metadata reads on the USB volume; run it with the old daemons stopped so it is not
racing `MoveTo` copies.

---

## 8. Quick wins on the current install

Ordered by expected effect on the 38% mysqld / slow deletes, each with the evidence and the risk.

1. **Stop the zmaudit orphan scans, or run zmaudit hourly-daily instead of every 15 min.** `zmaudit.pl.in:713-714` and
   `733-734` scan the whole Frames index and the whole Stats table every `ZM_AUDIT_CHECK_INTERVAL` (900 s). Set
   `ZM_AUDIT_CHECK_INTERVAL` to 86400 (Options -> System), or disable zmaudit on the server (`Servers.zmaudit=0`,
   honoured at `zmpkg.pl.in:252-255`) and run `zmaudit.pl -r` by hand. Risk: orphaned Frames/Stats rows from
   `ZM_OPT_FAST_DELETE`-style deletes accumulate until the next run (they are already cleaned inline by
   `Event::delete`, so in practice nothing accumulates); events left `EndDateTime IS NULL` by a crashed zmc are closed
   a day late. Verify first with `EXPLAIN` on the two queries and `SHOW PROCESSLIST` during a pass.
2. **`ZM_RECORD_EVENT_STATS=0`** (`ConfigData.pm.in:1624`). Removes ~27 Stats inserts/s and the same in deletes, and
   the 3-index (upstream) or 1-index (fork) maintenance on a 52M-row table. Nothing on this install reads Stats except
   the per-frame zone popup (`web/ajax/stats.php`) and the `AlarmedZoneId` filter term, which is not in use. Risk:
   none for operations; zone-tuning telemetry is lost going forward. Then `TRUNCATE Stats` (instant; 8 GB back) rather
   than `DELETE`.
3. **Make Frames cheap to delete: apply `zm_update-1.39.27.sql`** (fork) or by hand `ALTER TABLE Frames ADD INDEX
   EventId_FrameId_idx (EventId,FrameId), DROP INDEX EventId, DROP INDEX Type, DROP INDEX TimeStamp` in that order.
   Every reader is anchored on `EventId` and orders by `FrameId` (evidence list in the migration header,
   `1.39.27.sql:1-20`), so the `Type` and `TimeStamp` indexes are pure insert/delete overhead. Expect the 18.5 GB of
   Frames index to drop to ~6-7 GB. Risk: the `ADD INDEX` is an online InnoDB build over 243M rows: hours, extra ~8 GB
   of temp space, and it competes for I/O with recording; do it during a quiet window, and never `DROP` before the
   `ADD` has finished (FK on older installs, `1.39.27.sql:412-415`). Also `1.39.28.sql` for `Stats` if Stats is kept.
   Do **not** add `Frames(EventId,Type)`: nothing filters on Type without also ordering by FrameId, and Type has three
   values.
4. **Shrink the Frames write rate at the source.** The state term at `zm_event.cpp:664-666` is what turns Bulk mode
   off on this fleet. Options without a code change: raise `PostEventCount` is irrelevant (recording is continuous);
   lowering zone sensitivity so the monitor is IDLE more would help both Frames and AlarmFrames but changes what
   counts as an alarm. With a one-line code change (`db_frame` = Bulk or Alarm or first or new-max), Frames rows per
   event fall from ~4,090 to ~AlarmFrames + 135, and the ~45 `UPDATE Events` flushes per event fall to ~10. Risk: the
   score graph in the event view gets sparser between alarms (it is already interpolating between Bulk rows,
   `zm_eventstream.cpp:316-330`).
5. **Retire the `Update DiskSpace` filter** (`Filters` row seeded at `zm_create.sql.in:1231-1245`). `~Event` writes
   `DiskSpace` at close (`zm_event.cpp:325-345`), so the filter finds nothing and only costs a 60 k-row scan per
   minute and one zmfilter process. Risk: events closed by zmaudit after a zmc crash keep `DiskSpace NULL`; zmaudit's
   Storage resync (`zmaudit.pl.in:1116-1140`) tolerates that. Same for the `(EndDateTime,DiskSpace)` index: after the
   filter is gone, replace it with `(EndDateTime,MonitorId)` per `1.39.22.sql`.
6. **Purge on both storages and drop AutoMove.** Change `PurgeWhenFull` to two filters (`StorageId=1 AND
   DiskPercent>=95`, `StorageId=2 AND DiskPercent>=95`), both AutoDelete, and disable `MoveToDiskWhenFull`. New events
   keep landing on storage 1 (per-monitor `StorageId`), so storage 2 would then only drain; to keep both volumes in
   use assign ~14 of the 23 monitors to storage 2 (`Monitors.StorageId`) in proportion to capacity (11:6.3). Effect:
   48 GB/pass of copy traffic disappears, the USB disk stops being the write sink for every byte, and the `SELECT FOR
   UPDATE` held across each copy (`Event.pm:823`) is gone. Risk: retention becomes per-volume rather than pooled (a
   camera on the small volume gets ~7 days if 14 cameras share 6.3 TB - so split by bitrate, not count); the change is
   a config edit, reversible.
7. **Filter intervals and limits.** `ExecuteInterval` is per filter (`Filters.ExecuteInterval`,
   `zmfilter.pl.in:203-209`). The purge at 60 s / limit 100 is fine (24 GB per pass against 0.56 GB/min inflow); the
   7-day filter can run every 3600 s with no loss; each filter is its own Perl process holding a DB connection and
   re-running its `SELECT` on the interval, so fewer filters is directly less load. Risk: none.
8. **InnoDB.** Check `innodb_buffer_pool_size` against the working set: after items 2-3 the hot index set is Events
   (tiny) + the Frames composite index pages for the last hour (inserts) and for the events being purged (20-day-old
   pages, always cold). Deletes will stay I/O-bound regardless of pool size because purge targets old pages; what the
   pool must cover is the current write set plus zmaudit's scans if those are kept. A pool of 8-16 GB on a 48-thread
   box is reasonable; `innodb_flush_log_at_trx_commit=2` is acceptable for Frames (they are reconstructible from the
   mp4 and already lag by `dbQueue` depth); `innodb_io_capacity` should match the DB disk. Risk: `=2` loses up to 1 s
   of DB writes on power loss.
9. **Event_Summaries / triggers.** If production is still on the upstream triggers (`Monitors.TotalEvents` exists,
   `SHOW TRIGGERS` lists `Events_Hour_delete_trigger`), applying `1.39.26.sql.in` removes the 5-lock cascade per
   delete and the `UPDATE Monitors` on every event open. Risk: it is a trigger rewrite with a one-time resync; take a
   `mysqldump --no-data --triggers` first. If production is already on the fork's triggers this is done.
10. **`ZM_OPT_FAST_DELETE`** (`ConfigData.pm.in:1945`): do not enable it. It makes the web delete drop only the Events
    row and leaves the Frames/ Stats rows and the files for zmaudit, which is the scan you are trying to avoid (and
    `Event.pm:495-500` still deletes files when zmaudit runs).
11. **`df` in the purge filter** is one `df` per filter pass, not per event (`Filter.pm:112-116, 617-625`), so there
    is nothing to win there; `Event::DiskSpace` does run `File::Find` per event (`Event.pm:676-691`) but only from
    `UpdateDiskSpace` and `MoveTo`, both removed by items 5-6.

What these do not fix: the per-event directory + 4 files, the `rm -rf` + parent-prune per delete, the variable-length
event rows with two range columns, and retention as an emergent property of thresholds. Those are the reasons for
section 6.
