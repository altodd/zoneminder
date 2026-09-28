# zmng phase-2 code review (865a740..HEAD)

Scope: 7 commits, `zmng/src/{notify,health,api,zmapi,main,db,detect,retention,mp4}.rs`, `tests/`, `web/app.js`, `deploy/`, `README.md`, `docs/redesign/CUTOVER.md`. Line numbers are HEAD (517bc49). Read-only review; nothing built or run. The uncommitted working-tree edits (objects.rs etc.) are a different agent's phase-3 work and do not touch anything below.

## Findings (ranked)

### HIGH

**H1. MQTT sink deadlocks on connect with a real fleet, and permanently whenever the broker is down for a while.** `src/notify.rs:322-358`.
`AsyncClient::new(opts, 64)` gives a bounded (flume, cap 64) request channel that is drained *only* inside `eventloop.poll()` (verified in rumqttc 0.25.1 `eventloop.rs:133,241`). Both `select!` arms call `publish(...).await` inline, so while a publish is awaiting a free slot, `poll()` is not running and nothing frees a slot:
- ConnAck arm: 1 status + per enabled camera 4 discovery + `recording` + `motion` = 6 msgs/camera. With 23 cameras that is 139 messages > 64: the sink blocks on the ~65th publish before HA ever receives complete discovery, and never polls again. The integration test uses one camera (7 msgs), so it cannot catch this.
- Broker unreachable: `poll()` errors, sleeps 5 s; meanwhile notifications keep arriving in the `rx.recv()` arm and queue in the request channel; after 64 the arm blocks forever, `poll()` is never called again, no reconnect ever happens. The bus receiver also stops draining, so it lags (harmless) but MQTT is dead until process restart, with a single `warn!` in the log.
Fix: never `.await` a publish inside the loop that owns the event loop. Either (a) spawn the event loop in its own task (`tokio::spawn(async move { loop { eventloop.poll().await ... } })`, with a `watch`/`Notify` to signal ConnAck back) and let the sink task publish freely, or (b) use `client.try_publish` for everything and drop with a warn on `TrySendError::Full`. Raise `cap` to at least `8 * cameras`. Add a test with >= 20 cameras through `FakeBroker`, and one where the broker is down for >64 notifications and then comes up.

**H2. `zmng restore` can silently discard committed transactions and does not detect a running service.** `src/health.rs:423-431`, `src/main.rs:197-204`.
- Line 425 renames `zmng.db` to `.before-restore`, then line 427-429 *deletes* `zmng.db-wal`/`-shm`. In WAL mode with `synchronous=NORMAL` the WAL can hold up to ~1000 pages (default autocheckpoint) of committed rows that are not yet in the main file; deleting it makes the "kept" `.before-restore` copy lose the last minutes of segments/events/sessions. Fix: before renaming, open the old DB and run `PRAGMA wal_checkpoint(TRUNCATE)`, or rename the `-wal` alongside the main file; only delete a `-wal` that is empty.
- `--stopped` is a self-attestation, nothing is checked (`main.rs:198`). If the service is running, `rename()` succeeds while zmng keeps writing to the old inode; the restored file diverges silently and the writes are lost at the next restart. `tests/health.rs:120` even performs a restore while `db2` still holds the target open and passes. Fix: try `Connection::open_with_flags(db_path, READ_WRITE)` + `PRAGMA locking_mode=EXCLUSIVE; BEGIN EXCLUSIVE` (fails with SQLITE_BUSY if the service holds the file), or `flock` the db, before renaming. Also fsync the copied file and its directory (`std::fs::copy` + `File::sync_all`).
- `src` may equal `db_path` (rename then copy from a now-missing path leaves the index gone); guard with `same_file`/canonicalize.

### MEDIUM

**M1. `backup()` holds the global DB mutex for the whole `VACUUM INTO`.** `src/health.rs:392-395`, `src/db.rs:356-359`.
`Db` is one `Connection` behind `parking_lot::Mutex`; `with()` locks it for the entire copy. With ~2 MB/day/camera index (ARCHITECTURE 3.1), 23 cameras, 17 days ≈ 0.8 GB written to the USB `backup_dir` the runbook recommends: seconds to tens of seconds during which every recorder `insert_segment`, every detector event write and every API request blocks (segment close on 46 streams stalls; live viewers stall). `spawn_blocking` in `main.rs:329` moves it off the runtime but not off the lock. Fix: open a second `Connection` (WAL readers do not block the writer) just for the backup, or use `rusqlite::backup::Backup::run_to_completion(pages, sleep, ..)` in steps. Also `VACUUM INTO` never fsyncs its output (SQLite docs): `File::open(dest)?.sync_all()` after it, or the daily backup on a USB disk is not crash-safe.

