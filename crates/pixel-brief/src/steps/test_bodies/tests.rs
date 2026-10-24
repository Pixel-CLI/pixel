// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

use std::time::{Duration, Instant};

use super::*;
use crate::steps::testing::{Repo, texts};
use crate::steps::{Steps, blocks};

fn ask(repo: &Repo, typed: &str, near: &[&str]) -> Vec<String> {
    ask_as(repo, typed, QuestionKind::Tests, near)
}

fn ask_as(repo: &Repo, typed: &str, kind: QuestionKind, near: &[&str]) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let near: Vec<String> = near.iter().map(ToString::to_string).collect();
    texts(&blocks(typed, kind, Steps::default(), &near, repo, deadline).append)
}

fn heads(blocks: &[String]) -> Vec<&str> {
    blocks
        .iter()
        .map(|block| block.lines().next().unwrap())
        .collect()
}

const PY_TESTS: &str = "import pytest\n\n\ndef test_follows_a_redirect_and_handles_the_new_location():\n    response = get(\"/old\", follow=True)\n    assert response.redirect_target == \"/new\"\n\n\ndef test_parses_the_header_case_insensitively():\n    assert parse(\"X-A: 1\") == {\"x-a\": \"1\"}\n";

#[test]
fn subject_matching_uses_words_and_inflections_not_arbitrary_prefixes() {
    let toks = tokens("redirects handles balanced");
    assert!(carries(&toks, "redirect"));
    assert!(carries(&toks, "handl"));
    assert!(carries(&toks, "balance"));
    let toks = tokens("candidate production");
    assert!(!carries(&toks, "can"));
    assert!(!carries(&toks, "product"));
}

#[test]
fn the_case_naming_the_subject_is_quoted_whole_and_the_unrelated_case_is_not() {
    let repo = Repo::new(&[
        ("tests/test_client.py", PY_TESTS),
        ("src/client.py", "def get(path, follow=False):\n    pass\n"),
    ]);
    let shown = ask(
        &repo,
        "where are the tests for redirect handling and what do they cover",
        &[],
    );
    assert_eq!(
        shown,
        vec![
            "### tests/test_client.py:4-6 — test test_follows_a_redirect_and_handles_the_new_location\n\
             4| def test_follows_a_redirect_and_handles_the_new_location():\n\
             5|     response = get(\"/old\", follow=True)\n\
             6|     assert response.redirect_target == \"/new\""
        ]
    );
}

#[test]
fn a_rust_case_runs_to_its_closing_brace_inside_an_inline_test_module() {
    let source = "pub fn gate(score: u32) -> bool {\n    score > 3\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn the_gate_opens_above_three() {\n        assert!(gate(4));\n        assert!(!gate(3));\n    }\n}\n";
    let repo = Repo::new(&[("src/gate.rs", source)]);
    let shown = ask(&repo, "which tests cover the gate", &["src/gate.rs"]);
    assert_eq!(
        heads(&shown),
        ["### src/gate.rs:9-13 — test the_gate_opens_above_three"]
    );
    assert!(shown[0].ends_with("13|     }"), "{}", shown[0]);
    // Named by the brief or by its path, a source without a `#[test]` is no
    // test file.
    let plain = Repo::new(&[("src/gate.rs", "pub fn gate() {}\n")]);
    assert!(ask(&plain, "which tests cover the gate", &["src/gate.rs"]).is_empty());
}

#[test]
fn a_word_every_case_carries_does_not_bring_them_all_in() {
    let mut cases: String = (0..4)
        .map(|n| format!("def test_check_{n}():\n    check({n})\n\n\n"))
        .collect();
    cases.push_str("def test_check_parser():\n    check(parser())\n");
    let repo = Repo::new(&[("tests/test_a.py", &cases)]);
    assert_eq!(
        heads(&ask(&repo, "which tests check the parser", &[])),
        ["### tests/test_a.py:17-18 — test test_check_parser"]
    );
}

#[test]
fn an_exact_option_name_does_not_select_a_sibling_option_case() {
    let repo = Repo::new(&[(
        "tests/options.rs",
        "\n#[test]\nfn test_exec() {\n    run(&[\"--exec\", \"echo\", \"{}\"]).assert_stdout(\"one.txt\");\n    run(&[\"--exec\", \"echo\", \"{/.}\"]).assert_stdout(\"one\");\n}\n\n#[test]\nfn test_exec_batch() {\n    run(&[\"--exec-batch\", \"echo\", \"{}\"]).assert_stdout(\"one.txt\");\n}\n",
    )]);
    let exact = ask(
        &repo,
        "where are the integration tests for the exec option and what behaviours do they cover",
        &[],
    );
    assert_eq!(heads(&exact), ["### tests/options.rs:2-6 — test test_exec"]);
    assert!(exact[0].contains("--exec\", \"echo\""));
    assert!(!exact[0].contains("test_exec_batch"));

    let batch = ask(
        &repo,
        "where are the integration tests for the --exec-batch option",
        &[],
    );
    assert_eq!(
        heads(&batch),
        ["### tests/options.rs:8-11 — test test_exec_batch"]
    );
    assert!(batch[0].contains("--exec-batch"));
    assert!(!batch[0].contains("test_exec()"));
}

#[test]
fn the_exact_option_selector_generalizes_to_other_option_names() {
    let repo = Repo::new(&[(
        "tests/options.rs",
        "\n#[test]\nfn test_search_depth() {\n    run(&[\"--search-depth\", \"4\"]).assert_depth(4);\n}\n\n#[test]\nfn test_search_depth_default() {\n    run(&[\"--search-depth\"]).assert_depth(8);\n}\n",
    )]);
    assert_eq!(
        heads(&ask(
            &repo,
            "where are the integration tests for the --search-depth option",
            &[]
        )),
        ["### tests/options.rs:2-5 — test test_search_depth"]
    );
}

