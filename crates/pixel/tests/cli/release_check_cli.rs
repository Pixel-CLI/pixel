// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel check-release`: exit code and output contract against a fixture
//! workspace. The checks themselves are unit-tested in the `pixel-release`
//! crate; this pins what the release workflow relies on:
//! exit 0 only when every check passes, the table on stdout, `--json` as
//! one document, and a malformed version rejected before any file is read.

use std::path::{Path, PathBuf};
use std::process::Output;

use crate::support::pixel_command;

fn fixture(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("pixel-release-check-{tag}-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(dir.join("crates/pixel")).unwrap();
    std::fs::create_dir_all(dir.join("crates/pixel-ops")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/pixel\", \"crates/pixel-ops\"]\nresolver = \"3\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("crates/pixel/Cargo.toml"),
        "[package]\nname = \"pixel-cli\"\nversion = \"0.2.3\"\n\n[[bin]]\nname = \"pixel\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("crates/pixel-ops/Cargo.toml"),
        "[package]\nname = \"pixel-ops\"\nversion = \"0.2.2\"\n",
    )
    .unwrap();
    write_lock(&dir, "0.2.3");
    std::fs::write(
        dir.join("CHANGELOG.md"),
        "# Changelog\n\n## [Unreleased]\n\n## [0.2.3] - 2026-09-12\n\n### Added\n- a thing\n\n## [0.2.2] - 2026-09-11\n- older\n",
    )
    .unwrap();
    dir
}

fn write_lock(dir: &Path, cli_version: &str) {
    std::fs::write(
        dir.join("Cargo.lock"),
        format!(
            "version = 4\n\n[[package]]\nname = \"pixel-cli\"\nversion = \"{cli_version}\"\n\n[[package]]\nname = \"pixel-ops\"\nversion = \"0.2.2\"\n"
        ),
    )
    .unwrap();
}

fn pixel(dir: &Path, args: &[&str]) -> Output {
    pixel_command()
        .args(args)
        .arg("--repo")
        .arg(dir)
        .output()
        .unwrap()
}

#[test]
fn consistent_release_passes_with_the_table_on_stdout() {
    let dir = fixture("ok");
    let out = pixel(&dir, &["check-release", "v0.2.3"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(stdout.starts_with("release-check 0.2.3\n"), "{stdout}");
    assert!(stdout.contains("[ok  ] cli-version:"), "{stdout}");
    assert!(stdout.contains("[ok  ] cargo-lock:"), "{stdout}");
    assert!(stdout.contains("[ok  ] changelog:"), "{stdout}");
    assert!(
        stdout
            .trim_end()
            .ends_with("release-check: all checks passed"),
        "{stdout}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn json_is_one_document_with_the_verdict_and_every_check() {
    let dir = fixture("json");
    let out = pixel(&dir, &["check-release", "--json", "refs/tags/v0.2.3"]);
    assert!(out.status.success(), "{out:?}");
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).expect("single JSON document");
    assert_eq!(doc["version"], "0.2.3");
    assert_eq!(doc["ok"], true);
    let names: Vec<&str> = doc["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["cli-version", "cargo-lock", "changelog", "plugin-versions"]
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn stale_lock_fails_the_command_and_names_the_fix() {
    let dir = fixture("stale");
    write_lock(&dir, "0.2.2");
    let out = pixel(&dir, &["check-release", "0.2.3"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        stdout.contains("[FAIL] cargo-lock: stale: pixel-cli 0.2.2 (manifest 0.2.3)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("[ok  ] cli-version:"),
        "every check still reported: {stdout}"
    );
    assert!(
        stdout.trim_end().ends_with("release-check: FAILED"),
        "{stdout}"
    );
    assert!(stderr.contains("release-check failed"), "{stderr}");

    let json = pixel(&dir, &["check-release", "--json", "0.2.3"]);
    assert_eq!(json.status.code(), Some(1));
    // The report IS the answer (`ok: false` inside it) and the exit status
    // carries the failure, so stdout stays ONE document: the CLI appends a
    // failure envelope only when a failing command wrote nothing
    // (see `json_contract::failing_json_command_answers_with_a_failure_envelope`).
    let doc: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(doc["ok"], false);
    assert_eq!(doc["checks"][1]["ok"], false);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn tag_that_is_not_a_version_is_rejected_before_reading_files() {
    let dir =
        std::env::temp_dir().join(format!("pixel-release-check-absent-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let out = pixel(&dir, &["check-release", "release-candidate"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(out.stdout.is_empty(), "no report for a malformed tag");
    assert!(stderr.contains("is not a version"), "{stderr}");
}
