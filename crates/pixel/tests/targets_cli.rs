//! CLI round-trip: `gitpixel targets` writes the enforcement manifest,
//! `--clear` removes it, `--no-manifest` leaves none.

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

fn gitpixel(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_pixel"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

fn fixture() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gpx-targets-cli-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/login.rs"),
        "pub fn login_user(name: &str) -> bool {\n    !name.is_empty()\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/other.rs"),
        "pub fn unrelated() -> u32 {\n    7\n}\n",
    )
    .unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "fixture"]);
    dir
}

#[test]
fn targets_round_trip_manifest_and_clear() {
    let dir = fixture();
    let manifest = dir.join(".pixel/targets.json");

    // Run with --json; manifest must be written and match the target list.
    let out = gitpixel(
        &dir,
        &["targets", "fix `login_user` login flow", ".", "--json"],
    );
    assert!(out.status.success(), "targets failed: {out:?}");
    let data: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let target_paths: Vec<&str> = data["targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["path"].as_str().unwrap())
        .collect();
    assert!(target_paths.contains(&"src/login.rs"));

    assert!(manifest.exists(), "manifest not written");
    let m: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    assert_eq!(m["version"], 2);
    let tasks = m["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 1);
    let t = &tasks[0];
    assert_eq!(t["task"], "fix `login_user` login flow");
    assert!(t["id"].as_str().unwrap().len() == 12);
    assert!(t["created_unix"].as_u64().unwrap() > 0);
    let manifest_paths: Vec<&str> = t["targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(manifest_paths, target_paths);

    // Pretty output renders tiers.
    let out = gitpixel(&dir, &["targets", "fix `login_user` login flow", "."]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("P0 — primary"), "missing tier header: {text}");
    assert!(text.contains("closed list:"));

    // Clear removes the manifest.
    let out = gitpixel(&dir, &["targets", "--clear", "."]);
    assert!(out.status.success(), "clear failed: {out:?}");
    assert!(!manifest.exists(), "manifest not cleared");

    // --no-manifest leaves none behind.
    let out = gitpixel(
        &dir,
        &[
            "targets",
            "fix `login_user` login flow",
            ".",
            "--no-manifest",
            "--json",
        ],
    );
    assert!(out.status.success());
    assert!(!manifest.exists());

    // --clear with a task errors.
    let out = gitpixel(&dir, &["targets", "some task", ".", "--clear"]);
    assert!(!out.status.success());

    std::fs::remove_dir_all(&dir).ok();
}