#[test]
fn an_exact_option_requires_title_or_exact_flag_evidence() {
    let batch_only = Repo::new(&[(
        "tests/options.rs",
        "\n#[test]\nfn test_exec_batch() {\n    run(&[\"--exec-batch\", \"echo\", \"{}\"]).assert_stdout(\"one.txt\");\n}\n",
    )]);
    assert!(ask(&batch_only, "where are the tests for the exec option", &[]).is_empty());

    let flag_in_body = Repo::new(&[(
        "tests/options.rs",
        "\n#[test]\nfn test_uses_command_override() {\n    run(&[\"--exec\", \"echo\", \"{}\"]).assert_stdout(\"one.txt\");\n}\n",
    )]);
    assert_eq!(
        heads(&ask(
            &flag_in_body,
            "where are the tests for the exec option",
            &[]
        )),
        ["### tests/options.rs:2-5 — test test_uses_command_override"]
    );
}

#[test]
fn a_js_case_ends_at_its_closing_line_and_the_spec_file_is_found_by_its_path() {
    let spec = "describe('undo', () => {\n  it('restores the deleted shape on undo', () => {\n    del(shape);\n    undo();\n    expect(shapes()).toContain(shape);\n    // The observable behaviours stay stable.\n  });\n\n  it('keeps the colour', () => {\n    expect(colour()).toBe('red');\n  });\n});\n";
    let repo = Repo::new(&[("src/history.test.ts", spec)]);
    assert_eq!(
        heads(&ask(
            &repo,
            "where are the tests for undo and what behaviours do they cover",
            &[]
        )),
        ["### src/history.test.ts:2-7 — test restores the deleted shape on undo"]
    );
}

#[test]
fn a_loose_test_word_neither_admits_nor_emits_unrelated_cases() {
    let repo = Repo::new(&[(
        "tests/test_candidate_policy.py",
        "def test_candidate_policy_stays_explicit():\n    assert balance_is_stable()\n\ndef test_general_case():\n    assert can_run()\n",
    )]);
    for prompt in [
        "How do I test whether a dragon can balance a teacup?",
        "Can you test whether our release mascot can drink tea?",
        "Please test a concept for our product workshop.",
    ] {
        assert!(ask(&repo, prompt, &[]).is_empty(), "{prompt}");
        assert!(
            ask_as(&repo, prompt, QuestionKind::Lookup, &[]).is_empty(),
            "{prompt}"
        );
    }
}

#[test]
fn only_a_tests_question_runs_and_the_flag_switches_it_off() {
    let repo = Repo::new(&[("tests/test_client.py", PY_TESTS)]);
    let prompt = "where are the tests for redirect handling";
    for kind in [
        QuestionKind::Lookup,
        QuestionKind::Flow,
        QuestionKind::History,
        QuestionKind::Review,
    ] {
        assert!(ask_as(&repo, prompt, kind, &[]).is_empty(), "{kind:?}");
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let off = Steps::default()
        .without(&["test-bodies".to_string()])
        .unwrap();
    assert!(
        blocks(prompt, QuestionKind::Tests, off, &[], &repo, deadline)
            .append
            .is_empty()
    );
    let on = blocks(
        prompt,
        QuestionKind::Tests,
        Steps::default(),
        &[],
        &repo,
        deadline,
    );
    assert!(on.rescuing, "the step answers a prompt the gate refused");
}

#[test]
fn a_file_longer_than_the_cap_or_a_case_past_forty_lines_is_cut() {
    let long: String = format!(
        "def test_redirect_flow():\n{}",
        (0..60)
            .map(|n| format!("    step({n})\n"))
            .collect::<String>()
    );
    let repo = Repo::new(&[("tests/test_a.py", &long)]);
    let shown = ask(&repo, "where are the tests for the redirect flow", &[]);
    assert_eq!(
        heads(&shown),
        ["### tests/test_a.py:1-40 — test test_redirect_flow"]
    );
}

#[test]
fn the_output_stops_at_nine_thousand_bytes() {
    let body = "    assert redirect_target == \"/new\"  # padding padding padding padding\n";
    let cases: String = (0..30)
        .map(|n| format!("def test_redirect_{n}():\n{}\n\n", body.repeat(8)))
        .collect();
    let repo = Repo::new(&[("tests/test_a.py", &cases)]);
    let shown = ask(&repo, "where are the tests for redirect", &[]);
    let size: usize = shown.iter().map(|block| block.len() + 1).sum();
    assert!(!shown.is_empty() && size <= OUT_CAP, "{size}");
    assert!(shown.len() < 30);
}

#[test]
fn terms_split_identifiers_and_drop_function_words() {
    assert_eq!(
        terms("where are the tests for the RedirectHandler"),
        ["redirect", "handler"]
    );
    assert_eq!(
        terms("which tests cover config.toml loading"),
        ["config", "toml", "configtoml", "load"]
    );
    assert!(terms("which tests cover the").is_empty());
    assert_eq!(stem("handlers"), "handl", "`ers` is cut before `s`");
    assert_eq!(stem("configuration"), "configur");
    assert_eq!(
        requested_option("the --exec-batch option").as_deref(),
        Some("exec_batch")
    );
    assert_eq!(requested_option("the exec option").as_deref(), Some("exec"));
    assert_eq!(requested_option("no flags here"), None);
}
