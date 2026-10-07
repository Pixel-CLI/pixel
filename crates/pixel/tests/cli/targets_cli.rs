// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! CLI round-trip: `gitpixel scope-task` writes the enforcement manifest,
//! `--clear` removes it, `--no-manifest` leaves none.

use std::path::Path;
use std::process::Command;

use crate::support::{Scratch, pixel_command};

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
    pixel_command()
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

fn fixture() -> Scratch {
    let dir = Scratch::for_test("gpx-targets-cli", "manifest");
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
        &["scope-task", "fix `login_user` login flow", ".", "--json"],
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
    assert_eq!(t["id"].as_str().unwrap().len(), 12);
    assert!(t["created_unix"].as_u64().unwrap() > 0);
    let manifest_paths: Vec<&str> = t["targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(manifest_paths, target_paths);

    // Pretty output renders tiers.
    let out = gitpixel(&dir, &["scope-task", "fix `login_user` login flow", "."]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("P0 — primary"), "missing tier header: {text}");
    assert!(text.contains("closed list:"));

    // Clear removes the manifest.
    let out = gitpixel(&dir, &["scope-task", "--clear", "."]);
    assert!(out.status.success(), "clear failed: {out:?}");
    assert!(!manifest.exists(), "manifest not cleared");

    // --no-manifest leaves none behind.
    let out = gitpixel(
        &dir,
        &[
            "scope-task",
            "fix `login_user` login flow",
            ".",
            "--no-manifest",
            "--json",
        ],
    );
    assert!(out.status.success());
    assert!(!manifest.exists());

    // --clear with a task errors.
    let out = gitpixel(&dir, &["scope-task", "some task", ".", "--clear"]);
    assert!(!out.status.success());

    std::fs::remove_dir_all(&dir).ok();
}

/// The session example: "de-flake ... test in upgrade_cli.rs". The named
/// test file must LEAD P0 even though `daemon`, `test` and `cli` match half
/// the tree. The path signal boosts the named file; it must not be diluted
/// by the generic tokens, and it must not demote the lexical targets either
/// (they stay P1 — still visible, no longer primary).
#[test]
fn named_test_file_leads_p0_over_generic_token_matches() {
    let dir = Scratch::for_test("gpx-targets-named-path", "upgrade");
    let files: &[(&str, &str)] = &[
        (
            "crates/pixel/tests/cli/upgrade_cli.rs",
            "fn upgrade_reports_unresponsive_daemon_without_claiming_completion() {\n    let _ = 1;\n}\n",
        ),
        (
            "crates/pixel/tests/cli/support.rs",
            "pub fn fake_recall_daemon() {}\npub fn assert_no_daemon() {}\n",
        ),
        (
            "crates/pixel/tests/cli/main.rs",
            "mod upgrade_cli;\nmod support;\n",
        ),
        (
            "crates/pixel-daemon/src/daemon.rs",
            "pub fn try_daemon_inner() {\n    let timeout_ms = 1500;\n}\n",
        ),
        (
            "crates/pixel-facts/tests/all/facts_integration.rs",
            "pub const TEST_WALL_CLOCK: u64 = 1;\n",
        ),
        (
            "crates/pixel/src/main.rs",
            "pub fn cli_daemon_command() {}\n",
        ),
    ];
    for &(path, content) in files {
        let full = dir.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "fixture"]);

    let out = gitpixel(
        &dir,
        &[
            "scope-task",
            "de-flake upgrade_reports_unresponsive_daemon_without_claiming_completion test in upgrade_cli.rs",
            ".",
            "--json",
            "--no-manifest",
        ],
    );
    assert!(out.status.success(), "scope-task failed: {out:?}");
    let data: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    // The path is lifted out of the keyword bag and reported as its own
    // evidence — the token list and the reason both name it. This is what
    // the baseline (which turned `upgrade_cli.rs` into the keyword `rs`)
    // could not answer.
    assert_eq!(
        data["path_tokens"],
        serde_json::json!(["upgrade_cli.rs"]),
        "the named path must be lifted out of the keywords: {data}"
    );
    let targets = data["targets"].as_array().unwrap();
    let named = targets
        .iter()
        .find(|t| t["path"] == "crates/pixel/tests/cli/upgrade_cli.rs")
        .expect("the named test file must be a target");
    assert!(
        named["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r == "path match: upgrade_cli.rs"),
        "the path must be its own evidence: {named}"
    );
    let p0: Vec<&str> = targets
        .iter()
        .filter(|t| t["tier"] == "P0")
        .filter_map(|t| t["path"].as_str())
        .collect();
    assert!(
        p0.contains(&"crates/pixel/tests/cli/upgrade_cli.rs"),
        "P0 must contain the named test file: {p0:?}"
    );
    assert_eq!(
        p0.first().copied(),
        Some("crates/pixel/tests/cli/upgrade_cli.rs"),
        "the named test file must lead P0: {p0:?}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn regions_manifest_written_beside_targets() {
    let dir = Scratch::for_test("gpx-targets-cli", "regions_written");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/login.rs"),
        "pub fn login_user(name: &str) -> bool {\n    !name.is_empty()\n}\n",
    )
    .unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "fixture"]);
    let regions_path = dir.join(".pixel/regions.json");

    // Run with --regions; both targets.json and regions.json must be written.
    let out = gitpixel(
        &dir,
        &["scope-task", "fix `login_user` login flow", ".", "--regions", "--json"],
    );
    assert!(out.status.success(), "scope-task --regions failed: {out:?}");

    assert!(regions_path.exists(), "regions manifest not written");
    let m: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&regions_path).unwrap()).unwrap();

    // The manifest must carry the four required sections.
    assert!(m.get("regions").is_some(), "missing regions");
    assert!(m.get("conflicts").is_some(), "missing conflicts");
    assert!(m.get("layers").is_some(), "missing layers");
    assert!(m.get("shared_files").is_some(), "missing shared_files");
    assert!(m.get("envelope").is_some(), "missing envelope");

    // Regions must be non-empty (login_user is a P0 symbol).
    let regions = m["regions"].as_array().unwrap();
    assert!(!regions.is_empty(), "regions must not be empty");

    // Each region must carry a line range and context reference.
    for r in regions {
        assert!(r.get("start_line").is_some(), "region missing start_line");
        assert!(r.get("end_line").is_some(), "region missing end_line");
        assert!(r.get("context_ref").is_some(), "region missing context_ref");
        assert!(r.get("file_path").is_some(), "region missing file_path");
    }

    // Layers must be non-empty and ordered.
    let layers = m["layers"].as_array().unwrap();
    assert!(!layers.is_empty(), "layers must not be empty");
    for (i, layer) in layers.iter().enumerate() {
        assert_eq!(layer["level"].as_u64().unwrap(), i as u64);
    }

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn regions_manifest_not_written_without_flag() {
    let dir = Scratch::for_test("gpx-targets-cli", "regions_not_written");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/login.rs"),
        "pub fn login_user(name: &str) -> bool {\n    !name.is_empty()\n}\n",
    )
    .unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "fixture"]);
    let regions_path = dir.join(".pixel/regions.json");

    // Run without --regions; regions.json must NOT be written.
    let out = gitpixel(
        &dir,
        &["scope-task", "fix `login_user` login flow", ".", "--json"],
    );
    assert!(out.status.success(), "scope-task failed: {out:?}");

    assert!(
        !regions_path.exists(),
        "regions manifest should not be written without --regions"
    );

    std::fs::remove_dir_all(&dir).ok();
}
