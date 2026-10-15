// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Contract tests for the agent-config rewrite helpers: what `pixel setup`
//! may write into a user's CLAUDE.md / AGENTS.md / settings.json, what it must
//! leave alone, and what a dry run must never touch.

use super::*;
use serde_json::json;

fn home() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

// --- rewrite_agent_config -------------------------------------------------

/// `backup_if_changing` surfaces an unreadable source instead of skipping
/// the backup and letting the caller overwrite it.
#[test]
fn backup_if_changing_should_fail_when_the_source_is_a_directory() {
    let home = home();
    assert!(backup_if_changing(home.path(), b"x").is_err());
}

// --- managed markers and stale blocks ----------------------------------------

// --- hook merging ---------------------------------------------------------

fn nested(command: &str) -> serde_json::Value {
    json!({"matcher": "Bash", "hooks": [{"type": "command", "command": command}]})
}

/// A malformed, non-array hook value is someone's configuration: it is kept
/// beside pixel's entry, not dropped.
#[test]
fn merge_hook_entry_should_wrap_and_keep_a_non_array_value() {
    let other = json!({"matcher": "x", "hooks": []});
    let merged = merge_hook_entry(
        Some(&other),
        "run-hook guard",
        nested("pixel run-hook guard"),
    );
    assert_eq!(merged, json!([other, nested("pixel run-hook guard")]));
}

/// Re-installing replaces pixel's previous entry and keeps every other one.
#[test]
fn merge_hook_entry_should_replace_only_the_marked_entry_when_reinstalling() {
    let existing = json!([nested("other tool"), nested("pixel run-hook guard --old")]);
    let merged = merge_hook_entry(
        Some(&existing),
        "run-hook guard",
        nested("pixel run-hook guard"),
    );
    assert_eq!(
        merged,
        json!([nested("other tool"), nested("pixel run-hook guard")])
    );
    assert_eq!(
        merge_hook_entry(None, "m", nested("pixel m")),
        json!([nested("pixel m")])
    );
}
