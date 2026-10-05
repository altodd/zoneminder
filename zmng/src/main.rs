use zmng::{api, config, db, detect, health, import, notify, recorder, retention, tier};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;

#[derive(Parser)]
#[command(name = "zmng", version, about = "ZoneMinder NG: passthrough NVR core")]
struct Cli {
    /// Config file (TOML)
    #[arg(short, long, default_value = "zmng.toml")]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run recorder, detector, retention and HTTP API
    Run,
    /// Print an example config file
    ExampleConfig,
    /// Add a storage location
    AddStorage {
        path: String,
        /// Hard cap in GB (default: use free-space reserve only)
        #[arg(long)]
        max_gb: Option<f64>,
        /// Always keep this many GB free on the filesystem
        #[arg(long, default_value_t = 50.0)]
        reserve_gb: f64,
        /// Archive segments to this storage id (tiering)
        #[arg(long)]
        archive_to: Option<i64>,
        /// Move segments older than this many days to the archive
        #[arg(long)]
        archive_after_days: Option<f64>,
        /// Never delete or move anything here (imported ZoneMinder events)
        #[arg(long)]
        read_only: bool,
    },
    /// Add a camera
    AddCamera {
        name: String,
        main_url: String,
        #[arg(long)]
        sub_url: Option<String>,
        #[arg(long, default_value_t = 1)]
        storage: i64,
    },
    /// List cameras
    Cameras,
    /// Add a user (prompts for password via --password or ZMNG_PASSWORD)
    AddUser {
        username: String,
        #[arg(long)]
        password: Option<String>,
        #[arg(long, default_value = "viewer")]
        role: String,
        /// Camera ids a viewer may see, comma separated
        #[arg(long)]
        cameras: Option<String>,
    },
    /// Re-adopt segment files that exist on disk but not in the index
    Reindex,
    /// Pre-flight checks: ffmpeg, clock, database, storage volumes, cameras
    Doctor {
        /// Also open an RTSP session to every enabled camera
        #[arg(long)]
        cameras: bool,
        #[arg(long)]
        json: bool,
    },
    /// Write a consistent copy of the index (default: backup_dir or next to the db)
    Backup {
        #[arg(long)]
        to: Option<PathBuf>,
    },
    /// Replace the index with a backup (stop the service first)
    Restore {
        file: PathBuf,
        /// Required: confirms the service is stopped
        #[arg(long)]
        stopped: bool,
    },
    /// Print storage usage
    Stats,
    /// Give existing zmng accounts their ZoneMinder passwords: reads ZoneMinder's
    /// `Users` table as TSV with a header row (`mysql -B zm -e "SELECT * FROM Users"`)
    /// and copies each bcrypt hash to the zmng user of the same name. People keep
    /// their old password; it is re-hashed with argon2 at their first login.
    ImportZmUsers {
        tsv: PathBuf,
        #[arg(long)]
        dry_run: bool,
    },
    /// Import ZoneMinder events (TSV export + files in place). See src/import.rs for the SQL.
    ImportZm {
        /// TSV from `mysql -B -N -e "SELECT ..."`
        tsv: PathBuf,
        /// zmng storage id whose path is the ZoneMinder events root
        #[arg(long)]
        storage: i64,
        /// ZoneMinder MonitorId=zmng camera id pairs, comma separated (default identity)
        #[arg(long, default_value = "")]
        camera_map: String,
        #[arg(long, default_value_t = 480)]
        thumb_width: u32,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        skip_thumbs: bool,
        /// Alarm frames needed for a section to import as a motion event
        /// (fewer = a `zm` marker that event retention does not pin)
        #[arg(long, default_value_t = 1)]
        motion_min_alarm_frames: i64,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = if cli.config.exists() {
        config::Config::load(&cli.config)?
    } else if matches!(cli.cmd, Cmd::ExampleConfig) {
        config::Config::default()
    } else {
        anyhow::bail!("config {} not found (run `zmng example-config > zmng.toml`)", cli.config.display());
    };
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_new(&cfg.log).unwrap_or_else(|_| "info".into()))
        .with_target(false)
        .init();

