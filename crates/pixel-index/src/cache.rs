// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Shared cache for commit-anchored base shards.
//!
//! Git worktrees at the same commit produce byte-identical base shards (the
//! shard header embeds the commit OID and extractor id, and the indexed
//! content comes straight from the git object store). This module keeps a
//! copy of each built base shard keyed by `<commit_oid>.<extractor_id>.v<VERSION>.shard`
//! under the user's cache directory, so a second worktree at the same commit
//! can hardlink (or copy) the cached shard instead of rebuilding a 343 MB
//! index from scratch.
//!
//! All operations are best-effort: any failure (permissions, disk full,
//! cross-device hardlink) silently falls back to the normal build path.

use std::fs;
use std::path::{Path, PathBuf};

/// Tests share the process-global `XDG_CACHE_HOME` env var, so every test
/// that sets it (here and in `indexset`) holds this lock; the rest of the
/// suite still runs in parallel.
#[cfg(test)]
pub(crate) static CACHE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Default cap for the on-disk shard cache.
pub const DEFAULT_CACHE_CAP_BYTES: u64 = 5_000_000_000; // 5 GB

/// Shard format version — must match `shard.rs`'s VERSION so that a
/// version bump invalidates the cache (prevents cross-version ping-pong).
const CACHE_SHARD_VERSION: u32 = 1;

/// Resolve the cache directory (`$XDG_CACHE_HOME/pixel/shards` or
/// `$HOME/.cache/pixel/shards`), creating it if needed. Returns `None` when
/// no cache directory can be determined (e.g. no `HOME`), which disables
/// the cache entirely.
fn cache_base_dir() -> Option<PathBuf> {
    let base = if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        if xdg.is_empty() {
            return None;
        }
        PathBuf::from(xdg)
    } else if let Ok(home) = std::env::var("HOME") {
        if home.is_empty() {
            return None;
        }
        PathBuf::from(home).join(".cache")
    } else {
        return None;
    };
    // Reject relative paths — a relative cache is worse than no cache
    // (pollutes the CWD and is not shared across worktrees).
    if !base.is_absolute() {
        return None;
    }
    Some(base.join("pixel").join("shards"))
}

/// Returns the shard cache directory, creating it (0700) if needed.
/// Returns `None` when no usable cache directory can be determined —
/// callers must treat `None` as "cache disabled" and fall through to
/// the normal build path.
pub fn cache_dir() -> Option<PathBuf> {
    let dir = cache_base_dir()?;
    if fs::create_dir_all(&dir).is_err() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }
    Some(dir)
}

/// Path of the cached shard for `commit_oid` + `extractor_id`.
/// Returns `None` when the cache is disabled (no HOME/XDG).
/// Includes the shard format VERSION in the key so a bump invalidates
/// stale entries instead of producing a link-then-fail-open loop.
pub fn cached_shard_path(commit_oid: &str, extractor_id: &str) -> Option<PathBuf> {
    let dir = cache_dir()?;
    Some(dir.join(format!(
        "{}.{}.v{}.shard",
        safe_component(commit_oid),
        safe_component(extractor_id),
        CACHE_SHARD_VERSION,
    )))
}

