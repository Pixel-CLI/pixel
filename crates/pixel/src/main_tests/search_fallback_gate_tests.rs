// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `empty_exact_page_is_complete`: only a complete first empty page may be
//! replaced by the task-aware fallback — a truncated empty page is partial
//! evidence and must not trigger it.

use super::*;

fn empty_data(truncated: bool) -> Value {
    json!({"matches": [], "match_count": 0, "truncated": truncated})
}

#[test]
fn a_complete_empty_first_page_runs_the_fallback() {
    assert!(empty_exact_page_is_complete(0, &[], &empty_data(false)));
}

#[test]
fn a_truncated_empty_page_does_not_run_the_fallback() {
    // Deleting the `!` on the truncated check would let a partial page be
    // silently replaced by the find-code answer.
    assert!(!empty_exact_page_is_complete(0, &[], &empty_data(true)));
}

#[test]
fn a_later_empty_page_does_not_run_the_fallback() {
    assert!(!empty_exact_page_is_complete(1, &[], &empty_data(false)));
}

#[test]
fn a_nonempty_page_does_not_run_the_fallback() {
    let matches = vec![json!({"path": "a.rs", "line": 1})];
    assert!(!empty_exact_page_is_complete(
        0,
        &matches,
        &empty_data(false)
    ));
}

#[test]
fn an_uncounted_empty_page_does_not_run_the_fallback() {
    let data = json!({"matches": [], "truncated": false});
    assert!(!empty_exact_page_is_complete(0, &[], &data));
}
