// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

use std::path::Path;

use super::super::testing::{commit_line, git, repo};
use super::*;

fn numbers(text: &str) -> Vec<&str> {
    version_spans(text)
        .into_iter()
        .map(|(_, _, number)| number)
        .collect()
}

#[test]
fn a_version_is_dotted_digits_not_glued_to_a_word_or_a_dot() {
    assert_eq!(numbers("v5.3.0 and 3.10"), ["5.3.0", "3.10"]);
    assert_eq!(numbers("see 2.0. Then -1.5, (4.2)"), ["2.0", "1.5", "4.2"]);
    assert_eq!(numbers("1.2.x"), ["1.2"]);
    assert_eq!(numbers("v1.2"), ["1.2"]);
    for glued in ["x3.10", "1.2.3a", "1.2_3", "a.1.2", "3", "v3", "3.", "10.a"] {
        assert_eq!(numbers(glued), Vec::<&str>::new(), "{glued}");
    }
}

#[test]
fn the_spans_cover_the_optional_v_and_replace_by_a_space() {
    assert_eq!(version_spans("in v5.3.0 now"), [(3, 9, "5.3.0")]);
    assert_eq!(without_versions("in v5.3.0 now, 3.10"), "in   now,  ");
    assert_eq!(without_versions("no version"), "no version");
}

#[test]
fn versions_are_listed_once_in_the_order_quoted() {
    assert_eq!(versions("3.10 then v2.0 then 3.10"), ["3.10", "2.0"]);
}

#[test]
fn stems_are_four_letters_of_the_words_a_history_question_does_not_use() {
    let held = stems("Which commits changed the Redirect handling? Parsers");
    let mut held: Vec<_> = held.into_iter().collect();
    held.sort();
    assert_eq!(held, ["hand", "pars", "redi"]);
}

#[test]
fn subject_words_keep_letters_and_digits_and_drop_what_says_nothing() {
    assert_eq!(
        subject_words("Which commit added the v2 CLI parser to a build?"),
        ["v2", "cli", "parser", "build"]
    );
    assert_eq!(subject_words("x I"), Vec::<String>::new());
}

#[test]
fn quoted_titles_are_between_quotes_of_eight_to_one_hundred_twenty_characters() {
    assert_eq!(
        quoted("what did \"fix the archive reader\" and `bump 1.2.3 now` do, or 'short'"),
        ["fix the archive reader", "bump 1.2.3 now"]
    );
    let long = "x".repeat(121);
    assert_eq!(quoted(&format!("\"{long}\"")), Vec::<String>::new());
    assert_eq!(quoted("\"  padded title  \""), ["padded title"]);
}

#[test]
fn a_line_has_its_id_first_and_its_subject_after_the_date() {
    assert_eq!(id_of("abc1234 2026-01-02 fix: it"), Some("abc1234"));
    assert_eq!(id_of("  "), None);
    assert_eq!(subject_of("abc1234 2026-01-02 fix: it now"), "fix: it now");
    assert_eq!(subject_of("abc1234 2026-01-02"), "2026-01-02");
    assert_eq!(subject_of(""), "");
}

#[test]
fn a_grep_argument_is_made_for_each_of_the_first_words() {
    let words: Vec<String> = ["alpha", "beta", "gamma"].map(ToOwned::to_owned).to_vec();
    assert_eq!(greps(&words, 2), ["--grep=alpha", "--grep=beta"]);
    assert_eq!(greps(&words, 5).len(), 3);
}

fn ctx_for<R>(root: &Path, prompt: &str, run: impl FnOnce(&Ctx) -> R) -> R {
    let deadline = Instant::now() + Duration::from_secs(15);
    run(&Ctx::new(root, prompt, "", deadline))
}

/// A repository whose history is `subjects`, newest first, as empty commits.
fn history(subjects: &[&str]) -> tempfile::TempDir {
    let dir = repo(&[]);
    for subject in subjects.iter().rev() {
        commit_empty(dir.path(), &["-m", subject]);
    }
    dir
}

fn commit_empty(root: &Path, message: &[&str]) {
    let mut args = vec![
        "-c",
        "user.name=Probe Test",
        "-c",
        "user.email=probe@example.invalid",
        "commit",
        "-q",
        "--allow-empty",
    ];
    args.extend_from_slice(message);
    git(root, &args);
}