**M2. `thumb` is never populated in any external notification; README and the HA recipe promise it.** `src/detect.rs:609-620`, `README.md` "Event payloads carry … `thumb` (URL)", `deploy/homeassistant.yaml` automation 4 (`image: …{{ trigger.payload_json.thumb }}`).
`EventEnd` is published at 609 *before* `make_thumbnail` is spawned at 619; `EventStart` obviously has no thumb either. So `thumb` is `null` in every webhook/SSE/MQTT `event` JSON, and the HA automation builds `https://host?token=…` (and `null` also lands in the retained `zmng/camera/<id>/event`). The `EventUpdate` variant (`notify.rs:80`) that the doc-comment says carries such changes is never published anywhere. Fix: after `Ok(jpeg)` at 620 also `bus.publish_event(&db, id, Notification::EventUpdate)` (thumb now set via `set_event_thumb`), document `event_update`, and have the HA recipe trigger on it (or on `event_end` once the end is published after the thumbnail). Add an assertion in `detector_end_to_end_on_synthetic_source` that an event notification with a non-null `thumb` arrives.

**M3. Webhook sink misses `service_started` (subscribe-after-publish race).** `src/main.rs:258-265`.
`tokio::spawn(webhook_sink)` returns before the task runs `bus.subscribe()` (`notify.rs:171`); `bus.publish(ServiceStarted)` at 265 goes to zero receivers and is dropped. On a multi-thread runtime it is a coin flip. The test `webhook_posts_json_for_external_notifications` sleeps 100 ms before publishing (`tests/notify.rs:408`), which hides exactly this. Fix: create the `Receiver` in `run()` (`let rx = bus.subscribe()`) and pass it into `webhook_sink`/`mqtt_sink`, or publish `ServiceStarted` after the recorders are up (which also makes it more meaningful).

**M4. SSE ACL snapshot lives for the whole connection.** `src/api.rs:970-975`.
`vis` is computed once; an admin removing a camera from a viewer (or deleting the viewer, session_hours = 336) keeps that browser tab receiving `event_start` with camera name/score/id for the revoked camera until it reconnects. The ES websocket (`zmapi.rs:844`) does it right per notification. Fix: re-evaluate `visible_cameras` per notification (one indexed query, only on delivery), or refresh it on a timer/when `session_user` fails.

**M5. `zmng.service` grants write access to ZoneMinder's event tree; runbook says it is read-only and untouched.** `deploy/zmng.service` (`ReadWritePaths=… /var/cache/zoneminder /media`), `CUTOVER.md` §3 ("read-only legacy storages"), §7.4 ("lower the reserve so retention starts deleting the oldest imported events"), §8 ("ZoneMinder's events were never modified").
Once storages 3/4 are ordinary rows, `retention::run_once` unlinks their files when over budget/under reserve, `tier.rs` could move them, and `doctor` writes probe files into them (`health.rs:250`). §8's rollback promise is false after §7.4. Fix: add a `read_only` flag to `storage` honoured by retention/tiering/doctor, or drop `/var/cache/zoneminder` from `ReadWritePaths` (use `ReadOnlyPaths`) and make §7.4 an explicit, separate, irreversible step.

