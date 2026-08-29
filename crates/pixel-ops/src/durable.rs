//! Durable file write utilities — the crash-safety foundation.
//!
//! Every journal/snapshot/recovery write goes through `write_durably`:
//! 1. Write to a temp file (same directory, hidden name).
//! 2. `fsync` the temp file.
//! 3. `rename` temp → final (atomic on POSIX).
//! 4. `fsync` the parent directory (so the rename survives power loss).
//!
//! `write_new_durably` uses `hard_link` instead of `rename` so a race
//! between two processes writing the same path deterministically picks
//! one winner (the loser gets `EEXIST`).

use std::fs;
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

/// Write `data` to `path` atomically: temp → fsync → rename → dir fsync.
/// Overwrites if the file already exists.
pub fn write_durably(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(
        ".tmp-{}-{}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("x"),
        uuid::Uuid::new_v4().as_simple()
    ));

    // Write + fsync the temp file.
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }

    // Rename (atomic on POSIX).
    fs::rename(&tmp, path)?;

    // fsync the directory so the rename is durable.
    fsync_dir(dir)?;

    Ok(())
}

/// Write `data` to `path` only if it does not already exist.
/// Uses `hard_link` from a temp file so concurrent writers race-safely:
/// the first to link wins; others get `AlreadyExists`.
/// Returns `Ok(true)` if created, `Ok(false)` if already existed.
pub fn write_new_durably(path: &Path, data: &[u8]) -> std::io::Result<bool> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(
        ".tmp-{}-{}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("x"),
        uuid::Uuid::new_v4().as_simple()
    ));

    // Write + fsync the temp file.
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }

    // Hard-link temp → final. If final already exists, this fails with
    // AlreadyExists — the race-safe one-shot semantics.
    match fs::hard_link(&tmp, path) {
        Ok(()) => {
            fsync_dir(dir)?;
            // Clean up the temp (the link created a second name for the
            // same inode, so removing tmp doesn't affect the final file).
            let _ = fs::remove_file(&tmp);
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(&tmp);
            Ok(false)
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Ensure a directory exists (creating parents as needed) with mode 0700.
pub fn ensure_dir(path: &Path) -> std::io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .or_else(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                Ok(())
            } else {
                Err(e)
            }
        })
}

/// fsync a directory on Unix.
#[cfg(unix)]
fn fsync_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let f = fs::File::open(path)?;
    let _ = unsafe { libc::fsync(f.as_raw_fd()) };
    Ok(())
}

#[cfg(not(unix))]
fn fsync_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// SHA-256 of a string, returned as lowercase hex.
pub fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

/// State root directory for pixel operations.
/// `$XDG_STATE_HOME/pixel` or `~/.local/state/pixel`.
pub fn state_root() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        PathBuf::from(xdg).join("pixel")
    } else {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".local/state/pixel")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn write_durably_overwrites_existing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file.json");
        write_durably(&path, b"first").unwrap();
        write_durably(&path, b"second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
    }

    #[test]
    fn write_new_durably_creates_once() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("once.json");
        assert!(write_new_durably(&path, b"first").unwrap());
        assert!(!write_new_durably(&path, b"second").unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");
    }

    #[test]
    fn sha256_is_deterministic() {
        assert_eq!(sha256_hex("hello"), sha256_hex("hello"));
        assert_ne!(sha256_hex("hello"), sha256_hex("world"));
        assert_eq!(sha256_hex("hello").len(), 64);
    }
}
