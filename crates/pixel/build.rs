// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Build provenance for `pixel --version`.
//!
//! Emits `PIXEL_GIT_SHA`, `PIXEL_GIT_DIRTY`, `PIXEL_BUILD_COMMIT`, `PIXEL_BUILD_TARGET`,
//! `PIXEL_RUSTC_VERSION` and `PIXEL_BUILD_DATE` as `rustc-env` so the binary
//! can say which tree it was built from. Every value falls back to
//! `unknown` rather than failing the build: a crates.io/tarball build has no
//! `.git`, and a sandbox may have no `git` on PATH.
//!
//! `PIXEL_BUILD_DATE` honours `SOURCE_DATE_EPOCH` (reproducible builds).
//! The script re-runs when HEAD moves (`.git/HEAD` and the ref it points
//! to) and when the index changes, so the dirty flag tracks `git add` and
//! commits without a `cargo clean`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let repo = manifest_dir.join("../..");

    let sha = git(&repo, &["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let dirty = match git(&repo, &["status", "--porcelain", "--untracked-files=no"]) {
        Some(out) => {
            if out.trim().is_empty() {
                "false"
            } else {
                "true"
            }
        }
        None => "unknown",
    };
    println!("cargo:rustc-env=PIXEL_GIT_SHA={sha}");
    println!("cargo:rustc-env=PIXEL_GIT_DIRTY={dirty}");
    // The one-token form `--version` prints: `<sha>` or `<sha>-dirty`.
    let commit = if dirty == "true" {
        format!("{sha}-dirty")
    } else {
        sha.clone()
    };
    println!("cargo:rustc-env=PIXEL_BUILD_COMMIT={commit}");

    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=PIXEL_BUILD_TARGET={target}");

    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let rustc_version = Command::new(rustc)
        .arg("-V")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map_or_else(
            || "unknown".to_string(),
            |o| String::from_utf8_lossy(&o.stdout).trim().to_string(),
        );
    println!("cargo:rustc-env=PIXEL_RUSTC_VERSION={rustc_version}");

    let epoch = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs())
        });
    let date = epoch.map_or_else(|| "unknown".to_string(), utc_date);
    println!("cargo:rustc-env=PIXEL_BUILD_DATE={date}");

    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    for path in git_watch_paths(&repo) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

/// Run `git -C repo args`, `None` when git is missing, fails, or `repo` is
/// not a repository.
fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `.git/HEAD`, the ref it points to, and `.git/index`, resolved through
/// `git rev-parse --git-dir` so worktrees (where `.git` is a file) work.
fn git_watch_paths(repo: &Path) -> Vec<PathBuf> {
    let Some(git_dir) = git(repo, &["rev-parse", "--git-dir"]) else {
        return Vec::new();
    };
    let git_dir = {
        let p = PathBuf::from(&git_dir);
        if p.is_absolute() { p } else { repo.join(p) }
    };
    let mut paths = vec![git_dir.join("HEAD"), git_dir.join("index")];
    if let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD"))
        && let Some(reference) = head.trim().strip_prefix("ref: ")
    {
        // Refs may live in the common dir for worktrees; try both.
        paths.push(git_dir.join(reference));
        if let Some(common) = git(repo, &["rev-parse", "--git-common-dir"]) {
            let common = PathBuf::from(common);
            let common = if common.is_absolute() {
                common
            } else {
                repo.join(common)
            };
            paths.push(common.join(reference));
        }
    }
    paths.into_iter().filter(|p| p.exists()).collect()
}

/// `YYYY-MM-DD` for a Unix timestamp (civil-from-days, Howard Hinnant).
fn utc_date(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}
