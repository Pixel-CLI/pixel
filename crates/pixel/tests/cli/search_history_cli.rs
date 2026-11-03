// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel search-history` is built into the core binary: it searches the
//! commit messages, paths and diffs of a real git repository through the
//! facts index, and no longer answers with the "moved to a plugin" hint.

use serde_json::Value;

use crate::support::{Scratch, git, pixel_command};

/// A repository whose three commits each hold a term only one facet can see:
/// the message says "retry", the path is `billing.rs`, the diff adds
/// `backoff_ceiling`.
fn fixture(tag: &str) -> Scratch {
    let repo = Scratch::for_test("pixel-search-history-cli", tag);
    std::fs::write(repo.join(".gitignore"), ".pixel/\n").unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("readme.txt"), "hello\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial import"]);
    std::fs::write(repo.join("billing.rs"), "pub fn bill() {}\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "introduce the retry loop"]);
    std::fs::write(
        repo.join("readme.txt"),
        "hello\nconst backoff_ceiling: u32 = 30;\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "tune limits"]);
    repo
}

fn pixel(repo: &Scratch, args: &[&str]) -> std::process::Output {
    pixel_command()
        .current_dir(&**repo)
        .args(args)
        .arg("--metrics=off")
        .output()
        .unwrap()
}

fn head_of(repo: &Scratch, revision: &str) -> String {
    let out = std::process::Command::new("git")
        .current_dir(&**repo)
        .args(["rev-parse", revision])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// Whether `hit` names `revision` (the index stores a 12-digit abbreviation).
fn names(repo: &Scratch, hit: &Value, revision: &str) -> bool {
    let oid = hit["oid"].as_str().unwrap();
    oid.len() >= 12 && head_of(repo, revision).starts_with(oid)
}

fn candidates(repo: &Scratch, args: &[&str]) -> Vec<Value> {
    let mut all = vec!["search-history"];
    all.extend_from_slice(args);
    all.push("--json");
    let out = pixel(repo, &all);
    assert!(out.status.success(), "{out:?}");
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    value["candidates"].as_array().unwrap().clone()
}

#[test]
fn a_message_term_should_find_the_commit_that_says_it() {
    let repo = fixture("message");
    let hits = candidates(&repo, &["retry", "--facet", "message"]);
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert!(names(&repo, &hits[0], "HEAD~1"), "{hits:?}");
    assert_eq!(hits[0]["subject"], "introduce the retry loop");
    assert_eq!(hits[0]["kind"], "message");
}

#[test]
fn each_facet_should_see_only_what_it_indexes() {
    let repo = fixture("facets");
    let by_path = candidates(&repo, &["billing", "--facet", "path"]);
    assert_eq!(by_path.len(), 1, "{by_path:?}");
    assert!(names(&repo, &by_path[0], "HEAD~1"), "{by_path:?}");
    assert_eq!(by_path[0]["path"], "billing.rs");

    let by_diff = candidates(&repo, &["backoff_ceiling", "--facet", "diff"]);
    assert!(!by_diff.is_empty(), "the diff term is in the last commit");
    assert!(
        by_diff.iter().all(|hit| names(&repo, hit, "HEAD")),
        "{by_diff:?}"
    );
    assert!(
        candidates(&repo, &["backoff_ceiling", "--facet", "message"]).is_empty(),
        "no message says it"
    );
}

#[test]
fn the_text_should_print_the_short_oid_the_subject_and_the_kind() {
    let repo = fixture("text");
    let out = pixel(&repo, &["search-history", "retry", "--facet", "message"]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    let short: String = head_of(&repo, "HEAD~1").chars().take(10).collect();
    assert!(
        text.contains(&format!("{short} ")) && text.contains("introduce the retry loop [message]"),
        "{text}"
    );

    let none = pixel(&repo, &["search-history", "nonexistentterm"]);
    assert!(none.status.success(), "{none:?}");
    assert_eq!(
        String::from_utf8(none.stdout).unwrap(),
        "no commits match\n"
    );
}

#[test]
fn read_only_should_answer_from_nothing_without_building_the_history() {
    let repo = fixture("read-only");
    let out = pixel(&repo, &["search-history", "retry", "--read-only"]);
    assert_eq!(out.status.code(), Some(1), "no facts db yet: {out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "pixel: facts index absent or not built"
    );
    assert!(
        !repo.join(".pixel/history.db").exists(),
        "a read-only query never creates the history db"
    );
}

#[test]
fn the_command_should_be_the_core_s_and_not_a_moved_one() {
    let repo = fixture("not-moved");
    let out = pixel(&repo, &["search-history", "retry"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("moved to a plugin"), "{stderr}");
    assert!(crate::support::built_in_commands().contains("search-history"));
}