**M6. `doctor` mutates the index and creates directories.** `src/health.rs:287,299`.
`Db::open` runs `SCHEMA` + `migrate()` + an `INSERT` (`db.rs:341-352`), and 287 does `create_dir_all(thumb_dir)`. A pre-flight run as the wrong user (README's "Operating it" shows plain `zmng doctor`; only install-ubuntu.sh and CUTOVER use `sudo -u zmng`) leaves root-owned `zmng.db`/`-wal`/`-shm`/`thumbs`, and the service then fails to start: the doctor causes the fault it is meant to find. Fix: open with `SQLITE_OPEN_READ_ONLY` in doctor (skip migrations), report "would create" for missing dirs, and add an owner/uid check (`stat(db_path).uid == geteuid()`) as its own line.

### LOW

**L1. Prometheus label values are not escaped.** `src/health.rs:201,213`. Only `"` is replaced; `\` and `\n` in a camera name or storage path break the exposition (admin-supplied, so not a security issue). Escape `\`→`\\`, `\n`→`\n`, `"`→`\"` per the text format instead of replacing.

**L2. Flaky assertion in the MQTT test.** `tests/notify.rs:389-390`. After `publish(CameraUp)`, `wait_for("…/recording")` returns the *latest* publish on that topic; if the sink already sent `ON`, `assert_eq!(…, b"OFF")` fails. Assert only the eventual `ON` (the loop at 391-395 already does).

**L3. `retention::backup_db` is dead.** `src/retention.rs:168-172`. Superseded by `health::backup`; not called anywhere. Delete.

**L4. `report()`/`status` does blocking work on the runtime.** `src/health.rs:91-182` via `api.rs:1005,1013`: 7 DB round-trips plus `statvfs`/`is_dir` per storage. `statvfs` on a hung USB/NFS mount blocks a worker thread; the UI polls every 30 s. Wrap `report()` in `spawn_blocking` (the file already does so for retention). Same for `main.rs:337` in the maintenance loop.

**L5. `mqtt_sink` panics on `client_id = ""`.** `notify.rs:316`: rumqttc `MqttOptions::new` panics on an empty or space-prefixed client id; config-controlled, but it kills the sink task with a stack trace rather than a config error. Validate in `Config` load.

**L6. Stale HA discovery.** Discovery is retained and only ever added (`notify.rs:337-344`); a deleted or disabled camera remains as a device in HA forever, now permanently "unavailable"-looking. Publish an empty retained payload to each `config` topic for cameras that disappear (compare against the previous set on each reconcile tick).

**L7. `EventThumbnail` JPEGs are retained by the broadcast ring.** `notify.rs:129` (cap 256). Slots keep their `Bytes` until overwritten, so up to 256 thumbnails (tens of MB) stay resident regardless of consumers. Publish thumbnails on a separate small channel to the MQTT sink only, or keep the bus JSON-only.

**L8. Tokens in URLs are encouraged.** `deploy/homeassistant.yaml` (`?token=YOUR_LONG_LIVED_TOKEN` in `still_image_url` and the notification `image`), supported by `api.rs:121 token_from_query`. Query-string tokens end up in Caddy access logs, HA logs and phone notification payloads. Prefer HA's `rest`/`generic` with `headers:` (HA generic camera does not support headers; say so and point at the MQTT thumbnail entity instead), and never put an admin token there.

**L9. README contradicts itself on the ES websocket.** `README.md` "Notifications" says `/zm/ws` speaks the ES protocol; the "ZoneMinder-compatible API" paragraph still lists "Not yet: … ES websocket". Also `homeassistant.yaml` header says `sensor.<name>_last_event (attributes = … objects, thumb)` which is empty/null today (see M2).

**L10. `tokio-tungstenite` is a normal dependency but only tests use it.** `Cargo.toml`. Move to `[dev-dependencies]`; it compiles into the release binary otherwise.

**L11. Restore leaves stale rows for files retention deleted after the backup.** `CUTOVER.md` §9 blast-radius paragraph. Self-healing (oldest rows get unlinked as NotFound), but the timeline shows coverage that 404s and `used_bytes` is overstated until then. One sentence in §9, or run `reindex` plus a "prune rows whose file is missing" pass on `restore`.

**L12. `backup()` on an unmounted `backup_dir`.** `health.rs:385-387`: `create_dir_all` happily creates `/media/zmoverflow/zmng-backups` on the root filesystem when the USB disk is not mounted, so the "other volume" backup silently lands on the system SSD (and is shadowed once the disk mounts). Check `backup_dir` is a mount point or that its device differs from `db_path`'s (`st_dev`), or at least refuse when the parent did not exist.

## Tests: what they do and do not cover

Meaningful: `tests/api.rs` (real fMP4 from ffmpeg, viewer 404 matrix over every media route, keyset paging, Range/HLS byte-range check against the actual `moof`), `tests/retention.rs::byte_budget_removes_oldest_first` (directly asserts the fixed behaviour), the ES websocket test (rate limit and invisibility are asserted with a negative timeout), `health.rs::status_reports…` (viewer/admin split and `metrics` 403 asserted).

Gaps that map to findings above: MQTT tested with 1 camera and a broker that is always up (H1); no restore-while-open or WAL-content test (H2; the existing test in fact restores over an open connection and passes); webhook test pre-sleeps to avoid the subscribe race (M3); nothing asserts a non-null `thumb` ever appears (M2); `doctor` test never runs against a DB owned by another uid (M6); SSE test never revokes a camera mid-stream (M4). `tests/health.rs::status_reports…` checks `effective_days` against the response's own `headroom_bytes`, so the headroom formula itself (min(cap−used, free−reserve)) is unverified; add a case with `max_bytes` set. `fragment_samples_roundtrip` covers our writer's fragments; `parse_encoded` exercises ffmpeg's `default_base_moof` output indirectly, fine.

## Verdict

The retention fix, library split, per-camera ACL filtering on the new routes (SSE, status, metrics, ES websocket), credential handling (nothing new leaks to viewers, logs or `ps`; doctor scrubs the password) and the SQL (all parameterised) are sound, and the harness is a real improvement. Two things should block merge: the MQTT sink as written cannot complete discovery for the 23-camera fleet and wedges permanently after a broker outage (H1), and `restore` can throw away the WAL of the copy it claims to keep and cannot tell that the service is still running (H2). Fix those, then the medium items in order (backup lock hold time M1, the phantom `thumb` field that the HA recipe depends on M2, the startup race M3, SSE ACL staleness M4, and the unit/runbook contradiction over ZoneMinder's event tree M5/M6); the lows are cleanups.
