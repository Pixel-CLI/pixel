// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The release notice reaches a person at a terminal and nobody else. An
//! agent's command tool and a hook read pixel's stderr through a pipe, and
//! a line there costs context on every call, so the pipe case must print
//! nothing and must not even start a check.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::support::{neutral_home, pixel_command};

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// A cache directory whose state already knows a far newer release, checked
/// just now: the binary has something to say and no reason to fetch.
fn cache_knowing_newer_release(tag: &str) -> (PathBuf, String) {
    let dir = std::env::temp_dir().join(format!("px-notice-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("pixel")).unwrap();
    let state = format!(
        r#"{{"checked_at":{},"latest":"v999.0.0","notified_at":0}}"#,
        now()
    );
    std::fs::write(dir.join("pixel/release-check.json"), &state).unwrap();
    (dir, state)
}

fn state_of(cache: &Path) -> String {
    std::fs::read_to_string(cache.join("pixel/release-check.json")).unwrap()
}

#[test]
fn a_piped_stderr_never_sees_the_notice_nor_starts_a_check() {
    let (cache, state) = cache_knowing_newer_release("pipe");
    let out = pixel_command()
        .args(["doctor", "--list"])
        .env("XDG_CACHE_HOME", &cache)
        .env_remove("CI")
        .env_remove("PIXEL_NO_UPDATE_CHECK")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("is available"), "{stderr}");
    assert!(!stderr.contains("999.0.0"), "{stderr}");
    assert_eq!(state_of(&cache), state, "the state file must be untouched");
    let _ = std::fs::remove_dir_all(cache);
}

/// `pixel` run under a pseudo-terminal through `script`, whose flags differ
/// between the BSD (macOS) and util-linux versions. `None` without `script`.
fn under_a_terminal(exe: &Path, cache: &Path, extra_env: &[(&str, &str)]) -> Option<String> {
    let line = format!("{} doctor --list", exe.display());
    let mut command = Command::new("script");
    if cfg!(target_os = "macos") {
        command.args(["-q", "/dev/null", "/bin/sh", "-c", &line]);
    } else {
        command.args(["-qec", &line, "/dev/null"]);
    }
    command
        .env("PIXEL_DAEMON_AUTO_START", "0")
        .env("HOME", neutral_home())
        .env("XDG_CACHE_HOME", cache)
        .env("NO_COLOR", "1")
        .env_remove("CI")
        .env_remove("PIXEL_NO_UPDATE_CHECK")
        .current_dir(cache)
        .stdin(Stdio::null());
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let out = command.output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).replace('\r', ""))
}

/// The test binary lives in `target/`, which the notice treats as a
/// developer's build and never nags: link it out as an installed copy.
fn installed_copy(dir: &Path) -> PathBuf {
    let exe = dir.join("pixel");
    let built = Path::new(env!("CARGO_BIN_EXE_pixel"));
    if std::fs::hard_link(built, &exe).is_err() {
        std::fs::copy(built, &exe).unwrap();
    }
    exe
}

#[test]
fn a_terminal_sees_the_notice_once_with_its_update_command() {
    let (cache, _) = cache_knowing_newer_release("tty");
    let bin = cache.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let exe = installed_copy(&bin);
    let Some(first) = under_a_terminal(&exe, &cache, &[]) else {
        eprintln!("skipped: no `script` to allocate a terminal");
        return;
    };
    let expected = format!(
        "pixel 999.0.0 is available (this is {}) · curl -fsSL https://github.com/Pixel-CLI/pixel/releases/latest/download/install.sh | PIXEL_INSTALL_DIR='{}' sh\n",
        env!("CARGO_PKG_VERSION"),
        bin.canonicalize().unwrap().display()
    );
    assert!(first.contains(&expected), "{first}");
    // Once a day: the second command of the day says nothing.
    let second = under_a_terminal(&exe, &cache, &[]).unwrap();
    assert!(!second.contains("is available"), "{second}");
    let _ = std::fs::remove_dir_all(cache);
}

#[test]
fn the_opt_out_and_ci_silence_a_terminal_too() {
    for (tag, env) in [
        ("optout", ("PIXEL_NO_UPDATE_CHECK", "1")),
        ("ci", ("CI", "true")),
    ] {
        let (cache, state) = cache_knowing_newer_release(tag);
        let bin = cache.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let exe = installed_copy(&bin);
        let Some(out) = under_a_terminal(&exe, &cache, &[env]) else {
            eprintln!("skipped: no `script` to allocate a terminal");
            return;
        };
        assert!(!out.contains("is available"), "{tag}: {out}");
        assert_eq!(
            state_of(&cache),
            state,
            "{tag}: the state file must be untouched"
        );
        let _ = std::fs::remove_dir_all(cache);
    }
}
