//! Keep the index in memory.
//!
//! The recorders write close to a terabyte a day through the page cache, and
//! the kernel makes room for it by dropping the pages it thinks are cold. The
//! index is a few GB that is read in bursts (a retention sweep, someone
//! opening last month's events), so parts of it keep getting dropped, and the
//! next query that needs them waits on the disk: measured on securityserver,
//! one scan of the index took 2 s from memory and more than 170 s after
//! eviction. With `pin_index = true` the database and its WAL are mapped and
//! `mlock`ed, which makes those pages unevictable for as long as zmng runs.
//!
//! Needs `LimitMEMLOCK=infinity` in the unit (the default allows 8 MB). When
//! the lock is refused zmng says so once and carries on unpinned.

use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{info, warn};

/// A locked read-only mapping of a whole file; unlocked and unmapped on drop.
pub struct Pinned {
    addr: *mut libc::c_void,
    len: usize,
}

// the mapping is never read or written through this handle
unsafe impl Send for Pinned {}

impl Pinned {
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for Pinned {
    fn drop(&mut self) {
        if self.len > 0 {
            unsafe {
                libc::munmap(self.addr, self.len);
            }
        }
    }
}

/// Map `path` and lock its pages in memory (reading them in if needed).
pub fn pin(path: &Path) -> std::io::Result<Pinned> {
    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len() as usize;
    if len == 0 {
        return Ok(Pinned { addr: std::ptr::null_mut(), len: 0 });
    }
    let addr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, file.as_raw_fd(), 0) };
    if addr == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::mlock(addr, len) } != 0 {
        let e = std::io::Error::last_os_error();
        unsafe {
            libc::munmap(addr, len);
        }
        return Err(e);
    }
    Ok(Pinned { addr, len })
}

/// Pin the index and its WAL from a background thread and follow them as
/// they grow. Never blocks the caller: the first lock reads whatever is not
/// in memory yet.
pub fn spawn(db_path: PathBuf) {
    let wal = PathBuf::from(format!("{}-wal", db_path.display()));
    let _ = std::thread::Builder::new().name("index-pin".into()).spawn(move || {
        let mut held: Vec<(PathBuf, Option<Pinned>)> = vec![(db_path, None), (wal, None)];
        let mut announced = false;
        loop {
            for (path, pinned) in held.iter_mut() {
                let len = std::fs::metadata(&*path).map(|m| m.len() as usize).unwrap_or(0);
                if pinned.as_ref().map(|p| p.len()) == Some(len) {
                    continue;
                }
                // lock the new extent before letting go of the old one, so the
                // pages stay locked throughout
                match pin(path) {
                    Ok(p) => *pinned = Some(p),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => *pinned = None,
                    Err(e) => {
                        warn!(path = %path.display(), "cannot keep the index in memory ({e}); set LimitMEMLOCK=infinity in the unit. Carrying on unpinned");
                        return;
                    }
                }
            }
            if !announced {
                announced = true;
                let mb: usize = held.iter().filter_map(|(_, p)| p.as_ref().map(|p| p.len())).sum::<usize>() / 1_000_000;
                info!(mb, "index pinned in memory");
            }
            std::thread::sleep(Duration::from_secs(60));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resident pages of a mapping, by `mincore`.
    fn resident(p: &Pinned) -> usize {
        let pages = p.len().div_ceil(4096);
        let mut vec = vec![0u8; pages];
        assert_eq!(unsafe { libc::mincore(p.addr, p.len, vec.as_mut_ptr()) }, 0);
        vec.iter().filter(|b| **b & 1 == 1).count()
    }

    #[test]
    fn a_pinned_file_is_resident_and_an_empty_one_is_fine() {
        let d = std::env::temp_dir().join(format!("zmng-pin-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("index");
        std::fs::write(&f, vec![7u8; 300_000]).unwrap();
        let p = pin(&f).unwrap();
        assert_eq!(p.len(), 300_000);
        assert_eq!(resident(&p), 300_000usize.div_ceil(4096));
        drop(p);
        let e = d.join("empty");
        std::fs::write(&e, b"").unwrap();
        assert!(pin(&e).unwrap().is_empty());
        assert!(pin(&d.join("missing")).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