/// Sanitize a string into a filesystem-safe component. Both commit OIDs
/// (hex, but defensive) and extractor ids go through this to prevent
/// path traversal under `cache_dir()`.
fn safe_component(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// If a cached shard for `commit_oid`/`extractor_id` exists, hardlink it to
/// `dest` (falling back to a copy on cross-device errors). Returns `true`
/// when `dest` now holds a usable shard.
///
/// Touches the cache entry's mtime so LRU eviction treats hits as uses.
pub fn try_link_from_cache(commit_oid: &str, extractor_id: &str, dest: &Path) -> bool {
    let Some(src) = cached_shard_path(commit_oid, extractor_id) else {
        return false;
    };
    if !src.exists() {
        return false;
    }
    // Ensure the destination parent exists.
    if let Some(parent) = dest.parent() {
        let _ = fs::create_dir_all(parent);
    }
    // Remove a stale destination first.
    let _ = fs::remove_file(dest);
    if fs::hard_link(&src, dest).is_ok() {
        // Touch the source mtime so LRU sees this as a use.
        touch_mtime(&src);
        return true;
    }
    // Cross-device or permission issue: fall back to a full copy.
    match fs::copy(&src, dest) {
        Ok(_) => {
            touch_mtime(&src);
            true
        }
        Err(_) => {
            // Leave no partial artifact.
            let _ = fs::remove_file(dest);
            false
        }
    }
}

/// After building a shard at `src`, publish it into the cache for reuse by
/// other worktrees at the same commit. Best-effort: logs a warning on
/// failure but never aborts indexing.
///
/// Publish is atomic: copy to a tmp file in the cache dir, fsync, then
/// rename over the final name. On `EEXIST` (another worktree published
/// the same commit+extractor concurrently), the existing entry wins —
/// both contain the same content since the key is (commit, extractor).
pub fn link_to_cache(src: &Path, commit_oid: &str, extractor_id: &str) {
    let Some(dest) = cached_shard_path(commit_oid, extractor_id) else {
        return;
    };
    if let Some(parent) = dest.parent() {
        let _ = fs::create_dir_all(parent);
    }
    // Prefer a hardlink so the cache and the worktree share inodes (no extra
    // disk). If that fails (cross-device), copy to a tmp file then rename.
    if fs::hard_link(src, &dest).is_ok() {
        // Already linked — nothing more to do.
    } else if dest.exists() {
        // Another worktree beat us to it. Same key = same content.
        return;
    } else {
        // Atomic publish: copy to tmp, fsync, rename.
        let tmp = dest.with_extension(format!("tmp.{}", std::process::id()));
        match fs::copy(src, &tmp) {
            Ok(_) => {
                if let Ok(f) = fs::File::open(&tmp) {
                    let _ = f.sync_all();
                }
                if fs::rename(&tmp, &dest).is_err() {
                    let _ = fs::remove_file(&tmp);
                }
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                eprintln!("pixel: warning: could not populate shard cache ({e})");
                return;
            }
        }
    }
    // Best-effort eviction to keep the cache under its cap.
    if let Err(e) = evict_cache(DEFAULT_CACHE_CAP_BYTES) {
        eprintln!("pixel: warning: shard cache eviction failed ({e})");
    }
}

/// Remove a cached shard entry, e.g. when `pixel reindex` needs to force a
/// rebuild. No-op when the cache is disabled or the entry doesn't exist.
pub fn remove_cached(commit_oid: &str, extractor_id: &str) {
    if let Some(path) = cached_shard_path(commit_oid, extractor_id) {
        let _ = fs::remove_file(path);
    }
}

/// Update a file's mtime to now (best-effort LRU touch).
fn touch_mtime(path: &Path) {
    if let Ok(f) = fs::OpenOptions::new().write(true).open(path) {
        let now = std::time::SystemTime::now();
        let _ = f.set_modified(now);
    }
}

/// LRU eviction: walk the cache directory, sort entries by modified time
/// (oldest first), and delete until total size is under `max_bytes`.
/// `try_link_from_cache` touches the source mtime on hit, so frequently-used
/// shards survive eviction.
pub fn evict_cache(max_bytes: u64) -> Result<(), std::io::Error> {
    let Some(dir) = cache_dir() else {
        return Ok(());
    };
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };

    let mut files: Vec<(PathBuf, std::time::SystemTime, u64)> = Vec::new();
    let mut total: u64 = 0;
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.file_type().is_file() {
            continue;
        }
        let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let len = meta.len();
        total += len;
        files.push((path, mtime, len));
    }

    if total <= max_bytes {
        return Ok(());
    }

    // Oldest first.
    files.sort_by_key(|entry| entry.1);
    for (path, _mtime, len) in &files {
        if total <= max_bytes {
            break;
        }
        if fs::remove_file(path).is_ok() {
            total = total.saturating_sub(*len);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use super::CACHE_TEST_LOCK;

    /// Unique temp dir for this test process, used as `XDG_CACHE_HOME` so the
    /// cache is isolated from the real user cache. `tempfile` is not a
    /// dependency of this crate, so we roll our own with `std::env::temp_dir`.
    struct CacheEnv {
        dir: PathBuf,
    }

    impl CacheEnv {
        fn new(label: &str) -> CacheEnv {
            let dir = std::env::temp_dir()
                .join(format!("pixel-cache-test-{label}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            // SAFETY: tests are single-threaded per process; setting an env
            // var here is benign and isolated to this test process.
            unsafe {
                std::env::set_var("XDG_CACHE_HOME", &dir);
            }
            CacheEnv { dir }
        }
    }

    impl Drop for CacheEnv {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn cache_dir_is_created() {
        let _guard = CACHE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _env = CacheEnv::new("dir");
        let dir = cache_dir().expect("cache dir should be created");
        assert!(dir.exists());
        assert!(dir.to_string_lossy().contains("pixel/shards"));
    }

    #[test]
    fn cached_shard_path_format() {
        let _guard = CACHE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _env = CacheEnv::new("path");
        let p = cached_shard_path("abc123", "trigram").unwrap();
        assert_eq!(p.file_name().unwrap(), "abc123.trigram.v1.shard");
    }

    #[test]
    fn link_and_retrieve_roundtrip() {
        let _guard = CACHE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _env = CacheEnv::new("roundtrip");
        let work = std::env::temp_dir().join(format!("pixel-cache-work-{}", std::process::id()));
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(&work).unwrap();
        let src = work.join("base.shard");
        fs::write(&src, b"hello shard").unwrap();

        link_to_cache(&src, "deadbeef", "trigram");

        let dest = work.join("linked.shard");
        assert!(try_link_from_cache("deadbeef", "trigram", &dest));
        assert_eq!(fs::read(&dest).unwrap(), b"hello shard");
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn miss_returns_false() {
        let _guard = CACHE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _env = CacheEnv::new("miss");
        let work = std::env::temp_dir().join(format!("pixel-cache-miss-{}", std::process::id()));
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(&work).unwrap();
        let dest = work.join("nope.shard");
        assert!(!try_link_from_cache("nonexistent", "trigram", &dest));
        assert!(!dest.exists());
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn evict_keeps_under_cap() {
        let _guard = CACHE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _env = CacheEnv::new("evict");
        let dir = cache_dir().unwrap();
        // Write three 10-byte files (30 bytes total), cap at 20.
        for i in 0..3 {
            let p = dir.join(format!("oid{i}.trigram.shard"));
            fs::write(&p, b"0123456789").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o600));
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        evict_cache(20).unwrap();
        let remaining: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        let total: u64 = remaining
            .iter()
            .filter_map(|p| fs::metadata(p).ok())
            .map(|m| m.len())
            .sum();
        assert!(total <= 20, "total {total} should be <= 20");
    }

    #[test]
    fn eexist_republish_keeps_existing() {
        let _guard = CACHE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _env = CacheEnv::new("eexist");
        let work = std::env::temp_dir().join(format!("pixel-cache-eexist-{}", std::process::id()));
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(&work).unwrap();
        let src1 = work.join("shard1");
        let src2 = work.join("shard2");
        fs::write(&src1, b"first").unwrap();
        fs::write(&src2, b"second").unwrap();

        link_to_cache(&src1, "sameoid", "trigram");
        let cached = cached_shard_path("sameoid", "trigram").unwrap();
        assert_eq!(fs::read(&cached).unwrap(), b"first");

        // Second publish with same key: existing entry should win.
        link_to_cache(&src2, "sameoid", "trigram");
        assert_eq!(fs::read(&cached).unwrap(), b"first");
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn remove_cached_deletes_entry() {
        let _guard = CACHE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _env = CacheEnv::new("remove");
        let work = std::env::temp_dir().join(format!("pixel-cache-rm-{}", std::process::id()));
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(&work).unwrap();
        let src = work.join("shard");
        fs::write(&src, b"data").unwrap();
        link_to_cache(&src, "rmoid", "trigram");
        assert!(cached_shard_path("rmoid", "trigram").unwrap().exists());
        remove_cached("rmoid", "trigram");
        assert!(!cached_shard_path("rmoid", "trigram").unwrap().exists());
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn touch_mtime_moves_an_old_mtime_to_now() {
        let path = std::env::temp_dir().join(format!("pixel-cache-touch-{}", std::process::id()));
        fs::write(&path, b"x").unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        touch_mtime(&path);
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let _ = fs::remove_file(&path);
        assert!(
            modified > old + std::time::Duration::from_secs(86_400),
            "{modified:?}"
        );
    }
}

#[cfg(test)]
mod contract_tests;