/// The subjects `by_subject` ranks for `prompt`, best first.
fn ranked_subjects(root: &Path, prompt: &str) -> Vec<String> {
    ctx_for(root, prompt, |ctx| by_subject(ctx, prompt))
        .into_iter()
        .map(|(_, line)| subject_of(&line).to_owned())
        .collect()
}

#[test]
fn subjects_rank_by_rarity_and_adjacency_of_the_shared_words() {
    // Each term of the score moves this order: the rare words of the second
    // line outweigh the common ones of the third, the adjacent pair of the
    // first lifts a two-word subject over both, and ties go to the newest.
    let dir = history(&[
        "delta bravo alpha foxtrot",
        "alpha other",
        "alpha other",
        "alpha bravo",
        "foxtrot other",
        "charlie echo bravo delta",
    ]);
    assert_eq!(
        ranked_subjects(dir.path(), "alpha bravo charlie delta echo foxtrot"),
        [
            "alpha bravo",
            "charlie echo bravo delta",
            "delta bravo alpha foxtrot"
        ]
    );
}

#[test]
fn three_shared_words_admit_a_subject_and_two_need_an_adjacent_pair() {
    let dir = history(&["charlie bravo alpha", "bravo alpha other"]);
    assert_eq!(
        ranked_subjects(dir.path(), "alpha bravo charlie"),
        ["charlie bravo alpha"]
    );
}

#[test]
fn a_pair_of_which_one_word_says_nothing_is_not_adjacent() {
    let dir = history(&["parser the lexer"]);
    assert_eq!(
        ranked_subjects(dir.path(), "parser the lexer"),
        Vec::<String>::new()
    );
}

#[test]
fn a_history_of_more_than_a_mebibyte_of_subjects_is_still_ranked() {
    let dir = history(&["charlie bravo alpha"]);
    let message = dir.path().join(".git/long-subject");
    std::fs::write(&message, "z".repeat(1_200_000)).unwrap();
    commit_empty(dir.path(), &["-F", message.to_str().unwrap()]);
    assert_eq!(
        ranked_subjects(dir.path(), "alpha bravo charlie"),
        ["charlie bravo alpha"]
    );
}

/// `(commit, subject)` of what `by_version` finds for `prompt`.
fn named_by_version(root: &Path, prompt: &str) -> Vec<(String, String)> {
    ctx_for(root, prompt, |ctx| by_version(ctx, prompt))
        .into_iter()
        .map(|(sha, line)| (sha, subject_of(&line).to_owned()))
        .collect()
}

#[test]
fn the_commit_a_release_tag_points_at_is_named_without_the_version_in_its_message() {
    let dir = repo(&[]);
    let tagged = commit_line(dir.path(), "cut the cord", "");
    git(dir.path(), &["tag", "v4.2"]);
    assert_eq!(
        named_by_version(dir.path(), "which commit shipped 4.2"),
        [(tagged, "cut the cord".to_owned())]
    );
}

#[test]
fn a_tagged_commit_that_names_the_version_is_listed_once() {
    let dir = repo(&[]);
    let tagged = commit_line(dir.path(), "release 5.0.0 notes", "");
    git(dir.path(), &["tag", "v5.0.0"]);
    assert_eq!(
        named_by_version(dir.path(), "what changed in 5.0.0"),
        [(tagged, "release 5.0.0 notes".to_owned())]
    );
}

#[test]
fn the_tagged_commit_comes_before_a_newer_one_that_shares_more_words() {
    let dir = repo(&[]);
    let tagged = commit_line(dir.path(), "cut the cord", "");
    git(dir.path(), &["tag", "v6.1"]);
    let newer = commit_line(dir.path(), "tune the parser for 6.1", "");
    assert_eq!(
        named_by_version(dir.path(), "what changed in the parser for 6.1"),
        [
            (tagged, "cut the cord".to_owned()),
            (newer, "tune the parser for 6.1".to_owned())
        ]
    );
}

#[test]
fn an_untagged_commit_that_shares_no_word_with_the_prompt_is_not_named() {
    let dir = repo(&[]);
    commit_line(dir.path(), "update to 3.1", "");
    assert_eq!(
        named_by_version(dir.path(), "which commit tuned the parser for 3.1"),
        Vec::<(String, String)>::new()
    );
}
