// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Contract tests for the agent-config rewrite helpers: what `pixel setup`
//! may write into a user's settings.json, what it must
//! leave alone, and what a dry run must never touch.

use super::*;
use serde_json::json;

fn home() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

/// `backup_if_changing` surfaces an unreadable source instead of skipping
/// the backup and letting the caller overwrite it.
#[test]
fn backup_if_changing_should_fail_when_the_source_is_a_directory() {
    let home = home();
    assert!(backup_if_changing(home.path(), b"x").is_err());
}

// --- hook merging ---------------------------------------------------------

fn nested(command: &str) -> serde_json::Value {
    json!({"matcher": "Bash", "hooks": [{"type": "command", "command": command}]})
}

/// A malformed, non-array hook value is someone's configuration: it is kept
/// beside pixel's entry, not dropped.
#[test]
fn merge_hook_entry_should_wrap_and_keep_a_non_array_value() {
    let other = json!({"matcher": "x", "hooks": []});
    let merged = merge_hook_entry(Some(&other), "hook stop", nested("pixel hook stop"));
    assert_eq!(merged, json!([other, nested("pixel hook stop")]));
}

/// Re-installing replaces pixel's previous entry and keeps every other one.
#[test]
fn merge_hook_entry_should_replace_only_the_marked_entry_when_reinstalling() {
    let existing = json!([nested("other tool"), nested("pixel hook stop --old")]);
    let merged = merge_hook_entry(Some(&existing), "hook stop", nested("pixel hook stop"));
    assert_eq!(
        merged,
        json!([nested("other tool"), nested("pixel hook stop")])
    );
    let existing = json!([nested("pixel hook stop --old"), nested("other tool")]);
    assert_eq!(
        merge_hook_entry(Some(&existing), "hook stop", nested("pixel hook stop")),
        json!([nested("pixel hook stop"), nested("other tool")])
    );
    assert_eq!(
        merge_hook_entry(None, "m", nested("pixel m")),
        json!([nested("pixel m")])
    );
}

#[test]
fn backup_should_preserve_the_original_permissions_and_bytes() {
    use std::os::unix::fs::PermissionsExt;
    let home = home();
    let source = home.path().join("config.yaml");
    fs::write(&source, "my private settings\n").unwrap();
    for mode in [0o600, 0o640] {
        fs::set_permissions(&source, fs::Permissions::from_mode(mode)).unwrap();
        let backup = backup_if_changing(&source, b"changed").unwrap().unwrap();
        assert_eq!(
            fs::read_to_string(&backup).unwrap(),
            "my private settings\n"
        );
        assert_eq!(
            fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
            mode
        );
    }
}
