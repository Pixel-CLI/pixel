// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Contract tests for the action log: oversized fields are cut on a
//! character boundary, a linked `.pixel/` is never written through, the
//! log rotates to its newest lines, and `tail` reads what it can.

use super::*;
use tempfile::tempdir;

fn event_line(args: &str) -> String {
    serde_json::to_string(&ActionEvent::new("search", args)).unwrap()
}

/// Arguments over the cap are cut and marked; the cut never splits a
/// multi-byte character.
#[test]
fn truncate_should_cut_on_a_char_boundary_and_mark_the_cut() {
    assert_eq!(truncate("short", 10), "short");
    assert_eq!(truncate("exact", 5), "exact");
    // "é" is two bytes (1..3): a cut at 2 falls inside it and backs off to 1.
    assert_eq!(truncate("h\u{e9}llo", 2), "h… (truncated)");
    assert_eq!(truncate("\u{e9}\u{e9}", 1), "… (truncated)");
    let long = "x".repeat(MAX_ARGS_LEN + 10);
    let ev = ActionEvent::new("search", long);
    assert_eq!(
        ev.args,
        format!("{}… (truncated)", "x".repeat(MAX_ARGS_LEN))
    );
}

/// An error message over its cap is cut the same way.
#[test]
fn with_result_should_cap_the_error_message() {
    let err = format!("{}tail", "e".repeat(MAX_ERROR_LEN));
    let ev = ActionEvent::new("x", "").with_result(&Err(err), Duration::from_millis(1));
    assert_eq!(
        ev.error.as_deref(),
        Some(format!("{}… (truncated)", "e".repeat(MAX_ERROR_LEN)).as_str())
    );
}

/// A repository that ships `.pixel` as a link gets a logger that writes
/// nothing, rather than one that follows the link.
#[test]
fn spawn_for_root_should_not_write_through_a_linked_pixel_dir() {
    let repo = tempdir().unwrap();
    let elsewhere = tempdir().unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), repo.path().join(".pixel")).unwrap();
    let mut log = ActionLog::spawn_for_root(repo.path());
    log.log(ActionEvent::new("search", "x"));
    log.finish_flush();
    assert!(
        std::fs::read_dir(elsewhere.path())
            .unwrap()
            .next()
            .is_none(),
        "nothing written through the link"
    );
}

/// The writer appends to an existing log rather than replacing it.
#[test]
fn spawn_at_should_append_to_an_existing_log() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("actions.jsonl");
    std::fs::write(&path, format!("{}\n", event_line("before"))).unwrap();
    let mut log = ActionLog::spawn_at(path.clone());
    log.log(ActionEvent::new("search", "after"));
    log.finish_flush();
    let args: Vec<String> = tail(&path, 10)
        .unwrap()
        .into_iter()
        .map(|e| e.args)
        .collect();
    assert_eq!(args, vec!["before", "after"]);
}

/// A log under the size cap is left alone; a missing one is not an error.
#[test]
fn rotate_if_needed_should_leave_small_or_missing_logs_alone() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("actions.jsonl");
    rotate_if_needed(&path).unwrap();
    assert!(!path.exists());
    // More lines than rotation keeps, but far under the byte cap: the size
    // decides, not the line count.
    let small = "1\n".repeat(MAX_KEPT_LINES + 100);
    std::fs::write(&path, &small).unwrap();
    rotate_if_needed(&path).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), small);
}

/// Past the size cap, the log keeps exactly its newest lines, in order.
#[test]
fn rotate_if_needed_should_keep_only_the_newest_lines_past_the_cap() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("actions.jsonl");
    let total = MAX_KEPT_LINES * 3;
    let width = (MAX_LOG_BYTES as usize / total) + 16;
    let mut body = String::new();
    for i in 0..total {
        body.push_str(&format!("{i:0width$}\n"));
    }
    std::fs::write(&path, &body).unwrap();
    assert!(std::fs::metadata(&path).unwrap().len() > MAX_LOG_BYTES);
    rotate_if_needed(&path).unwrap();
    let kept = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = kept.lines().collect();
    assert_eq!(lines.len(), MAX_KEPT_LINES);
    assert_eq!(lines[0], format!("{:0width$}", total - MAX_KEPT_LINES));
    assert_eq!(*lines.last().unwrap(), format!("{:0width$}", total - 1));
}

/// A missing log reads as empty; blank lines are skipped; a limit of zero
/// returns nothing.
#[test]
fn tail_should_treat_missing_and_blank_lines_as_nothing() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("actions.jsonl");
    assert!(tail(&path, 10).unwrap().is_empty());
    std::fs::write(&path, format!("\n   \n{}\n\n", event_line("only"))).unwrap();
    let args: Vec<String> = tail(&path, 10)
        .unwrap()
        .into_iter()
        .map(|e| e.args)
        .collect();
    assert_eq!(args, vec!["only"]);
}

/// A log path that cannot be read (a directory) is an error, not an empty
/// history.
#[test]
fn tail_should_report_an_unreadable_log() {
    let dir = tempdir().unwrap();
    assert!(tail(dir.path(), 10).is_err());
}

/// A pool of zero characters has no savings to report; a snippet larger
/// than the pool saves nothing rather than a negative share.
#[test]
fn savings_ratio_should_refuse_an_empty_pool_and_clamp_at_zero() {
    let ev = ActionEvent::new("search", "x").with_savings(10, 0);
    assert_eq!(ev.savings_ratio(), None);
    let ev = ActionEvent::new("search", "x").with_savings(150, 100);
    assert_eq!(ev.savings_ratio(), Some(0.0));
}
