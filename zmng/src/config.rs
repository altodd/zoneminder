//! Process configuration (TOML). Cameras, users and storage live in the
//! database; this file only holds what is needed to find the database and
//! bind the server.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// SQLite database path.
    pub db_path: PathBuf,
    /// HTTP bind address.
    pub listen: String,
    /// Directory with the static web UI.
    pub web_dir: PathBuf,
    /// Where event thumbnails and other derived images go.
    pub thumb_dir: PathBuf,
    /// Length of each recording segment file in seconds (closed at the first
    /// keyframe after this much time).
    pub segment_secs: u32,
    /// fsync each segment as it closes.
    pub fsync: bool,
    /// Path to the ffmpeg binary used for substream decode and thumbnails.
    pub ffmpeg: String,
    /// Optional go2rtc base URL (e.g. http://10.10.100.100:1984) for WebRTC
    /// low-latency live view. Recording never depends on it.
    pub go2rtc_url: Option<String>,
    /// Session lifetime in hours.
    pub session_hours: u32,
    /// Log filter (tracing EnvFilter syntax).
    pub log: String,
    /// Secure cookies (set true behind HTTPS).
    pub secure_cookies: bool,
    /// Optional URL that receives a JSON POST when a camera stops recording
    /// (or recovers), for Home Assistant / ntfy / Slack style alerting.
    pub alert_webhook: Option<String>,
    /// Minutes without frames before a camera is alerted as down.
    pub alert_after_minutes: u32,
    /// MQTT broker for Home Assistant (motion/recording topics + discovery).
    pub mqtt: Option<crate::notify::MqttConfig>,
    /// Where the daily index backup (VACUUM INTO) goes; ideally on the other
    /// volume. Default: next to the database.
    pub backup_dir: Option<PathBuf>,
    /// Seconds between scrub-preview tiles taken from the detector's frames
    /// (0 disables). Tiles are ~3 KB, so 5 s ≈ 2 MB per camera-hour.
    pub preview_secs: u32,
    /// External object-detection server (DeepStack / CodeProject.AI API).
    pub objects: Option<crate::objects::ObjectsConfig>,
    /// On-demand H.264 transcode for clients without HEVC decode
    /// (`video.mp4?codec=h264`, `live.mp4?codec=h264`).
    pub transcode: Option<crate::transcode::TranscodeConfig>,
    /// Other zmng servers whose cameras this UI shows (`[[peers]]`).
    pub peers: Vec<crate::peers::PeerConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            db_path: PathBuf::from("zmng.db"),
            listen: "0.0.0.0:8080".into(),
            web_dir: PathBuf::from("web"),
            thumb_dir: PathBuf::from("thumbs"),
            segment_secs: 60,
            fsync: true,
            ffmpeg: "ffmpeg".into(),
            go2rtc_url: None,
            session_hours: 24 * 14,
            log: "info,retina=warn".into(),
            secure_cookies: false,
            alert_webhook: None,
            alert_after_minutes: 3,
            mqtt: None,
            backup_dir: None,
            preview_secs: 5,
            objects: None,
            transcode: None,
            peers: Vec::new(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading config {}: {e}", path.display()))?;
        let cfg: Config = toml::from_str(&text)?;
        for p in &cfg.peers {
            p.validate()?;
        }
        Ok(cfg)
    }
}
