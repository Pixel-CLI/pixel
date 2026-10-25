// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The cases of `probes/history-fallback/tests/test_subjects.py`, run on the
//! step, and one case per branch of the lookup.

use std::path::Path;

use super::super::testing::{applied, commit_line as commit, commit_staged, git, outcomes_of, repo, run};
use super::*;

fn ask(root: &Path, prompt: &str) -> String {
    run(&STEP, root, prompt, "")
}

/// The commit a `### commit <id> — …` header names.
fn commit_of(out: &str) -> &str {
    out.lines()
        .find_map(|line| line.strip_prefix("### commit "))
        .and_then(|rest| rest.split(' ').next())
        .unwrap_or_else(|| panic!("no commit block in {out:?}"))
}

fn assert_commit(root: &Path, prompt: &str, expected: &str) {
    let out = ask(root, prompt);
    assert!(expected.starts_with(commit_of(&out)), "{out}");
    assert!(out.contains("+feature = True"), "{out}");
    assert_eq!(out, ask(root, prompt), "deterministic");
}

#[test]
fn subject_evidence_beats_incidental_body_and_keeps_short_words() {
    let dir = repo(&[]);
    let expected = commit(dir.path(), "feat: compact data pack", "");
    commit(
        dir.path(),
        "docs: refresh output",
        "introduced compact data in a later manual",
    );
    assert_commit(
        dir.path(),
        "which commit introduced the compact data pack and what did it add",
        &expected,
    );
}

#[test]
fn a_two_word_subject_phrase_is_enough() {
    let dir = repo(&[]);
    let expected = commit(dir.path(), "feat: delta compression", "");
    commit(
        dir.path(),
        "docs: refresh manual",
        "introduced delta behavior and compression examples",
    );
    assert_commit(dir.path(), "which commit introduced delta compression", &expected);
}

#[test]
fn a_lone_shared_subject_word_names_no_commit() {
    let dir = repo(&[]);
    commit(dir.path(), "feat: zip engine", "");
    assert_eq!(ask(dir.path(), "which commit introduced zip compression"), "");
}

#[test]
fn an_exact_quoted_title_keeps_priority() {
    let dir = repo(&[]);
    let expected = commit(dir.path(), "fix the archive reader", "");
    commit(dir.path(), "feat: compact data pack", "");
    assert_commit(
        dir.path(),
        "what did \"fix the archive reader\" change for the compact data pack",
        &expected,
    );
    let out = ask(
        dir.path(),
        "what did \"fix the archive reader\" change for the compact data pack",
    );
    assert!(
        out.starts_with("### history — commits matching fix, archive, reader\n"),
        "{out}"
    );
}

#[test]
fn an_exact_release_tag_keeps_priority_and_the_first_tag_is_named() {
    let dir = repo(&[]);
    let expected = commit(dir.path(), "release 7.2.0", "");
    git(dir.path(), &["tag", "v7.2.0"]);
    commit(dir.path(), "feat: compact data pack", "");
    let out = ask(dir.path(), "what changed in v7.2.0 for the compact data pack");
    assert!(expected.starts_with(commit_of(&out)), "{out}");
    assert!(out.starts_with("### history — commits matching 7.2.0\n"), "{out}");
    assert!(out.ends_with("first tag containing it: v7.2.0\n"), "{out}");
}

#[test]
fn a_version_names_the_commit_that_mentions_it_whole_and_shares_a_word() {
    let dir = repo(&[]);
    let expected = commit(dir.path(), "bump parser to 3.10", "");
    commit(dir.path(), "bump parser to 3.1", "");
    commit(dir.path(), "bump lexer to 3.10.1", "");
    let out = ask(dir.path(), "which commit bumped the parser to 3.10?");
    assert!(expected.starts_with(commit_of(&out)), "{out}");
    let listed: Vec<&str> = out.lines().skip(1).take_while(|l| !l.starts_with("###")).collect();
    assert_eq!(listed.len(), 1, "{out}");
}

#[test]
fn a_version_no_commit_names_falls_through_to_the_other_lookups() {
    let dir = repo(&[]);
    let expected = commit(dir.path(), "feat: delta compression", "");
    assert_commit(dir.path(), "in 9.9 which commit introduced delta compression", &expected);
}

#[test]
fn the_prompts_rarest_words_find_a_commit_when_no_subject_does() {
    let dir = repo(&[]);
    commit(dir.path(), "chore: tidy", "the wombat frobnicates");
    let expected = commit(dir.path(), "chore: update", "a frobnicate pass over wombat code");
    commit(dir.path(), "chore: other", "wombat only");
    let out = ask(dir.path(), "who touched wombat and frobnicate code");
    assert!(expected.starts_with(commit_of(&out)), "{out}");
    assert!(
        out.starts_with("### history — commits matching frobnicate, wombat\n"),
        "{out}"
    );
}

