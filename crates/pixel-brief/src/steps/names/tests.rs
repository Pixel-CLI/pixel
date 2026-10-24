// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

use std::time::{Duration, Instant};

use super::*;
use crate::steps::testing::{Repo, texts};
use crate::steps::{Steps, blocks};

fn ask(repo: &Repo, typed: &str, kind: QuestionKind, near: &[&str]) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let near: Vec<String> = near.iter().map(ToString::to_string).collect();
    texts(&blocks(typed, kind, Steps::default(), &near, repo, deadline).lead)
}

fn matcher() -> String {
    let mut text = String::from("//! The matcher.\n\n");
    for n in 0..6 {
        text.push_str(&format!("fn filler_{n}() {{}}\n"));
    }
    text.push_str("pub struct SmartCase {\n    sensitive: bool,\n}\n");
    text
}

#[test]
fn adjacent_words_spell_a_code_name_and_its_definition_is_shown() {
    let source = matcher();
    let repo = Repo::new(&[
        ("src/matcher.rs", &source),
        ("src/other.rs", "fn smart() {}\n"),
    ]);
    let shown = ask(
        &repo,
        "which part of the code handles smart case matching",
        QuestionKind::Lookup,
        &[],
    );
    assert_eq!(shown.len(), 1, "{shown:?}");
    // `pub struct SmartCase {` is line 9; three lines of context above it.
    assert!(
        shown[0].starts_with(
            "### src/matcher.rs:6-11 — SmartCase definition (named by the prompt: 'smart case')\n\
             6| fn filler_3() {}\n"
        ),
        "{}",
        shown[0]
    );
    assert!(
        shown[0].contains("9| pub struct SmartCase {\n10|     sensitive: bool,"),
        "{}",
        shown[0]
    );
}

#[test]
fn the_snake_case_spelling_finds_a_function_and_a_longer_name_wins_its_prefix() {
    let repo = Repo::new(&[(
        "src/a.py",
        "def smart_case(x):\n    return x\n\nclass SmartCaseMatcher:\n    pass\n",
    )]);
    let shown = ask(
        &repo,
        "which function uses smart case here",
        QuestionKind::Lookup,
        &[],
    );
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert!(
        shown[0].starts_with("### src/a.py:1-5 — SmartCase definition"),
        "{}",
        shown[0]
    );
    // `SmartCaseMatcher` is another word: the alternation's `\b` refuses it.
    assert!(!shown[0].contains("SmartCaseMatcher\n1|"));
}

#[test]
fn a_file_named_by_a_word_of_the_prompt_is_shown_whole_beside_the_briefs_files() {
    let repo = Repo::new(&[
        (
            "src/flags/config.rs",
            "pub struct Config;\n\nimpl Config {}\n",
        ),
        ("src/flags/mod.rs", "mod config;\n"),
        ("tests/config.rs", "fn t() {}\n"),
        ("other/config.py", "x = 1\n"),
    ]);
    let prompt = "which file holds the config part";
    assert_eq!(
        ask(&repo, prompt, QuestionKind::Config, &["src/flags/mod.rs"]),
        vec![
            "### src/flags/config.rs:1-3 — file named 'config' (a word of the prompt)\n\
             1| pub struct Config;\n\
             2| \n\
             3| impl Config {}"
        ]
    );
    // Away from what the brief shows, the file is no lead.
    assert!(ask(&repo, prompt, QuestionKind::Config, &["docs/x.md"]).is_empty());
    assert!(ask(&repo, prompt, QuestionKind::Config, &[]).is_empty());
}

#[test]
fn a_long_file_is_not_shown_whole() {
    let long: String = (0..221).map(|n| format!("let x{n} = {n};\n")).collect();
    let repo = Repo::new(&[("src/config.rs", &long), ("src/lib.rs", "")]);
    assert!(
        ask(
            &repo,
            "which file holds the config part",
            QuestionKind::Config,
            &["src/lib.rs"]
        )
        .is_empty()
    );
}