    match cli.cmd {
        Cmd::ExampleConfig => {
            print!("{}", toml::to_string_pretty(&config::Config::default())?);
            Ok(())
        }
        Cmd::AddStorage { path, max_gb, reserve_gb, archive_to, archive_after_days, read_only } => {
            let db = db::Db::open(&cfg.db_path)?;
            std::fs::create_dir_all(&path).with_context(|| format!("creating {path}"))?;
            let id = db.add_storage(
                &std::fs::canonicalize(&path)?.to_string_lossy(),
                max_gb.map(|g| (g * 1e9) as i64),
                (reserve_gb * 1e9) as i64,
            )?;
            if archive_to.is_some() || archive_after_days.is_some() || read_only {
                let mut m = serde_json::Map::new();
                if let Some(a) = archive_to { m.insert("archive_to".into(), a.into()); }
                if let Some(d) = archive_after_days { m.insert("archive_after_days".into(), d.into()); }
                if read_only { m.insert("read_only".into(), true.into()); }
                db.update_storage(id, &m)?;
            }
            println!("storage {id} added");
            Ok(())
        }
        Cmd::AddCamera { name, main_url, sub_url, storage } => {
            let db = db::Db::open(&cfg.db_path)?;
            let id = db.add_camera(&name, &main_url, sub_url.as_deref(), storage)?;
            println!("camera {id} added");
            Ok(())
        }
        Cmd::Cameras => {
            let db = db::Db::open(&cfg.db_path)?;
            for c in db.cameras()? {
                println!("{:>3} {:<24} enabled={} storage={} main={}", c.id, c.name, c.enabled, c.storage_id, c.main_url);
            }
            Ok(())
        }
        Cmd::AddUser { username, password, role, cameras } => {
            let db = db::Db::open(&cfg.db_path)?;
            let pw = password
                .or_else(|| std::env::var("ZMNG_PASSWORD").ok())
                .ok_or_else(|| anyhow::anyhow!("--password or ZMNG_PASSWORD required"))?;
            let hash = api::hash_password(&pw)?;
            let id = db.add_user(&username, &hash, &role)?;
            if let Some(cs) = cameras {
                let ids: Vec<i64> = cs.split(',').filter_map(|s| s.trim().parse().ok()).collect();
                db.set_user_cameras(id, &ids)?;
            }
            println!("user {id} added");
            Ok(())
        }
        Cmd::Reindex => {
            let db = db::Db::open(&cfg.db_path)?;
            let n = retention::reindex(&db)?;
            println!("adopted {n} segment files");
            Ok(())
        }
        Cmd::Doctor { cameras, json } => {
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
            let checks = rt.block_on(health::doctor(&cfg, cameras));
            if json {
                println!("{}", serde_json::to_string_pretty(&checks)?);
            } else {
                for c in &checks {
                    println!("{:<5} {:<24} {}", format!("{:?}", c.verdict).to_uppercase(), c.name, c.detail);
                }
            }
            if checks.iter().any(|c| c.verdict == health::Verdict::Fail) {
                std::process::exit(1);
            }
            Ok(())
        }
        Cmd::Backup { to } => {
            let dest = to.unwrap_or_else(|| health::backup_path(&cfg));
            health::backup(&cfg.db_path, &dest)?;
            println!("index backed up to {}", dest.display());
            Ok(())
        }
        Cmd::Restore { file, stopped } => {
            if !stopped {
                anyhow::bail!("stop the zmng service first, then re-run with --stopped");
            }
            let _lock = health::ServiceLock::acquire(&cfg.db_path)?; // fails while the service runs
            health::restore(&file, &cfg.db_path)?;
            println!("index restored from {} (previous copy kept as *.before-restore)", file.display());
            Ok(())
        }
        Cmd::Stats => {
            let db = db::Db::open(&cfg.db_path)?;
            for s in db.storages()? {
                let used = db.storage_used_bytes(s.id)?;
                let free = retention::fs_free_bytes(std::path::Path::new(&s.path)).unwrap_or(0);
                println!(
                    "storage {} {} used={:.1} GB free={:.1} GB max={:?} reserve={:.1} GB",
                    s.id,
                    s.path,
                    used as f64 / 1e9,
                    free as f64 / 1e9,
                    s.max_bytes.map(|m| m as f64 / 1e9),
                    s.reserve_bytes as f64 / 1e9
                );
            }
            Ok(())
        }
        Cmd::ImportZmUsers { tsv, dry_run } => {
            let db = db::Db::open(&cfg.db_path)?;
            let text = std::fs::read_to_string(&tsv).with_context(|| format!("reading {}", tsv.display()))?;
            let mut lines = text.lines();
            let header: Vec<&str> = lines.next().unwrap_or_default().split('\t').collect();
            let col = |name: &str| header.iter().position(|h| *h == name).ok_or_else(|| anyhow::anyhow!("no {name} column in {}", tsv.display()));
            let (iu, ip) = (col("Username")?, col("Password")?);
            let (mut done, mut skipped) = (0, 0);
            for line in lines.filter(|l| !l.trim().is_empty()) {
                let cols: Vec<&str> = line.split('\t').collect();
                let (Some(name), Some(hash)) = (cols.get(iu).map(|s| s.trim()), cols.get(ip).map(|s| s.trim())) else { continue };
                let Some(user) = db.user_by_name(name)? else {
                    println!("{name:<28} no zmng account with that name; skipped");
                    skipped += 1;
                    continue;
                };
                if !api::is_bcrypt_hash(hash) {
                    println!("{name:<28} ZoneMinder hash is not bcrypt ({}); skipped", if hash.is_empty() { "empty" } else { &hash[..hash.len().min(4)] });
                    skipped += 1;
                    continue;
                }
                if !dry_run {
                    db.set_password_hash(user.id, hash)?;
                }
                println!("{name:<28} {} ZoneMinder password (bcrypt, re-hashed at first login)", if dry_run { "would take" } else { "takes" });
                done += 1;
            }
            println!("{done} account(s) {}, {skipped} skipped", if dry_run { "would change" } else { "changed" });
            Ok(())
        }
        Cmd::ImportZm { tsv, storage, camera_map, thumb_width, dry_run, skip_thumbs, motion_min_alarm_frames } => {
            let db = db::Db::open(&cfg.db_path)?;
            std::fs::create_dir_all(&cfg.thumb_dir)?;
            let st = import::import_zm(
                &db,
                &import::ImportOpts {
                    tsv: &tsv,
                    storage_id: storage,
                    camera_map: import::parse_camera_map(&camera_map),
                    thumb_dir: &cfg.thumb_dir,
                    thumb_width,
                    dry_run,
                    skip_thumbs,
                    motion_min_alarm_frames,
                },
            )?;
            println!("{st:#?}");
            Ok(())
        }
        Cmd::Run => run(cfg),
    }
}

