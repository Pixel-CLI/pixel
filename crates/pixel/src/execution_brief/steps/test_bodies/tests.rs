// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The cases of `probes/test-bodies/tests/test_probe.py`, run on the step.

use super::super::testing::{headers, repo, run};
use super::*;

const PY_TESTS: &str = "import pytest


def test_follows_a_redirect_and_handles_the_new_location():
    response = get(\"/old\", follow=True)
    assert response.redirect_target == \"/new\"


def test_parses_the_header_case_insensitively():
    assert parse(\"X-A: 1\") == {\"x-a\": \"1\"}
";

fn ask(root: &std::path::Path, prompt: &str, brief: &str) -> String {
    run(&STEP, root, prompt, brief)
}

fn set(words: &[&str]) -> BTreeSet<String> {
    words.iter().map(ToString::to_string).collect()
}

#[test]
fn subject_matching_uses_words_and_inflections_not_arbitrary_prefixes() {
    let tokens = tokens("redirects handles balanced");
    assert!(carries(&tokens, "redirect"));
    assert!(carries(&tokens, "handl"));
    assert!(carries(&tokens, "balance"));
    let tokens = self::tokens("candidate production");
    assert!(!carries(&tokens, "can"));
    assert!(!carries(&tokens, "product"));
    assert!(carries(&set(&["balanced"]), "balance"));
    assert!(carries(&set(&["stored"]), "store"));
    assert!(carries(&set(&["parsing"]), "parse"));
}

#[test]
fn the_step_runs_for_a_matching_closed_gate_prompt_and_for_no_unrelated_one() {
    let asked = "where are the tests for redirect handling and what do they cover";
    assert!(STEP.applies(Some("tests"), asked, true));
    assert!(STEP.applies(None, asked, true), "the prompt alone is enough");
    assert!(!STEP.applies(Some("history"), asked, false));
    assert!(!STEP.applies(Some("review"), asked, false));
    for prompt in [
        "How do I test whether a dragon can balance a teacup?",
        "Can you test whether our release mascot can drink tea?",
        "Please test a concept for our product workshop.",
    ] {
        assert!(!STEP.applies(None, prompt, true), "{prompt}");
    }
}

#[test]
fn the_case_naming_the_subject_is_quoted_whole_and_the_unrelated_case_is_not() {
    let dir = repo(&[
        ("tests/test_client.py", PY_TESTS),
        ("src/client.py", "def get(path, follow=False):\n    pass\n"),
    ]);
    let out = ask(
        dir.path(),
        "where are the tests for redirect handling and what do they cover",
        "",
    );
    assert_eq!(
        headers(&out),
        ["### tests/test_client.py:4-6 — test test_follows_a_redirect_and_handles_the_new_location"]
    );
    assert!(
        out.contains("6|     assert response.redirect_target == \"/new\""),
        "{out}"
    );
    assert!(!out.contains("parses_the_header"), "{out}");
    assert!(out.ends_with('\n') && !out.ends_with("\n\n"), "{out:?}");
}

#[test]
fn a_rust_case_runs_to_its_closing_brace_inside_an_inline_test_module() {
    let source = "pub fn gate(score: u32) -> bool {
    score > 3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gate_opens_above_three() {
        assert!(gate(4));
        assert!(!gate(3));
    }
}
";
    let dir = repo(&[("src/gate.rs", source)]);
    let brief = "### src/gate.rs:1-3 — gate\n1| pub fn gate(score: u32) -> bool {\n";
    let out = ask(dir.path(), "which tests cover the gate", brief);
    assert_eq!(
        headers(&out),
        ["### src/gate.rs:9-13 — test the_gate_opens_above_three"]
    );
    assert!(out.contains("13|     }"), "{out}");
    assert!(!out.contains("14|"), "{out}");
}

#[test]
fn a_word_every_case_carries_does_not_bring_them_all_in() {
    // "check" is in every case: only the case that also names the parser is
    // the question's.
    let mut cases: String = (0..4)
        .map(|n| format!("def test_check_{n}():\n    check({n})\n\n\n"))
        .collect();
    cases.push_str("def test_check_parser():\n    check(parser())\n");
    let dir = repo(&[("tests/test_a.py", &cases)]);
    assert_eq!(
        headers(&ask(dir.path(), "which tests check the parser", "")),
        ["### tests/test_a.py:17-18 — test test_check_parser"]
    );
}

#[test]
fn a_range_the_brief_already_quotes_is_not_quoted_again() {
    let dir = repo(&[("tests/test_client.py", PY_TESTS)]);
    let prompt = "where are the tests for redirect handling";
    let quoted = "### tests/test_client.py:1-12 — tests\n1| import pytest\n";
    assert_eq!(ask(dir.path(), prompt, quoted), "");
    let elsewhere = "### tests/test_client.py:1-5 — tests\n1| import pytest\n";
    assert_eq!(
        headers(&ask(dir.path(), prompt, elsewhere)),
        ["### tests/test_client.py:4-6 — test test_follows_a_redirect_and_handles_the_new_location"],
        "a range that does not hold the whole case leaves it quotable"
    );
}

#[test]
fn a_test_file_the_brief_names_is_read_before_the_others() {
    let other = "def test_redirect_cookie():\n    assert redirect_cookie\n";
    let dir = repo(&[
        ("tests/test_client.py", PY_TESTS),
        ("tests/test_cookies.py", other),
    ]);
    let out = ask(
        dir.path(),
        "which tests cover redirect",
        "### src/client.py:1-3 — x\nsee tests/test_cookies.py for more\n",
    );
    assert_eq!(
        headers(&out)[0],
        "### tests/test_cookies.py:1-2 — test test_redirect_cookie",
        "{out}"
    );
}

#[test]
fn an_exact_option_name_does_not_select_a_sibling_option_case() {
    let options = "
#[test]
fn test_exec() {
    run(&[\"--exec\", \"echo\", \"{}\"]).assert_stdout(\"one.txt\");
    run(&[\"--exec\", \"echo\", \"{/.}\"]).assert_stdout(\"one\");
}

#[test]
fn test_exec_batch() {
    run(&[\"--exec-batch\", \"echo\", \"{}\"]).assert_stdout(\"one.txt\");
}
";
    let dir = repo(&[("tests/options.rs", options)]);
    let exact = ask(
        dir.path(),
        "where are the integration tests for the exec option and what behaviours do they cover",
        "",
    );
    assert_eq!(headers(&exact), ["### tests/options.rs:2-6 — test test_exec"]);
    assert!(exact.contains("--exec\", \"echo\""), "{exact}");
    assert!(!exact.contains("test_exec_batch"), "{exact}");

    let batch = ask(
        dir.path(),
        "where are the integration tests for the --exec-batch option",
        "",
    );
    assert_eq!(
        headers(&batch),
        ["### tests/options.rs:8-11 — test test_exec_batch"]
    );
    assert!(batch.contains("--exec-batch"), "{batch}");
    assert!(!batch.contains("test_exec()"), "{batch}");
}

#[test]
fn the_exact_option_selector_generalizes_to_other_option_names() {
    let options = "
#[test]
fn test_search_depth() {
    run(&[\"--search-depth\", \"4\"]).assert_depth(4);
}

#[test]
fn test_search_depth_default() {
    run(&[\"--search-depth\"]).assert_depth(8);
}
";
    let dir = repo(&[("tests/options.rs", options)]);
    let out = ask(
        dir.path(),
        "where are the integration tests for the --search-depth option",
        "",
    );
    assert_eq!(
        headers(&out),
        ["### tests/options.rs:2-5 — test test_search_depth"]
    );
}

#[test]
fn an_exact_option_needs_the_title_or_the_exact_flag_as_evidence() {
    let sibling = "
#[test]
fn test_exec_batch() {
    run(&[\"--exec-batch\", \"echo\", \"{}\"]).assert_stdout(\"one.txt\");
}
";
    let dir = repo(&[("tests/options.rs", sibling)]);
    assert_eq!(
        ask(dir.path(), "where are the tests for the exec option", ""),
        ""
    );

    let flag = "
#[test]
fn test_uses_command_override() {
    run(&[\"--exec\", \"echo\", \"{}\"]).assert_stdout(\"one.txt\");
}
";
    let dir = repo(&[("tests/options.rs", flag)]);
    let out = ask(dir.path(), "where are the tests for the exec option", "");
    assert_eq!(
        headers(&out),
        ["### tests/options.rs:2-5 — test test_uses_command_override"]
    );
}

#[test]
fn the_option_a_prompt_names_is_the_flag_or_the_word_before_option() {
    assert_eq!(
        requested_option("the --exec-batch flag").as_deref(),
        Some("exec_batch")
    );
    assert_eq!(
        requested_option("tests for the Search-Depth option").as_deref(),
        Some("search_depth")
    );
    assert_eq!(requested_option("tests for the parser"), None);
    assert!(exact_option_case("test_exec", Some("exec"), ""));
    assert!(exact_option_case("Spec exec", Some("exec"), ""));
    assert!(!exact_option_case("test_exec_batch", Some("exec"), ""));
    assert!(exact_option_case("x", Some("exec"), "a --exec b"));
    assert!(exact_option_case("x", Some("exec"), "--exec"));
    assert!(!exact_option_case("x", Some("exec"), "--exec-batch"));
    assert!(!exact_option_case("x", Some("exec"), "x--exec"));
    assert!(!exact_option_case("test_exec", None, "--exec"));
}

#[test]
fn a_js_case_ends_at_its_closing_line_and_the_spec_file_is_found_by_its_path() {
    let spec = "describe('undo', () => {
  it('restores the deleted shape on undo', () => {
    del(shape);
    undo();
    expect(shapes()).toContain(shape);
    // The observable behaviours stay stable.
  });

  it('keeps the colour', () => {
    expect(colour()).toBe('red');
  });
});
";
    let dir = repo(&[("src/history.test.ts", spec)]);
    let out = ask(
        dir.path(),
        "where are the tests for undo and what behaviours do they cover",
        "",
    );
    assert_eq!(
        headers(&out),
        ["### src/history.test.ts:2-7 — test restores the deleted shape on undo"]
    );
}

#[test]
fn a_weak_test_word_does_not_emit_unrelated_cases() {
    let dir = repo(&[(
        "tests/test_candidate_policy.py",
        "def test_candidate_policy_stays_explicit():
    assert balance_is_stable()

def test_general_case():
    assert can_run()
",
    )]);
    for prompt in [
        "How do I test whether a dragon can balance a teacup?",
        "Can you test whether our release mascot can drink tea?",
        "Please test a concept for our product workshop.",
    ] {
        assert_eq!(ask(dir.path(), prompt, ""), "", "{prompt}");
    }
}

#[test]
fn a_prompt_with_no_subject_word_quotes_nothing() {
    let dir = repo(&[("tests/test_client.py", PY_TESTS)]);
    assert_eq!(ask(dir.path(), "which tests are there", ""), "");
}

#[test]
fn a_repository_git_cannot_list_quotes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("tests")).unwrap();
    std::fs::write(dir.path().join("tests/test_client.py"), PY_TESTS).unwrap();
    assert_eq!(
        ask(dir.path(), "where are the tests for redirect handling", ""),
        ""
    );
}

#[test]
fn a_tracked_symlink_to_a_file_outside_the_repo_is_not_read() {
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("test_outside.py");
    std::fs::write(
        &target,
        "def test_redirect_location():\n    assert SECRET_TOKEN\n",
    )
    .unwrap();
    let dir = repo(&[("keep.txt", "x")]);
    std::fs::create_dir(dir.path().join("tests")).unwrap();
    std::os::unix::fs::symlink(&target, dir.path().join("tests/test_redirect.py")).unwrap();
    crate::execution_brief::steps::testing::git(dir.path(), &["add", "."]);
    let out = ask(dir.path(), "which tests cover the redirect location", "");
    assert!(!out.contains("SECRET_TOKEN"), "{out}");
    assert_eq!(headers(&out), Vec::<&str>::new());
}

#[test]
fn a_tracked_symlink_to_dev_zero_neither_hangs_nor_quotes() {
    let dir = repo(&[("keep.txt", "x")]);
    std::fs::create_dir(dir.path().join("tests")).unwrap();
    std::os::unix::fs::symlink("/dev/zero", dir.path().join("tests/test_redirect.py")).unwrap();
    crate::execution_brief::steps::testing::git(dir.path(), &["add", "."]);
    let out = ask(dir.path(), "which tests cover the redirect location", "");
    assert_eq!(headers(&out), Vec::<&str>::new());
}

#[test]
fn a_case_is_cut_at_forty_lines_and_a_long_line_at_one_hundred_sixty_characters() {
    let long = "x".repeat(300);
    let body: String = (0..60).map(|n| format!("    step({n})  # {long}\n")).collect();
    let source = format!("def test_parser_cutoff():\n{body}");
    let dir = repo(&[("tests/test_parser.py", &source)]);
    let out = ask(dir.path(), "which tests cover the parser cutoff", "");
    assert_eq!(
        headers(&out),
        ["### tests/test_parser.py:1-40 — test test_parser_cutoff"]
    );
    let widest = out.lines().map(|line| line.chars().count()).max().unwrap();
    assert_eq!(widest, "40| ".len() + LINE_CHARS, "{out}");
}

#[test]
fn the_output_stops_at_nine_thousand_characters_and_keeps_the_best_cases() {
    let mut source = String::new();
    for n in 0..30 {
        source.push_str(&format!("def test_parser_{n}():\n"));
        for k in 0..30 {
            source.push_str(&format!("    parser_check({n}, {k}) + {}\n", "y".repeat(50)));
        }
        source.push_str("\n\n");
    }
    // A word that names a quarter of the test files or more names no subject:
    // three other files keep "parser" a path word of this one.
    let dir = repo(&[
        ("tests/test_parser.py", &source),
        ("tests/test_a.py", "def test_a():\n    pass\n"),
        ("tests/test_b.py", "def test_b():\n    pass\n"),
        ("tests/test_c.py", "def test_c():\n    pass\n"),
    ]);
    let out = ask(dir.path(), "which tests cover the parser", "");
    let size = out.trim_end_matches('\n').chars().count();
    assert!(size <= OUT_CAP, "{size}");
    assert!(size > OUT_CAP - 2_000, "{size}");
    assert!(headers(&out).len() < 30, "{}", headers(&out).len());
    assert_eq!(
        headers(&out)[0],
        "### tests/test_parser.py:1-31 — test test_parser_0",
        "equal scores keep the file's order"
    );
}

#[test]
fn terms_stem_split_and_drop_function_words() {
    assert_eq!(
        terms("where are the tests for redirect handling"),
        ["redirect", "handl"]
    );
    assert_eq!(terms("followRedirect"), ["follow", "redirect"]);
    assert_eq!(
        terms("follow_redirect"),
        ["follow", "redirect", "followredirect"]
    );
    assert_eq!(terms("a.b_c"), Vec::<String>::new());
    assert_eq!(terms("parser parser Parser"), ["parser"]);
    assert_eq!(stem("Redirections"), "redirection");
    assert_eq!(stem("run"), "run");
    assert_eq!(stem("tests"), "test");
}

#[test]
fn a_python_case_ends_where_the_indent_returns() {
    let lines: Vec<String> = ["def test_a():", "    x = 1", "", "    assert x", "", "", "y = 2"]
        .map(ToString::to_string)
        .to_vec();
    assert_eq!(units(&lines, "t.py"), [("test_a".to_owned(), 1, 4)]);
    let two: Vec<String> = ["def test_a():", "    pass", "def test_b():", "    pass"]
        .map(ToString::to_string)
        .to_vec();
    assert_eq!(
        units(&two, "t.py"),
        [("test_a".to_owned(), 1, 2), ("test_b".to_owned(), 3, 4)]
    );
}
