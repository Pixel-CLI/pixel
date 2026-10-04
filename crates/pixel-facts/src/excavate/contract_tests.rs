// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Contract tests for `dig-history` (excavate): path-only and recent
//! listings with their range filters, the snippet budget, and the pure
//! snippet and comment helpers.

use super::*;
use crate::ingest::{IngestOptions, ingest_until_fresh_within};
use crate::testutil::git_env;
use std::path::Path;
use tempfile::TempDir;

/// Under cargo-mutants' automatic timeout, so a broken ingest loop fails.
const TEST_WALL_CLOCK: std::time::Duration = std::time::Duration::from_secs(5);

/// Commit with a fixed, strictly increasing date so recency is exact.
fn commit_at(root: &Path, n: u32, msg: &str) {
    let date = format!("2026-01-01T00:{n:02}:00Z");
    git_env(root, &["add", "-A"], &[]);
    git_env(
        root,
        &["commit", "-q", "-m", msg],
        &[("GIT_AUTHOR_DATE", &date), ("GIT_COMMITTER_DATE", &date)],
    );
}

fn ingest(root: &Path) -> FactsStore {
    let mut store = FactsStore::open(root).unwrap();
    ingest_until_fresh_within(&mut store, &IngestOptions::default(), TEST_WALL_CLOCK).unwrap();
    store
}

/// c1 adds a.txt and b.txt, c2 edits a.txt, c3 deletes b.txt.
fn history() -> (TempDir, Vec<String>) {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    git_env(root, &["init", "-q", "-b", "main"], &[]);
    std::fs::write(root.join("a.txt"), "alpha\n").unwrap();
    std::fs::write(root.join("b.txt"), "bravo\n").unwrap();
    commit_at(root, 1, "add a and b");
    std::fs::write(root.join("a.txt"), "alpha\nmore\n").unwrap();
    commit_at(root, 2, "extend a");
    std::fs::remove_file(root.join("b.txt")).unwrap();
    commit_at(root, 3, "drop b");
    let oids = ["HEAD~2", "HEAD~1", "HEAD"]
        .iter()
        .map(|r| git_env(root, &["rev-parse", r], &[]))
        .collect();
    (dir, oids)
}

fn subjects(result: &ExcavateResult) -> Vec<(String, String)> {
    result
        .candidates
        .iter()
        .map(|c| (c.subject.clone(), c.path.clone()))
        .collect()
}

// --- path-only and recent listings ---------------------------------------------

#[test]
fn excavate_should_list_a_paths_history_newest_first_when_no_phrase_is_given() {
    let (dir, _) = history();
    let store = ingest(dir.path());
    let result = store.excavate(None, Some("a.txt"), None, None, 10).unwrap();
    assert_eq!(
        subjects(&result),
        vec![
            ("extend a".to_string(), "a.txt".to_string()),
            ("add a and b".to_string(), "a.txt".to_string()),
        ]
    );
    for c in &result.candidates {
        assert!(c.phrase_present && !c.suspect && !c.is_definition);
        assert!(!c.deleted_from_head);
        assert_eq!(c.snippet, None);
    }
    assert_eq!(result.path.as_deref(), Some("a.txt"));
    assert_eq!(result.last_good.as_ref().unwrap().subject, "extend a");
}

#[test]
fn excavate_should_flag_a_path_deleted_from_head_in_its_history() {
    let (dir, _) = history();
    let store = ingest(dir.path());
    let result = store.excavate(None, Some("b.txt"), None, None, 10).unwrap();
    let rows: Vec<(String, String, bool)> = result
        .candidates
        .iter()
        .map(|c| (c.subject.clone(), c.status.clone(), c.deleted_from_head))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("drop b".to_string(), "D".to_string(), true),
            ("add a and b".to_string(), "A".to_string(), true),
        ]
    );
}

#[test]
fn excavate_should_list_recent_changes_across_paths_up_to_the_limit() {
    let (dir, _) = history();
    let store = ingest(dir.path());
    let result = store.excavate(None, None, None, None, 2).unwrap();
    assert_eq!(
        subjects(&result),
        vec![
            ("drop b".to_string(), "b.txt".to_string()),
            ("extend a".to_string(), "a.txt".to_string()),
        ]
    );
    assert!(result.candidates[0].deleted_from_head);
    assert!(!result.candidates[1].deleted_from_head);
    assert!(result.candidates.iter().all(|c| !c.phrase_present));
    assert_eq!(
        result.last_good, None,
        "no phrase, nothing is a restore point"
    );
    assert!(result.next.contains("--show <oid>"), "{}", result.next);
}

#[test]
fn excavate_should_keep_only_the_older_bound_and_newer_commits_with_a_lone_from() {
    let (dir, oids) = history();
    let store = ingest(dir.path());
    let result = store
        .excavate(None, None, Some(&oids[1]), None, 10)
        .unwrap();
    let mut got: Vec<String> = result
        .candidates
        .iter()
        .map(|c| c.subject.clone())
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec!["drop b", "extend a"],
        "c1 is a proper ancestor of --from"
    );
}

#[test]
fn excavate_should_keep_only_ancestors_of_a_lone_to_bound() {
    let (dir, oids) = history();
    let store = ingest(dir.path());
    let result = store
        .excavate(None, Some("a.txt"), None, Some(&oids[0]), 10)
        .unwrap();
    assert_eq!(
        subjects(&result),
        vec![("add a and b".to_string(), "a.txt".to_string())]
    );
}

