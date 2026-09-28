//! Multi-server federation (phase 3): one UI, several recording servers.
//!
//! Each server records its own cameras. A server with `[[peers]]` in its
//! config exposes the others' *read* API under `/api/peers/<name>/api/...`,
//! authenticating to the peer with a token issued there (ideally for a
//! viewer account limited to the cameras this site may see). The browser
//! merges the peers' camera lists into the live wall, review and events
//! views and fetches their media through the proxy, so users need one login
//! and one address. Recording, retention, detection and notifications stay
//! local to each server; nothing is replicated.
//!
//! The proxy is an allow-list: only the camera/event/media/status routes a
//! viewer needs, GET (plus PATCH on events and POST on PTZ, admin only),
//! never the peer's users, tokens, storage or its own peers (no chaining).

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeerConfig {
    /// Short name used in URLs and the UI (`[a-z0-9-]+`).
    pub name: String,
    /// Base URL of the peer, e.g. `http://10.10.100.101:8080`.
    pub url: String,
    /// Bearer token created on the peer (`POST /api/tokens` there).
    pub token: String,
}

impl PeerConfig {
    pub fn validate(&self) -> Result<()> {
        if self.name.is_empty() || !self.name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
            anyhow::bail!("peer name {:?} must match [a-z0-9-]+", self.name);
        }
        let u = url::Url::parse(&self.url).map_err(|e| anyhow::anyhow!("peer {}: bad url: {e}", self.name))?;
        if !matches!(u.scheme(), "http" | "https") {
            anyhow::bail!("peer {}: url must be http(s)", self.name);
        }
        if self.token.trim().is_empty() {
            anyhow::bail!("peer {}: token required", self.name);
        }
        Ok(())
    }
}

/// Which proxied requests are allowed: (method, path under the peer root).
/// Returns whether the caller must be an admin.
pub fn allowed(method: &http::Method, path: &str) -> Option<bool> {
    let p = path.trim_start_matches('/');
    let read_prefixes = ["api/cameras", "api/events", "api/segments", "api/stats", "api/status", "api/health"];
    match *method {
        http::Method::GET | http::Method::HEAD => {
            if p == "api/me" || read_prefixes.iter().any(|x| p == *x || p.starts_with(&format!("{x}/")) || p.starts_with(&format!("{x}?"))) {
                Some(false)
            } else {
                None
            }
        }
        http::Method::PATCH => p.starts_with("api/events/").then_some(false),
        http::Method::POST => (p.starts_with("api/cameras/") && p.ends_with("/ptz")).then_some(true),
        _ => None,
    }
}

/// Response headers worth passing back to the browser.
pub const PASS_HEADERS: &[&str] = &["content-type", "content-length", "content-range", "accept-ranges", "cache-control", "x-first-dts-ms", "etag", "last-modified"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_validation() {
        let ok = PeerConfig { name: "barn-2".into(), url: "http://10.0.0.2:8080".into(), token: "t".into() };
        assert!(ok.validate().is_ok());
        assert!(PeerConfig { name: "Barn".into(), ..ok.clone() }.validate().is_err());
        assert!(PeerConfig { name: "a/b".into(), ..ok.clone() }.validate().is_err());
        assert!(PeerConfig { url: "ftp://x".into(), ..ok.clone() }.validate().is_err());
        assert!(PeerConfig { token: " ".into(), ..ok }.validate().is_err());
    }

    #[test]
    fn proxy_allow_list() {
        let m = http::Method::GET;
        assert_eq!(allowed(&m, "api/cameras"), Some(false));
        assert_eq!(allowed(&m, "/api/cameras/3/video.mp4"), Some(false));
        assert_eq!(allowed(&m, "api/events?limit=5"), Some(false));
        assert_eq!(allowed(&m, "api/segments/9/file.mp4"), Some(false));
        assert_eq!(allowed(&m, "api/me"), Some(false));
        assert_eq!(allowed(&m, "api/users"), None);
        assert_eq!(allowed(&m, "api/tokens"), None);
        assert_eq!(allowed(&m, "api/storages"), None);
        assert_eq!(allowed(&m, "api/peers/x/api/cameras"), None);
        assert_eq!(allowed(&m, "index.html"), None);
        assert_eq!(allowed(&m, "api/camerasx"), None);
        assert_eq!(allowed(&http::Method::PATCH, "api/events/4"), Some(false));
        assert_eq!(allowed(&http::Method::PATCH, "api/cameras/4"), None);
        assert_eq!(allowed(&http::Method::POST, "api/cameras/4/ptz"), Some(true));
        assert_eq!(allowed(&http::Method::POST, "api/cameras"), None);
        assert_eq!(allowed(&http::Method::DELETE, "api/events/4"), None);
    }
}