#[test]
fn a_single_rare_word_is_enough_for_this_fallback() {
    let dir = repo(&[]);
    let expected = commit(dir.path(), "chore: update", "reworded the grommet");
    commit(dir.path(), "chore: other", "nothing");
    let out = ask(dir.path(), "when was the grommet changed");
    assert!(expected.starts_with(commit_of(&out)), "{out}");
}

#[test]
fn a_question_naming_nothing_the_history_holds_gets_nothing() {
    let dir = repo(&[]);
    commit(dir.path(), "feat: delta compression", "");
    assert_eq!(ask(dir.path(), "which commit made the zebra stripe"), "");
    assert_eq!(ask(dir.path(), "which commit"), "");
}

#[test]
fn a_repository_without_a_commit_gets_nothing() {
    let dir = repo(&[("a.txt", "x")]);
    assert_eq!(ask(dir.path(), "which commit introduced delta compression"), "");
}

#[test]
fn a_brief_that_holds_a_history_block_gets_nothing() {
    let dir = repo(&[]);
    commit(dir.path(), "feat: delta compression", "");
    let prompt = "which commit introduced delta compression";
    let brief = "[PIXEL:BRIEF]\n### history — commits touching it\nabc 2026-01-01 x\n";
    assert_eq!(run(&STEP, dir.path(), prompt, brief), "");
    let other = "[PIXEL:BRIEF]\n### a history of it\nabc\n";
    assert_ne!(run(&STEP, dir.path(), prompt, other), "");
}

#[test]
fn a_merge_shows_what_it_brought_to_its_first_parent() {
    let dir = repo(&[]);
    let root = dir.path();
    commit(root, "feat: base work", "");
    git(root, &["checkout", "-q", "-b", "side"]);
    std::fs::write(root.join("sprocket.txt"), "teeth\n").unwrap();
    git(root, &["add", "sprocket.txt"]);
    commit_staged(root, "side: sprocket", "");
    git(root, &["checkout", "-q", "-"]);
    git(
        root,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@t",
            "merge",
            "--no-ff",
            "-q",
            "-m",
            "land the sprocket work",
            "side",
        ],
    );
    let out = ask(root, "what did \"land the sprocket work\" change");
    assert!(out.contains("sprocket.txt"), "{out}");
    assert!(out.contains("+teeth"), "{out}");
}

#[test]
fn the_message_is_cut_at_ten_lines_and_every_line_at_160_characters() {
    let dir = repo(&[]);
    let body: Vec<String> = (1..=30).map(|n| format!("line{n}")).collect();
    let expected = commit(dir.path(), "feat: delta compression", &body.join("\n"));
    let out = ask(dir.path(), "which commit introduced delta compression");
    assert!(expected.starts_with(commit_of(&out)), "{out}");
    assert!(out.contains("\nline7\n"), "{out}");
    assert!(!out.contains("\nline8\n"), "{out}");
    let dir = repo(&[]);
    commit(dir.path(), "feat: delta compression", &"w".repeat(300));
    let out = ask(dir.path(), "which commit introduced delta compression");
    assert!(out.contains(&format!("\n{}\n", "w".repeat(160))), "{out}");
    assert!(!out.contains(&"w".repeat(161)), "{out}");
}

#[test]
fn the_patch_stops_at_ninety_lines_of_the_commit_block() {
    let dir = repo(&[]);
    let root = dir.path();
    std::fs::write(
        root.join("source.py"),
        (0..200).map(|n| format!("line {n}\n")).collect::<String>(),
    )
    .unwrap();
    git(root, &["add", "source.py"]);
    commit_staged(root, "feat: delta compression", "");
    let out = ask(root, "which commit introduced delta compression");
    let block = out.split("### commit ").nth(1).unwrap();
    let lines = block.lines().count() - 1;
    assert_eq!(lines, 90, "{out}");
}

#[test]
fn the_step_applies_to_history_questions_and_to_commit_words_alone() {
    assert!(STEP.applies(Some("history"), "anything", true));
    assert!(STEP.applies(None, "which commits touched it", true));
    assert!(STEP.applies(None, "git history of it", false));
    assert!(!STEP.applies(Some("flow"), "where is it defined", false));
}

#[test]
fn the_registered_step_runs_and_the_installed_script_of_its_name_does_not() {
    let dir = repo(&[]);
    commit(dir.path(), "feat: delta compression", "");
    let prompt = "which commit introduced delta compression";
    // A review question: `cand-k-commit-patch` skips it, this step answers by its words.
    let (merged, runs) = applied(STEP.name, dir.path(), "review", prompt, "");
    assert_eq!(outcomes_of(&runs, "history-fallback"), ["ok"]);
    assert_eq!(outcomes_of(&runs, "cand-k-commit-patch"), Vec::<&str>::new());
    assert_eq!(
        merged.unwrap(),
        format!("[PIXEL:BRIEF]\n{}", ask(dir.path(), prompt))
    );
}
