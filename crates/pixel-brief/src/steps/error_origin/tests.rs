// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

use std::time::{Duration, Instant};

use super::*;
use crate::steps::testing::{Repo, texts};
use crate::steps::{Steps, blocks};

const CLI: &str = "pub fn run(args: &[String]) -> i32 {\n    if args.is_empty() {\n        eprintln!(\"error: nothing was searched\");\n        return 2;\n    }\n    0\n}\n";
const MAIN: &str = "fn main() {\n    let code = cli::run(&[]);\n    std::process::exit(code);\n}\n";
const CLI_TEST: &str = "fn runs() {\n    run(&[]);\n}\n";

fn repo() -> Repo {
    Repo::new(&[
        ("src/cli.rs", CLI),
        ("src/main.rs", MAIN),
        ("tests/cli.rs", CLI_TEST),
    ])
}

fn ask(repo: &Repo, typed: &str, kind: QuestionKind) -> Outcome {
    let deadline = Instant::now() + Duration::from_secs(5);
    blocks(typed, kind, Steps::default(), &[], repo, deadline)
}

fn heads(out: &Outcome) -> Vec<&str> {
    out.append.iter().map(Block::head).collect()
}

#[test]
fn a_quoted_message_gets_its_emitter_its_function_its_callers_and_a_test() {
    let out = ask(
        &repo(),
        "where does the error \"nothing was searched\" come from?",
        QuestionKind::Lookup,
    );
    assert!(!out.rescuing && out.lead.is_empty());
    assert_eq!(
        heads(&out),
        [
            "src/cli.rs:1-7 — error-emission candidate; matching terms and source condition, exact runtime cause unproven",
            "error origin — quoted occurrence candidate; emission unverified at src/cli.rs:3 near run (scope unresolved)",
            "src/cli.rs:1-7 — candidate source occurrence; inspect the evidence category above",
            "src/main.rs:1-4 — main → run: lexical call candidate; receiver/binding unresolved; scope unverified",
            "tests/cli.rs:1-3 — test runs: name match for run; binding unresolved",
        ]
    );
    assert_eq!(
        texts(&out.append)[3],
        "### src/main.rs:1-4 — main → run: lexical call candidate; receiver/binding unresolved; scope unverified\n\
         1| fn main() {\n\
         2|     let code = cli::run(&[]);\n\
         3|     std::process::exit(code);\n\
         4| }"
    );
}

#[test]
fn a_described_message_is_the_string_that_carries_most_of_the_prompts_words() {
    let repo = Repo::new(&[
        ("src/cli.rs", CLI),
        (
            "src/other.rs",
            "pub fn other() {\n    log(\"nothing at all\");\n    log(\"elsewhere\");\n}\n",
        ),
    ]);
    // The matching emitter outranks the same file's lexical neighbour: one block.
    let out = ask(
        &repo,
        "which message says that nothing was searched",
        QuestionKind::Lookup,
    );
    assert_eq!(
        heads(&out),
        [
            "src/cli.rs:1-7 — error-emission candidate; matching terms and source condition, exact runtime cause unproven"
        ]
    );
    // No emitter shape: the literal itself is the origin.
    let plain = Repo::new(&[(
        "src/text.rs",
        "pub fn banner() -> &'static str {\n    \"the cache was cleared successfully\"\n}\n\npub fn other() -> &'static str {\n    \"cleared\"\n}\n",
    )]);
    let out = ask(
        &plain,
        "which message says the cache was cleared successfully",
        QuestionKind::Lookup,
    );
    assert_eq!(
        heads(&out),
        [
            "error origin — described occurrence candidate; emission unverified at src/text.rs:2 near banner (scope unresolved)",
            "src/text.rs:1-7 — candidate source occurrence; inspect the evidence category above",
        ]
    );
}

