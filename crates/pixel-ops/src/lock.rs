//! Repository lock — serialize mutations per repository.
//!
//! Port of usable-git's `repository-lock.ts`. Uses a directory + owner.json
//! pattern: `mkdir` is atomic, so only one process can create the lock dir.
//! Stale lock recovery: if the owning PID is dead, the lock is removed and
//! acquisition retries once.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::durable::{ensure_dir, sha256_hex, state_root};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LockOwner {
    pid: u32,
    token: String,
    acquired_at: String,
    common_directory: String,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("repository is busy (locked by another process)")]
pub struct RepositoryBusyError;

pub struct RepositoryLock {
    lock_dir: PathBuf,
    token: String,
    acquired: bool,
}

impl RepositoryLock {
    /// Acquire a lock for the repo identified by `common_directory`
    /// (typically the `.git` path). Returns `RepositoryBusyError` if
    /// another live process holds the lock.
    pub fn acquire(common_directory: &str) -> Result<Self, RepositoryBusyError> {
        Self::acquire_with_state_root(common_directory, &state_root())
    }

    pub fn acquire_with_state_root(
        common_directory: &str,
        state_root: &Path,
    ) -> Result<Self, RepositoryBusyError> {
        let locks_dir = state_root.join("locks");
        ensure_dir(&locks_dir).map_err(|_| RepositoryBusyError)?;
        let lock_dir = locks_dir.join(format!("{}.lock", sha256_hex(common_directory)));
        let token = uuid::Uuid::new_v4().to_string();

        match Self::try_acquire(&lock_dir, &token, common_directory) {
            Ok(()) => Ok(Self { lock_dir, token, acquired: true }),
            Err(_) => {
                // Maybe stale — try recovery.
                if Self::try_recover_stale(&lock_dir) {
                    match Self::try_acquire(&lock_dir, &token, common_directory) {
                        Ok(()) => Ok(Self { lock_dir, token, acquired: true }),
                        Err(_) => Err(RepositoryBusyError),
                    }
                } else {
                    Err(RepositoryBusyError)
                }
            }
        }
    }

    fn try_acquire(lock_dir: &Path, token: &str, common_dir: &str) -> std::io::Result<()> {
        // mkdir with exclusive mode 0700 — atomic on POSIX.
        fs::DirBuilder::new()
            .recursive(false)
            .mode(0o700)
            .create(lock_dir)?;

        // Write owner.json.
        let owner = LockOwner {
            pid: std::process::id(),
            token: token.to_string(),
            acquired_at: iso_now(),
            common_directory: common_dir.to_string(),
        };
        let json = serde_json::to_vec_pretty(&owner)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let owner_path = lock_dir.join("owner.json");
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&owner_path)?;
        f.write_all(&json)?;
        f.sync_all()?;
        Ok(())
    }

    /// Check if the lock is stale (owning PID is dead) and remove it.
    fn try_recover_stale(lock_dir: &Path) -> bool {
        let owner_path = lock_dir.join("owner.json");
        let data = match fs::read(&owner_path) {
            Ok(d) => d,
            Err(_) => return false,
        };
        let owner: LockOwner = match serde_json::from_slice(&data) {
            Ok(o) => o,
            Err(_) => return false,
        };
        if !is_pid_alive(owner.pid) {
            let _ = fs::remove_dir_all(lock_dir);
            return true;
        }
        false
    }

    /// Release the lock if we still own it.
    pub fn release(&mut self) {
        if !self.acquired {
            return;
        }
        let owner_path = self.lock_dir.join("owner.json");
        if let Ok(data) = fs::read(&owner_path) {
            if let Ok(owner) = serde_json::from_slice::<LockOwner>(&data) {
                if owner.token == self.token {
                    let _ = fs::remove_dir_all(&self.lock_dir);
                }
            }
        }
        self.acquired = false;
    }
}

impl Drop for RepositoryLock {
    fn drop(&mut self) {
        self.release();
    }
}

/// Check if a PID is alive (Unix: `kill(pid, 0)`).
fn is_pid_alive(pid: u32) -> bool {
    // SAFETY: kill(pid, 0) is a standard POSIX signal-0 probe.
    let ret = unsafe { libc::kill(pid as i32, 0) };
    ret == 0
}

fn iso_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn acquire_and_release() {
        let dir = tempdir().unwrap();
        let common = "/test/repo/.git";
        let mut lock = RepositoryLock::acquire_with_state_root(common, dir.path()).unwrap();
        assert!(lock.acquired);
        lock.release();
        // Can re-acquire after release.
        let mut lock2 = RepositoryLock::acquire_with_state_root(common, dir.path()).unwrap();
        lock2.release();
    }

    #[test]
    fn second_acquire_fails() {
        let dir = tempdir().unwrap();
        let common = "/test/repo2/.git";
        let _lock1 = RepositoryLock::acquire_with_state_root(common, dir.path()).unwrap();
        // Second acquire should fail (same PID can't hold twice).
        let result = RepositoryLock::acquire_with_state_root(common, dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn stale_lock_recovered() {
        let dir = tempdir().unwrap();
        let common = "/test/repo3/.git";
        let lock_dir = dir.path().join("locks").join(format!("{}.lock", sha256_hex(common)));
        ensure_dir(&dir.path().join("locks")).unwrap();

        // Simulate a stale lock with a dead PID.
        fs::DirBuilder::new().mode(0o700).create(&lock_dir).unwrap();
        let owner = LockOwner {
            pid: 999999, // almost certainly dead
            token: "stale".to_string(),
            acquired_at: "0".to_string(),
            common_directory: common.to_string(),
        };
        fs::write(
            lock_dir.join("owner.json"),
            serde_json::to_vec_pretty(&owner).unwrap(),
        )
        .unwrap();

        // Should recover and acquire.
        let mut lock = RepositoryLock::acquire_with_state_root(common, dir.path()).unwrap();
        lock.release();
    }
}