#[test]
fn a_question_about_modules_gets_the_module_and_the_lines_that_import_it() {
    let worker: String = (0..30).map(|n| format!("fn step_{n}() {{}}\n")).collect();
    let repo = Repo::new(&[
        ("src/search_worker.rs", &worker),
        (
            "src/main.rs",
            "use crate::search_worker::Worker;\nfn main() {}\n",
        ),
        ("src/notes.rs", "// search_worker is nice\n"),
        ("tests/it.rs", "use crate::search_worker::Worker;\n"),
        ("src/other.rs", "fn other() {}\n"),
    ]);
    let shown = ask(
        &repo,
        "which files depend on the search worker module",
        QuestionKind::Config,
        &[],
    );
    assert_eq!(shown.len(), 2, "{shown:?}");
    assert!(
        shown[0].starts_with(
            "### src/search_worker.rs:1-24 — module named by the prompt\n1| fn step_0() {}"
        ),
        "{}",
        shown[0]
    );
    assert!(shown[0].ends_with("24| fn step_23() {}"), "{}", shown[0]);
    assert_eq!(
        shown[1],
        "### src/main.rs:1-1 — source reference to search_worker\n1| use crate::search_worker::Worker;"
    );
}

#[test]
fn nothing_is_said_without_a_word_about_parts_or_for_the_kinds_that_skip_it() {
    let source = matcher();
    let repo = Repo::new(&[("src/matcher.rs", &source)]);
    assert!(
        ask(
            &repo,
            "how does smart case matching work",
            QuestionKind::Lookup,
            &[]
        )
        .is_empty()
    );
    for kind in [
        QuestionKind::Overview,
        QuestionKind::Review,
        QuestionKind::Flow,
        QuestionKind::History,
    ] {
        assert!(
            ask(&repo, "which part handles smart case matching", kind, &[]).is_empty(),
            "{kind:?}"
        );
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let off = Steps::default()
        .without(&["name-files".to_string()])
        .unwrap();
    assert!(
        blocks(
            "which part handles smart case matching",
            QuestionKind::Lookup,
            off,
            &[],
            &repo,
            deadline
        )
        .lead
        .is_empty()
    );
}

#[test]
fn the_step_may_answer_a_prompt_the_gate_refused() {
    let source = matcher();
    let repo = Repo::new(&[("src/matcher.rs", &source)]);
    let deadline = Instant::now() + Duration::from_secs(5);
    let out = blocks(
        "which part handles smart case matching",
        QuestionKind::Lookup,
        Steps::default(),
        &[],
        &repo,
        deadline,
    );
    assert!(out.rescuing);
    let none = blocks(
        "which part is unrelated",
        QuestionKind::Lookup,
        Steps::default(),
        &[],
        &repo,
        deadline,
    );
    assert!(!none.rescuing);
}

#[test]
fn identifiers_split_into_lowercase_subwords_with_plurals_folded() {
    assert_eq!(
        identifier_parts("HTTPServerError2go"),
        ["http", "server", "error"]
    );
    assert_eq!(identifier_parts("search_worker"), ["search", "worker"]);
    assert_eq!(
        identifier_parts("entries classes boxes status"),
        ["entry", "class", "boxe", "statu"]
    );
    assert_eq!(identifier_parts("getUserId"), ["get", "user"]);
    assert_eq!(identifier_parts("é_ünï"), Vec::<String>::new());
}

#[test]
fn compounds_pair_content_words_and_directories_are_compared_from_the_root() {
    let pairs = compounds("Where is the Smart Case flag defined");
    let spelled: Vec<(&str, &str, &str)> = pairs
        .iter()
        .map(|p| (p.camel.as_str(), p.snake.as_str(), p.phrase.as_str()))
        .collect();
    assert_eq!(
        spelled,
        [
            ("SmartCase", "smart_case", "smart case"),
            ("CaseFlag", "case_flag", "case flag")
        ]
    );
    assert_eq!(common_dirs("a/b/c.rs", "a/b/d/e.rs"), 2);
    assert_eq!(common_dirs("a/c.rs", "b/c.rs"), 0);
    assert_eq!(common_dirs("c.rs", "d.rs"), 0);
}