fn run(cfg: config::Config) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        // held for the life of the process; `restore` refuses to run while it is taken
        let _service_lock = health::ServiceLock::acquire(&cfg.db_path)?;
        let db = db::Db::open(&cfg.db_path)?;
        let closed = db.close_stale_events()?;
        if closed > 0 {
            info!(closed, "closed events left open by previous run");
        }
        let adopted = retention::reindex(&db)?;
        if adopted > 0 {
            info!(adopted, "adopted orphan segment files");
        }
        std::fs::create_dir_all(&cfg.thumb_dir)?;
        let hub = Arc::new(recorder::LiveHub::new());
        let bus = Arc::new(notify::Bus::new());
        // subscribe before anything is published so the sinks see ServiceStarted
        if let Some(url) = cfg.alert_webhook.clone() {
            tokio::spawn(notify::webhook_sink(bus.subscribe(), url));
        }
        if let Some(m) = cfg.mqtt.clone() {
            tokio::spawn(notify::mqtt_sink(bus.subscribe(), db.clone(), m, hub.clone(), bus.clone()));
        }
        bus.publish(notify::Notification::ServiceStarted { version: env!("CARGO_PKG_VERSION").into() });
        let rctx = Arc::new(recorder::RecorderCtx {
            db: db.clone(),
            hub: hub.clone(),
            segment_secs: cfg.segment_secs,
            fsync: cfg.fsync,
        });
        recorder::reconcile(rctx.clone()).await?;

        let dctx = Arc::new(detect::DetectCtx {
            db: db.clone(),
            hub: hub.clone(),
            ffmpeg: cfg.ffmpeg.clone(),
            thumb_dir: cfg.thumb_dir.clone(),
            bus: bus.clone(),
            preview_secs: cfg.preview_secs,
            objects: match cfg.objects.clone() {
                Some(o) => Some(Arc::new(zmng::objects::ObjectDetector::new(o)?)),
                None => None,
            },
        });
        *hub.objects.write() = dctx.objects.clone();
        detect::reconcile(dctx.clone()).await?;

        // periodic: retention + reconcile (picks up camera changes made via API) + alerts + backup
        {
            let db = db.clone();
            let rctx = rctx.clone();
            let dctx = dctx.clone();
            let thumb_dir = cfg.thumb_dir.clone();
            let hub = hub.clone();
            let bus = bus.clone();
            let alert_after = cfg.alert_after_minutes as i64 * 60 * 90_000;
            let backup_to = health::backup_path(&cfg);
            let db_path = cfg.db_path.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
                let mut down: std::collections::HashSet<i64> = std::collections::HashSet::new();
                let mut last_backup = std::time::Instant::now();
                let mut storage_low: std::collections::HashSet<i64> = std::collections::HashSet::new();
                loop {
                    tick.tick().await;
                    // camera-down alerts (state transitions only)
                    let now = db::now_dts();
                    let cams: Vec<(i64, String, i64, bool)> = hub
                        .cams
                        .read()
                        .values()
                        .map(|h| {
                            let c = h.camera.read();
                            let st = h.status.read();
                            (c.id, c.name.clone(), st.last_frame_dts, st.connected)
                        })
                        .collect();
                    for (id, name, last, connected) in cams {
                        let is_down = !connected || (last > 0 && now - last > alert_after) || (last == 0);
                        let was_down = down.contains(&id);
                        if is_down && !was_down && last != 0 {
                            down.insert(id);
                            tracing::warn!(camera = id, %name, "camera down: no frames for {} min", (now - last) / 90_000 / 60);
                            bus.publish(notify::Notification::CameraDown { camera_id: id, name });
                        } else if !is_down && was_down {
                            down.remove(&id);
                            tracing::info!(camera = id, %name, "camera recovered");
                            bus.publish(notify::Notification::CameraUp { camera_id: id, name });
                        }
                    }
                    if last_backup.elapsed() > std::time::Duration::from_secs(24 * 3600) {
                        last_backup = std::time::Instant::now();
                        let (src, p) = (db_path.clone(), backup_to.clone());
                        match tokio::task::spawn_blocking(move || health::backup(&src, &p)).await {
                            Ok(Ok(())) => info!(path = %backup_to.display(), "index backup written"),
                            Ok(Err(e)) => tracing::error!("index backup: {e:#}"),
                            Err(e) => tracing::error!("index backup task: {e}"),
                        }
                    }
                    // storage headroom alerts (state transitions only)
                    for s in db.storages().unwrap_or_default() {
                        let free = retention::fs_free_bytes(std::path::Path::new(&s.path)).unwrap_or(u64::MAX);
                        let low = free < s.reserve_bytes as u64;
                        if low && storage_low.insert(s.id) {
                            tracing::warn!(storage = s.id, path = %s.path, "free space under reserve");
                            bus.publish(notify::Notification::StorageLow { storage_id: s.id, path: s.path.clone(), free_bytes: free, reserve_bytes: s.reserve_bytes as u64 });
                        } else if !low {
                            storage_low.remove(&s.id);
                        }
                    }
                    if let Err(e) = recorder::reconcile(rctx.clone()).await {
                        tracing::error!("reconcile recorders: {e:#}");
                    }
                    if let Err(e) = detect::reconcile(dctx.clone()).await {
                        tracing::error!("reconcile detectors: {e:#}");
                    }
                    let db2 = db.clone();
                    let td = thumb_dir.clone();
                    let r = tokio::task::spawn_blocking(move || retention::run_once(&db2, &td)).await;
                    match r {
                        Ok(Err(e)) => tracing::error!("retention: {e:#}"),
                        Err(e) => tracing::error!("retention task: {e}"),
                        _ => {}
                    }
                }
            });
        }

        // tiering runs on its own: a throttled copy of a backlog must never
        // hold up alerts, reconcile or retention, and a pass that hit its
        // limit runs again at once instead of waiting for the next tick
        {
            let db = db.clone();
            tokio::spawn(async move {
                loop {
                    let db2 = db.clone();
                    let more = match tokio::task::spawn_blocking(move || tier::run_once(&db2, 20)).await {
                        Ok(Ok(st)) => st.more,
                        Ok(Err(e)) => {
                            tracing::error!("tiering: {e:#}");
                            false
                        }
                        Err(e) => {
                            tracing::error!("tiering task: {e}");
                            false
                        }
                    };
                    tokio::time::sleep(std::time::Duration::from_secs(if more { 1 } else { 30 })).await;
                }
            });
        }

        // graceful shutdown: SIGTERM/SIGINT stop every recorder so open segments are closed and indexed
        {
            let hub = hub.clone();
            tokio::spawn(async move {
                let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal");
                tokio::select! {
                    _ = term.recv() => {},
                    _ = tokio::signal::ctrl_c() => {},
                }
                info!("shutdown requested: closing segments");
                let handles: Vec<_> = hub.all_handles();
                for h in &handles {
                    let _ = h.stop.send(true);
                }
                // give sessions time to flush + close (fsync on the blocking pool)
                for _ in 0..40 {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    if handles.iter().all(|h| h.status.read().current_segment.is_none()) {
                        break;
                    }
                }
                std::process::exit(0);
            });
        }

        api::serve(cfg, db, hub, bus).await
    })
}
