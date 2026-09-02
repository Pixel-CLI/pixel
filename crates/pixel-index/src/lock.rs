//! Per-root exclusive build lock via `flock(2)`.
//!
//! When multiple CLI invocations (or CLI + daemon) race to build the same
//! index, the first process acquires an exclusive lock on
//! `root/.pixel/build.lock`, builds the shard, and releases. Concurrent
//! callers block on the lock, then load the already-built shard — no
//! duplicated work, no write races.
//!
//! The lock is advisory (`flock`) and automatically released when the file
//! descriptor is closed (process exit, panic, or explicit drop).

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

use fs2::FileExt;

use crate::index::SHARD_DIR;

/// A guard that holds the build lock. Dropping it releases the lock.
pub struct BuildLock {
    _file: File,
}

impl BuildLock {
    /// Acquire an exclusive lock on `root/.pixel/build.lock`, blocking
    /// until it is available. The `.pixel` directory is created if missing.
    pub fn acquire(root: &Path) -> io::Result<Self> {
        let dir = root.join(SHARD_DIR);
        std::fs::create_dir_all(&dir)?;
        let lock_path = dir.join("build.lock");
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        file.lock_exclusive()?;
        Ok(BuildLock { _file: file })
    }

    /// Try to acquire an exclusive lock without blocking. Returns `None`
    /// if the lock is held by another process.
    pub fn try_acquire(root: &Path) -> io::Result<Option<Self>> {
        let dir = root.join(SHARD_DIR);
        std::fs::create_dir_all(&dir)?;
        let lock_path = dir.join("build.lock");
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(BuildLock { _file: file })),
            Err(_) => Ok(None),
        }
    }
}

impl Drop for BuildLock {
    fn drop(&mut self) {
        // `fs2` unlocks automatically on drop, but be explicit.
        let _ = self._file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_blocks_second_attempt() {
        let dir = std::env::temp_dir().join(format!(
            "pixel-lock-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();

        // First acquire succeeds.
        let _guard = BuildLock::acquire(&dir).unwrap();
        // Try-acquire (non-blocking) should return None while held.
        let second = BuildLock::try_acquire(&dir).unwrap();
        assert!(second.is_none(), "second try_acquire should fail while lock is held");

        // After dropping, try-acquire succeeds.
        drop(_guard);
        let third = BuildLock::try_acquire(&dir).unwrap();
        assert!(third.is_some(), "try_acquire should succeed after lock is released");

        std::fs::remove_dir_all(&dir).ok();
    }
}
