//! Tiered storage: move whole segments from a primary volume (fast RAID) to
//! an archive volume (big single disk), oldest first, rate limited, verified.
//!
//! Why this and not "put some cameras on the big disk": the most recent
//! footage of EVERY camera stays on the reliable volume; losing the archive
//! disk loses only old footage and never interrupts recording.  Unlike
//! ZoneMinder's AutoMove, a move here is one sequential copy of a 60 s file,
//! throttled, checksummed, indexed atomically, with no database row locked
//! while bytes are in flight and no work done on a 95 %-full disk.
//!
//! Policy per primary storage row: `archive_to` (target storage id),
//! `archive_after_days` (age threshold; segments older than this are moved),
//! and `archive_rate_mbps`.  Space pressure on the primary (over `max_bytes`
//! or under `reserve_bytes`) also triggers moves regardless of age.

use anyhow::{Context, Result};
use sha2::Digest;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::{info, warn};

use crate::db::{now_dts, Db, Segment, Storage};
use crate::mp4::TIMESCALE;
use crate::retention::fs_free_bytes;

pub struct TierStats {
    pub moved: usize,
    pub bytes: u64,
    /// a pass hit its per-pass limit: run again at once rather than after the tick
    pub more: bool,
}

/// Bytes on a primary that are past its `archive_after_days` cutoff and not
/// yet moved (0 when the storage does not archive by age).
pub fn backlog_bytes(db: &Db, st: &Storage) -> Result<i64> {
    match (st.archive_to, st.archive_after_days) {
        (Some(_), Some(days)) => db.storage_bytes_before(st.id, now_dts() - (days * 86_400.0 * TIMESCALE as f64) as i64),
        _ => Ok(0),
    }
}

/// One pass. Bounded work per pass so the loop stays responsive; the copy
/// is rate limited by age and unthrottled under space pressure.
pub fn run_once(db: &Db, max_per_pass: usize) -> Result<TierStats> {
    let storages = db.storages()?;
    let mut stats = TierStats { moved: 0, bytes: 0, more: false };
    for src in storages.iter().filter(|s| s.archive_to.is_some() && !s.read_only) {
        let Some(dst) = storages.iter().find(|s| Some(s.id) == src.archive_to) else {
            warn!(storage = src.id, "archive_to points at a missing storage");
            continue;
        };
        if !crate::retention::storage_mounted(src) {
            warn!(storage = src.id, path = %src.path, "primary volume not mounted; skipping this pass");
            continue;
        }
        if !crate::retention::storage_mounted(dst) || fs_free_bytes(Path::new(&dst.path)).is_err() {
            warn!(storage = dst.id, path = %dst.path, "archive volume not available; skipping this pass");
            continue;
        }
        // candidates by age
        let mut candidates: Vec<Segment> = Vec::new();
        if let Some(days) = src.archive_after_days {
            let cutoff = now_dts() - (days * 86_400.0 * TIMESCALE as f64) as i64;
            candidates = db.archive_candidates(src.id, cutoff, max_per_pass)?;
        }
        // space pressure on the primary: move oldest regardless of age
        let used = db.storage_used_bytes(src.id)?;
        let free = fs_free_bytes(Path::new(&src.path)).unwrap_or(u64::MAX) as i64;
        let pressure = src.max_bytes.map(|m| used > m).unwrap_or(false) || free < src.reserve_bytes;
        if pressure && candidates.len() < max_per_pass {
            for s in db.oldest_segments(src.id, max_per_pass)? {
                if !candidates.iter().any(|c| c.id == s.id) {
                    candidates.push(s);
                }
            }
        }
        if candidates.len() >= max_per_pass {
            stats.more = true;
        }
        for seg in candidates {
            // archive must have room: keep its own reserve
            let dst_free = fs_free_bytes(Path::new(&dst.path)).unwrap_or(0) as i64;
            if dst_free - seg.bytes < dst.reserve_bytes {
                warn!(storage = dst.id, "archive volume under reserve; retention on the archive must free space first");
                stats.more = false;
                break;
            }
            match move_segment(db, src, dst, &seg, !pressure) {
                Ok(()) => {
                    stats.moved += 1;
                    stats.bytes += seg.bytes as u64;
                }
                Err(e) => {
                    warn!(segment = seg.id, path = %seg.path, "archive move failed: {e:#}");
                    stats.more = false;
                    break; // do not hammer a failing disk
                }
            }
        }
    }
    if stats.moved > 0 {
        info!(moved = stats.moved, mb = stats.bytes / 1_000_000, "archived segments");
    }
    Ok(stats)
}

fn move_segment(db: &Db, src: &Storage, dst: &Storage, seg: &Segment, throttle: bool) -> Result<()> {
    let from = PathBuf::from(&src.path).join(&seg.path);
    let to = PathBuf::from(&dst.path).join(&seg.path);
    let tmp = to.with_extension("mp4.part");
    if let Some(p) = to.parent() {
        std::fs::create_dir_all(p)?;
    }
    // bytes per second; 0 = as fast as the disks go (space pressure)
    let rate = if throttle { (src.archive_rate_mbps.max(1) as u64) * 1_000_000 / 8 } else { 0 };
    let hash_src = copy_throttled(&from, &tmp, rate).context("copy")?;
    let hash_dst = hash_file(&tmp).context("verify read")?;
    if hash_src != hash_dst {
        let _ = std::fs::remove_file(&tmp);
        anyhow::bail!("checksum mismatch after copy");
    }
    std::fs::rename(&tmp, &to)?;
    if let Ok(d) = std::fs::File::open(to.parent().unwrap()) {
        let _ = d.sync_all();
    }
    // index switch is a single row update; the file exists at the same
    // relative path on both volumes until the source unlink below
    db.move_segment(seg.id, dst.id)?;
    match std::fs::remove_file(&from) {
        Ok(()) => {}
        Err(e) => warn!(path = %from.display(), "source unlink after archive failed: {e}"),
    }
    if let Some(dir) = from.parent() {
        let _ = std::fs::remove_dir(dir);
    }
    Ok(())
}

/// Sequential copy with fsync at the end and a simple token-bucket throttle.
/// Returns the SHA-256 of the bytes read from the source.
fn copy_throttled(from: &Path, to: &Path, bytes_per_sec: u64) -> Result<[u8; 32]> {
    let mut src = std::fs::File::open(from)?;
    let mut dst = std::fs::File::create(to)?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let start = Instant::now();
    let mut total = 0u64;
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        dst.write_all(&buf[..n])?;
        total += n as u64;
        // throttle: sleep if we are ahead of the allowed rate (0 = unthrottled)
        let allowed = start.elapsed().as_secs_f64() * bytes_per_sec as f64;
        if bytes_per_sec > 0 && total as f64 > allowed {
            let excess = total as f64 - allowed;
            std::thread::sleep(Duration::from_secs_f64(excess / bytes_per_sec as f64));
        }
    }
    dst.sync_all()?;
    Ok(hasher.finalize().into())
}

fn hash_file(p: &Path) -> Result<[u8; 32]> {
    let mut f = std::fs::File::open(p)?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_preserves_bytes_and_hashes() {
        let d = std::env::temp_dir().join(format!("zmng-tier-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&d).unwrap();
        let src = d.join("a.bin");
        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&src, &data).unwrap();
        let dst = d.join("b.bin");
        let h1 = copy_throttled(&src, &dst, 1_000_000_000).unwrap();
        let h2 = hash_file(&dst).unwrap();
        assert_eq!(h1, h2);
        assert_eq!(std::fs::read(&dst).unwrap(), data);
        let _ = std::fs::remove_dir_all(&d);
    }
}
