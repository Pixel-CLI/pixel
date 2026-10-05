// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Contract tests for the shared base-shard cache: where it lives, when it
//! is disabled, that a cache key cannot escape the cache directory, and
//! which entries LRU eviction removes.

use super::*;
use std::time::{Duration, SystemTime};

use super::CACHE_TEST_LOCK;

/// Holds the cache lock and restores `XDG_CACHE_HOME` / `HOME` on drop, so
/// a test can point them anywhere without leaking into the next one.
struct Env {
    _guard: std::sync::MutexGuard<'static, ()>,
    xdg: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
    scratch: PathBuf,
}

impl Env {
    fn new(label: &str) -> Env {
        let guard = CACHE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let scratch = std::env::temp_dir().join(format!(
            "pixel-cache-contract-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&scratch);
        fs::create_dir_all(&scratch).unwrap();
        Env {
            _guard: guard,
            xdg: std::env::var_os("XDG_CACHE_HOME"),
            home: std::env::var_os("HOME"),
            scratch,
        }
    }

    fn set(key: &str, value: Option<&std::ffi::OsStr>) {
        // SAFETY: every test that touches these variables holds
        // CACHE_TEST_LOCK, and Drop restores the previous values.
        unsafe {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }

    fn use_xdg(&self) -> PathBuf {
        Env::set("XDG_CACHE_HOME", Some(self.scratch.as_os_str()));
        self.scratch.join("pixel").join("shards")
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        Env::set("XDG_CACHE_HOME", self.xdg.as_deref());
        Env::set("HOME", self.home.as_deref());
        let _ = fs::remove_dir_all(&self.scratch);
    }
}

/// An empty `XDG_CACHE_HOME` disables the cache instead of falling back to
/// a path built from nothing.
#[test]
fn cache_dir_should_be_disabled_when_xdg_cache_home_is_empty() {
    let _env = Env::new("xdg-empty");
    Env::set("XDG_CACHE_HOME", Some(std::ffi::OsStr::new("")));
    assert_eq!(cache_dir(), None);
    assert_eq!(cached_shard_path("abc", "trigram"), None);
}

/// A relative cache path would pollute the current directory and not be
/// shared between worktrees: it disables the cache.
#[test]
fn cache_dir_should_be_disabled_when_xdg_cache_home_is_relative() {
    let _env = Env::new("xdg-rel");
    Env::set(
        "XDG_CACHE_HOME",
        Some(std::ffi::OsStr::new("relative/cache")),
    );
    assert_eq!(cache_dir(), None);
    assert!(
        !Path::new("relative").exists(),
        "nothing created in the CWD"
    );
}

/// Without `XDG_CACHE_HOME`, the cache lives under `$HOME/.cache`.
#[test]
fn cache_dir_should_fall_back_to_home_cache_when_xdg_is_unset() {
    let env = Env::new("home");
    Env::set("XDG_CACHE_HOME", None);
    Env::set("HOME", Some(env.scratch.as_os_str()));
    assert_eq!(
        cache_dir(),
        Some(env.scratch.join(".cache").join("pixel").join("shards"))
    );
}

/// An empty or missing `HOME` (with no XDG) disables the cache.
#[test]
fn cache_dir_should_be_disabled_when_home_is_empty_or_missing() {
    let _env = Env::new("home-empty");
    Env::set("XDG_CACHE_HOME", None);
    Env::set("HOME", Some(std::ffi::OsStr::new("")));
    assert_eq!(cache_dir(), None);
    Env::set("HOME", None);
    assert_eq!(cache_dir(), None);
}

/// The cache directory holds other worktrees' indexes: it is private to the
/// user.
#[cfg(unix)]
#[test]
fn cache_dir_should_be_private_to_the_user() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("mode");
    let expected = env.use_xdg();
    let dir = cache_dir().unwrap();
    assert_eq!(dir, expected);
    let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "{mode:o}");
}

/// A cache that cannot be created (its parent is a file) is disabled rather
/// than failing the build.
#[test]
fn cache_dir_should_be_disabled_when_the_directory_cannot_be_created() {
    let env = Env::new("blocked");
    let file = env.scratch.join("not-a-dir");
    fs::write(&file, b"x").unwrap();
    Env::set("XDG_CACHE_HOME", Some(file.as_os_str()));
    assert_eq!(cache_dir(), None);
}

/// A key carrying separators cannot climb out of the cache directory.
#[test]
fn cached_shard_path_should_stay_inside_the_cache_when_the_key_has_separators() {
    let env = Env::new("traversal");
    let dir = env.use_xdg();
    let p = cached_shard_path("../../etc/passwd", "a/b c").unwrap();
    assert_eq!(p.parent(), Some(dir.as_path()));
    assert_eq!(
        p.file_name().unwrap().to_string_lossy(),
        ".._.._etc_passwd.a_b_c.v1.shard"
    );
}

/// A hit replaces a stale destination and creates its directory.
#[test]
fn try_link_from_cache_should_replace_a_stale_destination_and_create_its_parent() {
    let env = Env::new("stale");
    env.use_xdg();
    let src = env.scratch.join("built.shard");
    fs::write(&src, b"fresh").unwrap();
    link_to_cache(&src, "c0ffee", "trigram");

    let dest = env.scratch.join("wt/.pixel/base.shard");
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    fs::write(&dest, b"stale bytes").unwrap();
    assert!(try_link_from_cache("c0ffee", "trigram", &dest));
    assert_eq!(fs::read(&dest).unwrap(), b"fresh");

    let fresh_dest = env.scratch.join("other/new/base.shard");
    assert!(try_link_from_cache("c0ffee", "trigram", &fresh_dest));
    assert_eq!(fs::read(&fresh_dest).unwrap(), b"fresh");
}

/// With the cache disabled, no lookup succeeds and nothing is published.
#[test]
fn cache_operations_should_be_no_ops_when_the_cache_is_disabled() {
    let env = Env::new("disabled");
    Env::set("XDG_CACHE_HOME", Some(std::ffi::OsStr::new("")));
    let src = env.scratch.join("built.shard");
    fs::write(&src, b"x").unwrap();
    link_to_cache(&src, "oid", "trigram");
    remove_cached("oid", "trigram");
    let dest = env.scratch.join("dest.shard");
    assert!(!try_link_from_cache("oid", "trigram", &dest));
    assert!(!dest.exists());
    evict_cache(0).unwrap();
}

fn aged(path: &Path, bytes: &[u8], secs_ago: u64) {
    fs::write(path, bytes).unwrap();
    let f = fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(SystemTime::now() - Duration::from_secs(secs_ago))
        .unwrap();
}

/// Eviction removes the least recently used entries first and stops as soon
/// as the cache fits; the most recent entry survives.
#[test]
fn evict_cache_should_remove_oldest_first_until_under_the_cap() {
    let env = Env::new("lru");
    let dir = env.use_xdg();
    fs::create_dir_all(&dir).unwrap();
    aged(&dir.join("old.shard"), b"0123456789", 300);
    aged(&dir.join("mid.shard"), b"0123456789", 200);
    aged(&dir.join("new.shard"), b"0123456789", 100);
    evict_cache(20).unwrap();
    assert!(!dir.join("old.shard").exists());
    assert!(dir.join("mid.shard").exists());
    assert!(dir.join("new.shard").exists());
}

/// A cache already under its cap is left whole; directories inside it are
/// neither counted nor deleted.
#[test]
fn evict_cache_should_keep_everything_when_under_the_cap_and_ignore_directories() {
    let env = Env::new("under");
    let dir = env.use_xdg();
    fs::create_dir_all(dir.join("subdir")).unwrap();
    fs::write(dir.join("subdir/inner"), vec![0u8; 100]).unwrap();
    aged(&dir.join("a.shard"), b"0123456789", 50);
    evict_cache(10).unwrap();
    assert!(
        dir.join("a.shard").exists(),
        "exactly at the cap is not over it"
    );
    assert!(dir.join("subdir/inner").exists());
}

/// A cache entry that `try_link_from_cache` just used is the newest and
/// survives the eviction that removes an untouched older one.
#[test]
fn evict_cache_should_spare_an_entry_touched_by_a_cache_hit() {
    let env = Env::new("touch");
    let dir = env.use_xdg();
    fs::create_dir_all(&dir).unwrap();
    let used = cached_shard_path("used", "trigram").unwrap();
    aged(&used, b"0123456789", 500);
    aged(&dir.join("idle.trigram.v1.shard"), b"0123456789", 100);
    assert!(try_link_from_cache(
        "used",
        "trigram",
        &env.scratch.join("d.shard")
    ));
    evict_cache(10).unwrap();
    assert!(used.exists(), "the hit refreshed its LRU position");
    assert!(!dir.join("idle.trigram.v1.shard").exists());
}