#[test]
fn excavate_should_refuse_a_range_ref_that_is_not_a_valid_ref_name() {
    let (dir, _) = history();
    let store = ingest(dir.path());
    let err = store
        .excavate(None, None, Some("--upload-pack=x"), None, 10)
        .unwrap_err()
        .to_string();
    assert!(err.contains("invalid --from ref"), "{err}");
    let err = store
        .excavate(None, None, None, Some("-x"), 10)
        .unwrap_err()
        .to_string();
    assert!(err.contains("invalid --to ref"), "{err}");
}

// --- snippet budget ---------------------------------------------------------------

#[test]
fn excavate_should_carry_snippets_on_the_top_five_and_keep_last_goods_snippet() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    git_env(root, &["init", "-q", "-b", "main"], &[]);
    for n in 1..=6 {
        std::fs::write(root.join(format!("f{n}.txt")), "zebracorn here\n").unwrap();
        commit_at(root, n, &format!("add f{n}"));
    }
    let store = ingest(root);
    let result = store
        .excavate(Some("zebracorn"), None, None, None, 10)
        .unwrap();
    assert_eq!(result.candidates.len(), 6);
    let with_snippet = result
        .candidates
        .iter()
        .filter(|c| c.snippet.is_some())
        .count();
    assert_eq!(with_snippet, 5);
    assert_eq!(result.candidates[5].subject, "add f1");
    assert_eq!(
        result.candidates[5].snippet, None,
        "the sixth is metadata-only"
    );
    let note = result.snippet_note.as_deref().unwrap();
    assert!(
        note.starts_with(
            "1 candidate(s) are metadata-only (snippets carry only the top 5 matches, 40 KB total)"
        ),
        "{note}"
    );
    let lg = result.last_good.as_ref().unwrap();
    assert_eq!(lg.subject, "add f6");
    assert_eq!(lg.snippet.as_deref(), Some("zebracorn here\n"));
    assert!(
        result
            .next
            .contains(&format!("--show {} --file f6.txt", lg.oid))
    );
    assert_eq!(result.plan[0], format!("{}:f6.txt", lg.oid));
}

#[test]
fn excavate_should_restrict_a_phrase_search_to_the_requested_path() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    git_env(root, &["init", "-q", "-b", "main"], &[]);
    std::fs::write(root.join("x.txt"), "quokka\n").unwrap();
    std::fs::write(root.join("y.txt"), "quokka\n").unwrap();
    commit_at(root, 1, "add both");
    let store = ingest(root);
    let result = store
        .excavate(Some("quokka"), Some("y.txt"), None, None, 10)
        .unwrap();
    let paths: Vec<&str> = result.candidates.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths, vec!["y.txt"]);
}

// --- pure helpers ---------------------------------------------------------------------

#[test]
fn range_filter_should_allow_members_within_and_non_members_when_excluding() {
    let set: HashSet<String> = ["a".to_string()].into_iter().collect();
    assert!(RangeFilter::Within(set.clone()).allows("a"));
    assert!(!RangeFilter::Within(set.clone()).allows("b"));
    assert!(!RangeFilter::Excluding(set.clone()).allows("a"));
    assert!(RangeFilter::Excluding(set).allows("b"));
}

#[test]
fn phrase_removed_between_should_skip_empty_units() {
    let units = vec![String::new(), "gone".to_string()];
    assert_eq!(
        phrase_removed_between("gone soon", "", &units),
        Some("gone".to_string())
    );
}

#[test]
fn phrase_outside_comment_should_ignore_every_line_comment_style() {
    for comment in ["/// uses foo", "//! foo", "  // foo", "# foo"] {
        assert!(!phrase_outside_comment(comment, "foo"), "{comment}");
    }
    assert!(phrase_outside_comment("// foo\nfn foo() {}", "foo"));
}

#[test]
fn snippet_should_show_the_first_120_chars_when_the_needle_is_absent() {
    let text = "x".repeat(200);
    assert_eq!(snippet(&text, "needle"), "x".repeat(120));
}

#[test]
fn snippet_should_mark_each_elided_end() {
    assert_eq!(snippet("needle at start", "needle"), "needle at start");
    let long = format!("{}needle{}", "a".repeat(30), "b".repeat(200));
    let s = snippet(&long, "NEEDLE");
    assert!(s.starts_with(&format!("…{}needle", "a".repeat(20))), "{s}");
    assert!(s.ends_with('…'), "{s}");
    assert_eq!(
        s.chars().count(),
        1 + 20 + 120 + 1,
        "20 chars before the hit, 120 from it"
    );
}
#[test]
fn snippet_block_should_be_empty_for_empty_text() {
    assert_eq!(snippet_block("", "x"), "");
}

#[test]
fn snippet_block_should_center_on_the_hit_and_mark_elided_lines() {
    let lines: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
    let text = lines.join("\n");
    let block = snippet_block(&text, "line 70");
    // Window of 60 lines starting 30 before the hit.
    assert!(block.starts_with("…\nline 40\n"), "{}", &block[..30]);
    assert!(block.ends_with("line 99\n"), "the window reaches the end");
    let early = snippet_block(&text, "line 3");
    assert!(early.starts_with("line 0\n"));
    assert!(early.ends_with("line 59\n…"));
    let absent = snippet_block(&text, "nowhere");
    assert!(absent.starts_with("line 0\n"));
}

#[test]
fn snippet_block_should_stop_at_the_byte_cap() {
    let line = "y".repeat(1000);
    let text = [line.as_str(); 10].join("\n");
    let block = snippet_block(&text, "y");
    assert!(block.ends_with('…'));
    assert_eq!(
        block.matches(&line).count(),
        6,
        "six 1 KB lines fit in 6 KB"
    );
    assert!(block.len() <= SNIPPET_MAX_BYTES + '…'.len_utf8());
}