#[test]
fn a_literal_in_a_json_catalogue_is_followed_to_the_code_that_names_its_key() {
    let repo = Repo::new(&[
        (
            "locales/en.json",
            "{\n  \"no_input\": \"nothing was searched for\",\n  \"other\": \"fine\"\n}\n",
        ),
        (
            "src/report.rs",
            "pub fn report() {\n    say(\"no_input\");\n}\n",
        ),
    ]);
    let out = ask(
        &repo,
        "which message says nothing was searched for",
        QuestionKind::Lookup,
    );
    assert_eq!(
        heads(&out),
        [
            "error origin — described occurrence candidate; emission unverified at src/report.rs:2 near report (scope unresolved)",
            "locales/en.json:2-2 — message definition in catalogue; emission unverified",
            "src/report.rs:1-3 — candidate source occurrence; inspect the evidence category above",
        ]
    );
}

#[test]
fn only_a_question_about_an_error_runs_and_the_flag_turns_it_off() {
    let repo = repo();
    for (prompt, kind) in [
        ("where is nothing searched handled", QuestionKind::Lookup),
        (
            "where does the error \"nothing was searched\" come from?",
            QuestionKind::History,
        ),
        (
            "where does the error \"nothing was searched\" come from?",
            QuestionKind::Review,
        ),
    ] {
        assert!(ask(&repo, prompt, kind).append.is_empty(), "{prompt}");
    }
    let off = Steps::default()
        .without(&["error-origin".to_string()])
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    assert!(
        blocks(
            "where does the error \"nothing was searched\" come from?",
            QuestionKind::Lookup,
            off,
            &[],
            &repo,
            deadline
        )
        .append
        .is_empty()
    );
}

#[test]
fn a_message_no_source_line_holds_leaves_the_pack_alone() {
    let out = ask(
        &repo(),
        "where does the error \"something else entirely\" come from?",
        QuestionKind::Lookup,
    );
    assert!(out.append.is_empty());
}

#[test]
fn a_python_line_is_labelled_by_its_shape() {
    let lines: Vec<String> = [
        "MESSAGE = \"nothing was searched\"",
        "def f():",
        "    raise ValueError(\"nothing was searched\")",
        "    \"\"\"nothing was searched",
        "    here\"\"\"",
        "    return g(\"nothing was searched\")",
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    let roles: Vec<&str> = (0..lines.len())
        .map(|index| python_role(&lines, index, "a.py"))
        .collect();
    assert_eq!(
        roles,
        [
            "message definition; emission unverified",
            "occurrence candidate; emission unverified",
            "local raise syntax; message origin candidate",
            "documentation occurrence; no emission proved",
            "documentation occurrence; no emission proved",
            "occurrence candidate; emission unverified",
        ]
    );
}

#[test]
fn words_split_at_case_changes_and_stem_one_inflection() {
    assert_eq!(
        tokens("HTTPServerError_code2 fooBar"),
        ["http", "server", "error", "code", "2", "foo", "bar"]
    );
    assert_eq!(stem("policies"), "policy");
    assert_eq!(stem("searched"), "search");
    assert_eq!(stem("is"), "is");
    assert_eq!(
        prompt_words("Which messages say the cache was cleared?"),
        ["message", "say", "cache", "clear"]
    );
    assert!(carried("no", &BTreeSet::from(["no".to_string()])));
    assert!(!carried("no", &BTreeSet::from(["not".to_string()])));
    assert!(carried("clear", &BTreeSet::from(["cleared".to_string()])));
}

#[test]
fn the_strings_of_a_line_and_the_strings_that_span_lines_are_found() {
    assert_eq!(
        string_part(r#"log("a \" b", 'c') + `d` + "open"#),
        "a \\\" b c d"
    );
    let lines: Vec<String> = [
        "let a = 1;",
        "let m = \"first",
        "second",
        "third\";",
        "let b = 2;",
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    assert_eq!(
        string_span(&lines, 2, "a.rs"),
        Some((2, 2)).and(Some((1, 3)))
    );
    assert_eq!(string_span(&lines, 0, "a.rs"), Some((0, 0)));
    assert_eq!(open_quotes("a \"b\" c \"d", "a.rs"), 3);
    assert_eq!(open_quotes("it's \\\"x\\\" '\"'", "a.rs"), 0);
    assert_eq!(open_quotes("\"\"\"doc", "a.py"), 1);
    assert_eq!(open_quotes("a ` b", "a.ts"), 1);
}
