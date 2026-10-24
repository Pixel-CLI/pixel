// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::*;
use crate::chain::{CallerHit, Evidence, Found, SymbolHit};
use crate::region_pack::Block;
use crate::steps::testing::{Repo, texts};
use crate::steps::{Outcome, Steps, blocks};

const WHY: &str =
    "Why does it stop printing matches for a binary file, and which tests prove that?";
const WHY_NO_TESTS: &str = "Why does it stop printing matches for a binary file?";
/// Three rare words, no mention of tests.
const SHORT: &str = "Why does it stop printing matches?";

/// Only this step runs: the others would answer some prompts too.
fn only_stop_cause() -> Steps {
    let others: Vec<String> = Step::ALL
        .into_iter()
        .filter(|step| *step != Step::CandJStopCause)
        .map(|step| step.name().to_string())
        .collect();
    Steps::default().without(&others).unwrap()
}

/// A repository from `(path, lines)`.
fn repo(files: &[(&str, &[&str])]) -> Repo {
    let owned: Vec<(&str, String)> = files
        .iter()
        .map(|(path, lines)| (*path, lines.join("\n") + "\n"))
        .collect();
    let pairs: Vec<(&str, &str)> = owned
        .iter()
        .map(|(path, text)| (*path, text.as_str()))
        .collect();
    Repo::new(&pairs)
}

fn ask_as(
    evidence: &dyn Evidence,
    typed: &str,
    kind: QuestionKind,
    near: &[&str],
    steps: Steps,
) -> Outcome {
    let near: Vec<String> = near.iter().map(ToString::to_string).collect();
    let deadline = Instant::now() + Duration::from_secs(20);
    blocks(typed, kind, steps, &near, evidence, deadline)
}

/// The step's blocks for `typed`.
fn ask(evidence: &dyn Evidence, typed: &str) -> Vec<String> {
    texts(
        &ask_as(
            evidence,
            typed,
            QuestionKind::Bugfix,
            &[],
            only_stop_cause(),
        )
        .lead,
    )
}

fn heads(blocks: &[String]) -> Vec<&str> {
    blocks
        .iter()
        .map(|block| block.lines().next().unwrap())
        .collect()
}

fn block(head: &str, body: &[String]) -> String {
    format!("### {head}\n{}", body.join("\n"))
}

/// `lines` numbered from `first`, blank ones skipped.
fn numbered(first: usize, lines: &[&str]) -> Vec<String> {
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| format!("{}| {line}", first + index))
        .collect()
}

fn strings(raw: &[&str]) -> Vec<String> {
    raw.iter().map(ToString::to_string).collect()
}

fn source(raw: &[&str]) -> Source {
    Source::new(strings(raw))
}

fn group(weight: f64, members: &[&str]) -> Group {
    Group {
        weight,
        members: members.iter().map(ToString::to_string).collect(),
    }
}

/// A repository of owned files.
fn repo_of(files: &[(String, Vec<String>)]) -> Repo {
    let pairs: Vec<(&str, &[String])> = files
        .iter()
        .map(|(path, lines)| (path.as_str(), lines.as_slice()))
        .collect();
    let owned: Vec<(&str, String)> = pairs
        .iter()
        .map(|(path, lines)| (*path, lines.join("\n") + "\n"))
        .collect();
    let refs: Vec<(&str, &str)> = owned
        .iter()
        .map(|(path, text)| (*path, text.as_str()))
        .collect();
    Repo::new(&refs)
}

/// `n` files that say nothing of the question, so that its words are rare.
fn fillers(n: usize) -> Vec<(String, Vec<String>)> {
    (0..n)
        .map(|i| {
            (
                format!("src/f{i:02}.rs"),
                strings(&["pub fn f() {", "    let x = 1;", "}"]),
            )
        })
        .collect()
}

/// A corpus of one-line files, to count document frequencies on.
fn corpus(rows: &[&str]) -> BTreeMap<String, Source> {
    rows.iter()
        .enumerate()
        .map(|(i, row)| (format!("src/c{i}.rs"), source(&[row])))
        .collect()
}

fn assert_close(left: f64, right: f64) {
    assert!((left - right).abs() < 1e-9, "{left} != {right}");
}

const HIARGS: &[&str] = &[
    "pub fn binary_mode(low: &Low) -> Binary {",
    "    // Quit on binary data so that nothing is printed.",
    "    let convert = matches!(low.binary, Mode::Convert);",
    "    let explicit = if convert {",
    "        Binary::convert(0)",
    "    } else {",
    "        Binary::quit(0)",
    "    };",
    "    Binary { explicit }",
    "}",
];
const SUMMARY: &[&str] = &[
    "impl Summary {",
    "    pub fn finish(&mut self, searcher: &Searcher) -> Result {",
    "        // If binary detection says to quit after binary data,",
    "        // then we should not print any results at all.",
    "        if self.binary_offset.is_some() && searcher.quit_byte().is_some() {",
    "            self.stats.squash(0);",
    "            return Ok(());",
    "        }",
    "        self.print_matches()?;",
    "        Ok(())",
    "    }",
    "}",
];
const BUFFER: &[&str] = &[
    "impl LineBuffer {",
    "    pub fn fill(&mut self) -> io::Result<bool> {",
    "        let n = self.rdr.read(&mut self.buf)?;",
    "        // Stop reading: no more matches are printed.",
    "        if self.binary.quit(&self.buf[..n]) {",
    "            self.last_lineno = self.pos;",
    "            return Ok(false);",
    "        }",
    "        Ok(n > 0)",
    "    }",
    "}",
];
const BINARY_TESTS: &[&str] = &[
    "#[test]",
    "fn binary_quit_stops_printing_matches() {",
    "    let out = rg(\"-a\", \"foo\");",
    "    assert_eq!(out.matches, 0);",
    "}",
    "",
    "#[test]",
    "fn binary_convert_prints_matches() {",
    "    let out = rg(\"--binary\", \"foo\");",
    "    assert_eq!(out.matches, 1);",
    "}",
    "",
    "fn rg(flag: &str, pat: &str) -> Out {",
    "    Out::run(flag, pat)",
    "}",
];

const FILLERS: [(&str, &[&str]); 4] = [
    ("src/a.rs", &["pub fn a() {", "    let x = 1;", "}"]),
    ("src/b.rs", &["pub fn b() {", "    let x = 2;", "}"]),
    ("src/c.rs", &["pub fn c() {", "    let x = 3;", "}"]),
    ("src/d.rs", &["pub fn d() {", "    let x = 4;", "}"]),
];

/// The ripgrep shape of the question: the glue that decides, a line buffer, a
/// summary printer and a tests file, among files that say nothing of it.
fn binary_repo() -> Repo {
    let mut files: Vec<(&str, &[&str])> = vec![
        ("src/hiargs.rs", HIARGS),
        ("src/summary.rs", SUMMARY),
        ("src/line_buffer.rs", BUFFER),
        ("tests/binary.rs", BINARY_TESTS),
    ];
    files.extend(FILLERS);
    repo(&files)
}

#[test]
fn the_words_of_the_question_meet_in_the_code_and_the_tests_that_pin_them_down() {
    let shown = ask(&binary_repo(), WHY);
    assert_eq!(
        shown,
        vec![
            block(
                "src/hiargs.rs:1-10 — where the question's words meet (code)",
                &numbered(1, HIARGS)
            ),
            block(
                "src/line_buffer.rs:1-11 — where the question's words meet (code)",
                &numbered(1, BUFFER)
            ),
            block(
                "src/summary.rs:1-12 — where the question's words meet (code)",
                &numbered(1, SUMMARY)
            ),
            block(
                "tests/binary.rs:1-5 — where the question's words meet (test)",
                &numbered(1, &BINARY_TESTS[..5])
            ),
            block(
                "tests/binary.rs:7-11 — where the question's words meet (test)",
                &numbered(7, &BINARY_TESTS[6..11])
            ),
        ]
    );
}

#[test]
fn the_tests_follow_only_when_the_question_asks_for_them() {
    assert_eq!(
        heads(&ask(&binary_repo(), WHY_NO_TESTS)),
        vec![
            "### src/hiargs.rs:1-10 — where the question's words meet (code)",
            "### src/line_buffer.rs:1-11 — where the question's words meet (code)",
            "### src/summary.rs:1-12 — where the question's words meet (code)",
        ]
    );
}

#[test]
fn a_block_is_a_head_and_numbered_lines_cut_at_two_hundred_characters_with_blank_lines_skipped() {
    let long = format!("    call({}); // stop", "x".repeat(300));
    let mut files = fillers(3);
    files.push((
        "src/glue.rs".to_string(),
        strings(&[
            "pub fn glue() {",
            "    // stop printing matches",
            "",
            &long,
            "}",
        ]),
    ));
    let shown = ask(&repo_of(&files), SHORT);
    assert_eq!(
        shown,
        vec![block(
            "src/glue.rs:1-5 — where the question's words meet (code)",
            &[
                "1| pub fn glue() {".to_string(),
                "2|     // stop printing matches".to_string(),
                format!("4| {}", long.chars().take(200).collect::<String>()),
                "5| }".to_string(),
            ]
        )]
    );
}

#[test]
fn only_a_why_question_with_an_effect_word_after_the_why_is_answered() {
    let repo = binary_repo();
    assert!(!ask(&repo, WHY).is_empty());
    for typed in [
        "How does it stop printing matches for a binary file?",
        "Why does it print matches for a binary file with tests?",
        "Stops: why is that printing matches for a binary file?",
    ] {
        assert_eq!(ask(&repo, typed), Vec::<String>::new(), "{typed}");
    }
    for effect in [
        "stops",
        "stopped",
        "skips",
        "skipped",
        "ignored",
        "hides",
        "hidden",
        "suppresses",
        "refuses",
        "never",
        "silently",
        "missing",
        "drops",
        "dropped",
        "fails",
        "not",
    ] {
        let typed = format!("Why does it {effect} printing matches for a binary file?");
        assert!(!ask(&repo, &typed).is_empty(), "{typed}");
    }
}

#[test]
fn the_overview_review_and_history_questions_are_left_alone() {
    let repo = binary_repo();
    for kind in [
        QuestionKind::Overview,
        QuestionKind::Review,
        QuestionKind::History,
    ] {
        let out = ask_as(&repo, WHY, kind, &[], only_stop_cause());
        assert_eq!(out, Outcome::default(), "{kind:?}");
    }
    for kind in [
        QuestionKind::Lookup,
        QuestionKind::Flow,
        QuestionKind::Tests,
        QuestionKind::Config,
        QuestionKind::Rationale,
        QuestionKind::Bugfix,
    ] {
        let out = ask_as(&repo, WHY, kind, &[], only_stop_cause());
        assert_eq!(out.lead.len(), 5, "{kind:?}");
    }
}

#[test]
fn the_flag_turns_the_step_off_and_a_spent_deadline_ends_it() {
    let repo = binary_repo();
    let off = Steps::default()
        .without(&["cand-j-stop-cause".to_string()])
        .unwrap();
    assert!(
        ask_as(&repo, WHY, QuestionKind::Bugfix, &[], off)
            .lead
            .is_empty()
    );
    let spent = Instant::now();
    let out = blocks(
        WHY,
        QuestionKind::Bugfix,
        only_stop_cause(),
        &[],
        &repo,
        spent,
    );
    assert_eq!(out.lead, Vec::new());
    assert!(!out.rescuing);
}

#[test]
fn the_step_opens_a_closed_gate_and_drops_every_block_that_is_not_a_definition() {
    let out = ask_as(
        &binary_repo(),
        WHY,
        QuestionKind::Bugfix,
        &[],
        only_stop_cause(),
    );
    assert!(out.rescuing);
    assert!(out.drops.drops_head("src/x.rs:1-9 — x usage in y"));
    assert!(
        !out.drops
            .drops_head("src/x.rs:1-9 — x definition (function)")
    );
    let quiet = ask_as(
        &repo(&FILLERS),
        WHY,
        QuestionKind::Bugfix,
        &[],
        only_stop_cause(),
    );
    assert!(!quiet.rescuing);
    assert!(quiet.drops.is_empty());
}

#[test]
fn a_definition_block_stays_and_every_other_ranged_block_goes() {
    let drops =
        |head: &str| Drop::NotDefinitions.matches(&Block::new(head.to_string(), Vec::new()));
    for kept in [
        "src/a.rs:1-9 — foo definition (function)",
        "src/a.rs:1-9 — Foo definition (struct) with its neighbours",
        "src/a.rs:1-9 — Foo definition (named by the prompt: 'foo bar')",
        "src/a.rs:1-9 — module named by the prompt",
        "config registry — keys matching foo (key | value)",
        "call trace",
        "tests matching stop (2 found)",
    ] {
        assert!(!drops(kept), "{kept}");
    }
    for gone in [
        "src/a.rs:1-9 — foo region",
        "src/a.rs:1-9 — foo usage in bar",
        "src/a.rs:1-9 — whole file (best match)",
        "src/a.rs:1-9 — foo match",
        "src/a.rs:1-9 — bar (a test of foo)",
        "src/a.rs:10-19 — where the question's words meet (code)",
        "src/a.rs:1-9",
    ] {
        assert!(drops(gone), "{gone}");
    }
}

#[test]
fn a_word_stems_by_one_ending_when_four_letters_stay() {
    for (word, stem) in [
        ("printing", "print"),
        ("matches", "match"),
        ("stopped", "stopp"),
        ("Stops", "stop"),
        ("boats", "boat"),
        ("runs", "runs"),
        ("sing", "sing"),
        ("ties", "ties"),
        ("fixes", "fixe"),
    ] {
        assert_eq!(root(word), stem, "{word}");
    }
}

#[test]
fn a_word_in_more_than_half_the_files_or_in_none_names_no_group() {
    let some = |rows: &[&str]| groups_of("binary", &corpus(rows)).len();
    assert_eq!(some(&["binary", "binary", "x", "x"]), 1, "half the files");
    assert_eq!(
        some(&["binary", "binary", "binary", "x"]),
        0,
        "three of four"
    );
    assert_eq!(some(&["x", "x", "x", "x"]), 0, "none");
    assert_eq!(some(&["binary", "x", "x", "x"]), 1, "one of four");
}

#[test]
fn a_group_weighs_the_square_of_the_log_of_how_rare_its_word_is() {
    let groups = groups_of("binary", &corpus(&["binary", "x", "x", "x"]));
    assert_eq!(groups.len(), 1);
    assert_close(groups[0].weight, 4.0_f64.ln().powi(2));
    let groups = groups_of("binary", &corpus(&["binary", "binary", "x", "x"]));
    assert_close(groups[0].weight, 2.0_f64.ln().powi(2));
}

#[test]
fn the_function_words_and_the_words_that_frame_a_question_name_no_group() {
    let stop: Vec<&str> = STOP
        .split_whitespace()
        .filter(|word| word.len() >= 4 && !FRAME.iter().any(|frame| word.starts_with(frame)))
        .collect();
    let framing = "testing provided proofs coverage defined callers behaves working happens decided explained meaning";
    for words in [stop.join(" "), framing.to_string()] {
        let groups = groups_of(&words, &corpus(&[&words, "x", "x", "x"]));
        assert_eq!(groups, Vec::new(), "{words}");
    }
    let both = format!("binary {framing}");
    let groups = groups_of(&both, &corpus(&[&both, "x", "x", "x"]));
    assert_eq!(groups, vec![group(4.0_f64.ln().powi(2), &["binary"])]);
}

#[test]
fn a_word_comes_once_whatever_its_case_and_the_groups_keep_the_order_of_the_prompt() {
    let rows = corpus(&["binary stop", "x", "x", "x"]);
    let groups = groups_of("binary Binary BINARY stop", &rows);
    assert_eq!(groups.len(), 2);
    assert!(groups[0].members.contains("binary"));
    assert!(groups[1].members.contains("stop"));
}

#[test]
fn a_verb_brings_its_synonyms() {
    let stop = [
        "stop",
        "quit",
        "halt",
        "abort",
        "terminate",
        "bail",
        "cancel",
    ];
    let skip = [
        "skip", "suppres", "ignore", "hide", "filter", "refuse", "reject", "discard", "omit",
        "exclude",
    ];
    let print = ["print", "display", "output", "emit"];
    let fail = ["fail", "error", "crash", "panic"];
    let members = |word: &str, row: &str| -> Vec<String> {
        let groups = groups_of(word, &corpus(&[row, "x", "x", "x"]));
        assert_eq!(groups.len(), 1, "{word}");
        groups[0].members.iter().cloned().collect()
    };
    let sorted = |words: &[&str]| -> Vec<String> {
        let mut words = words.to_vec();
        words.sort_unstable();
        words.into_iter().map(ToString::to_string).collect()
    };
    assert_eq!(members("stop", "stop"), sorted(&stop));
    assert_eq!(members("quits", "quit"), sorted(&stop));
    assert_eq!(members("skips", "skip"), sorted(&skip));
    assert_eq!(members("suppress", "suppres"), sorted(&skip));
    assert_eq!(members("printing", "print"), sorted(&print));
    assert_eq!(members("errors", "error"), sorted(&fail));
    assert_eq!(members("stopped", "stopp"), vec!["stopp".to_string()]);
    assert_eq!(members("binary", "binary"), vec!["binary".to_string()]);
}

#[test]
fn a_literal_that_spans_lines_is_prose_and_the_line_that_closes_it_too() {
    let flags = |raw: &[&str]| prose_lines(&strings(raw));
    assert_eq!(
        flags(&["let a = r#\"x", "y", "z\"#;", "after"]),
        [true, true, true, false]
    );
    assert_eq!(flags(&["let a = r\"x\";", "after"]), [false, false]);
    assert_eq!(
        flags(&["let a = r##\"x", "a\"#b", "\"##;", "after"]),
        [true, true, true, false]
    );
    assert_eq!(
        flags(&[
            "doc = \"\"\"",
            "text",
            "\"\"\"",
            "x = \"\"\"a\"\"\"",
            "after"
        ]),
        [true, true, true, false, false]
    );
    assert_eq!(
        flags(&["const T = `a", "b`;", "const U = `c`;"]),
        [true, true, false]
    );
}

#[test]
fn a_line_is_code_when_it_ends_a_statement_or_calls_and_is_no_comment() {
    for code in ["let a = 1;", "if x {", "}", "f(a,", "g(b)", "    a.b(c).d"] {
        assert!(code_like(code), "{code}");
    }
    for other in [
        "",
        "   ",
        "// f(a);",
        "# f(a);",
        "* f(a);",
        "/* f(a);",
        "\"f(a);\"",
        "'f(a);'",
        "let a = b",
    ] {
        assert!(!code_like(other), "{other}");
    }
}

/// `n` code lines that hold no group word.
fn pad(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("    let v{i} = f({i});")).collect()
}

fn windows_of(lines: &[String], groups: &[Group]) -> Vec<(usize, usize)> {
    windows(&Source::new(lines.to_vec()), groups)
        .iter()
        .map(|window| (window.first, window.last))
        .collect()
}

fn alpha_beta_gamma() -> Vec<Group> {
    vec![
        group(3.0, &["alpha"]),
        group(2.0, &["beta"]),
        group(1.0, &["gamma"]),
    ]
}

#[test]
fn a_window_must_hold_the_two_heaviest_groups() {
    let with = |words: &[&str]| -> Vec<(usize, usize)> {
        let mut lines = pad(4);
        for (at, word) in words.iter().enumerate() {
            lines[at] = format!("    f({word});");
        }
        windows_of(&lines, &alpha_beta_gamma())
    };
    assert_eq!(with(&["alpha", "beta"]), [(1, 4)]);
    assert_eq!(with(&["alpha", "beta", "gamma"]), [(1, 4)]);
    assert_eq!(with(&["alpha", "gamma"]), []);
    assert_eq!(with(&["beta", "gamma"]), []);
    assert_eq!(with(&["alpha"]), []);
    assert_eq!(with(&[]), []);
}

#[test]
fn the_two_heaviest_groups_are_the_two_by_weight_not_the_first_two() {
    let groups = vec![
        group(1.0, &["gamma"]),
        group(3.0, &["alpha"]),
        group(2.0, &["beta"]),
    ];
    let mut lines = pad(4);
    lines[0] = "    f(alpha);".to_string();
    lines[1] = "    f(beta);".to_string();
    assert_eq!(windows_of(&lines, &groups), [(1, 4)]);
    lines[1] = "    f(gamma);".to_string();
    assert_eq!(windows_of(&lines, &groups), []);
}

#[test]
fn a_window_scores_each_group_by_its_weight_and_how_often_it_occurs() {
    let mut lines = pad(4);
    lines[0] = "    f(alpha, alpha);".to_string();
    lines[1] = "    f(beta);".to_string();
    lines[2] = "    f(gamma);".to_string();
    let found = windows(&Source::new(lines), &alpha_beta_gamma());
    assert_eq!(found.len(), 1);
    assert_close(
        found[0].score,
        3.0 * (1.0 + 2.0_f64.ln()) + 2.0 * (1.0 + 1.0_f64.ln()) + 1.0 * (1.0 + 1.0_f64.ln()),
    );
}

#[test]
fn a_window_is_thirty_lines_and_slides_one_line_at_a_time() {
    let at = |alpha: usize, beta: usize, n: usize| {
        let mut lines = pad(n);
        lines[alpha - 1] = "    f(alpha);".to_string();
        lines[beta - 1] = "    f(beta);".to_string();
        windows_of(&lines, &alpha_beta_gamma())
    };
    assert_eq!(at(5, 8, 40), [(1, 30), (2, 31), (3, 32), (4, 33), (5, 34)]);
    assert_eq!(at(1, 30, 40), [(1, 30)]);
    assert_eq!(at(1, 31, 40), []);
    assert_eq!(at(2, 31, 40), [(2, 31)]);
    assert_eq!(at(11, 40, 40), [(11, 40)]);
    assert_eq!(at(3, 6, 10), [(1, 10)]);
}

/// Thirty code lines with `alpha` and `beta`, and a raw string of `literal`
/// lines where the padding was.
fn with_literal(literal: usize) -> Vec<String> {
    let mut lines = pad(30);
    lines[0] = "    f(alpha);".to_string();
    lines[1] = "    f(beta);".to_string();
    lines[2] = "    let s = r#\"".to_string();
    for line in lines.iter_mut().skip(3).take(literal - 2) {
        *line = "text".to_string();
    }
    lines[literal + 1] = "\"#;".to_string();
    lines
}

#[test]
fn a_window_with_more_than_a_tenth_of_its_lines_in_a_literal_is_prose() {
    let groups = alpha_beta_gamma();
    assert_eq!(windows_of(&with_literal(3), &groups), [(1, 30)], "3 of 30");
    assert_eq!(windows_of(&with_literal(4), &groups), [], "4 of 30");
}

#[test]
fn a_window_needs_a_third_of_its_lines_to_be_code() {
    let groups = alpha_beta_gamma();
    let lines = |code: usize| -> Vec<String> {
        let mut lines = vec!["// alpha beta".to_string(); 20];
        for line in lines.iter_mut().take(code) {
            *line = "let x = 1;".to_string();
        }
        lines
    };
    assert_eq!(windows_of(&lines(7), &groups), [(1, 20)], "7 of 20");
    assert_eq!(windows_of(&lines(6), &groups), [], "6 of 20");
}

fn unit(first: usize, last: usize, name: &str) -> Unit {
    Unit {
        first,
        last,
        name: name.to_string(),
    }
}

#[test]
fn a_test_unit_is_a_marked_or_test_named_function_a_case_macro_or_a_titled_case() {
    let lines = strings(&[
        "/// Docs for it.",
        "#[test]",
        "fn marked() {",
        "    let s = \"{\"; // }",
        "    f('}');",
        "}",
        "",
        "fn plain() {",
        "}",
        "fn test_named() {",
        "    body();",
        "}",
        "check!(case_a, |x| {",
        "    run(x);",
        "});",
        "it('stops at binary', () => {",
        "  run();",
        "});",
    ]);
    assert_eq!(
        units("src/lib.rs", &lines),
        vec![
            unit(1, 6, "marked"),
            unit(10, 12, "test_named"),
            unit(13, 15, "case_a"),
            unit(16, 18, "stops at binary"),
        ]
    );
    assert_eq!(
        units("tests/lib.rs", &lines)
            .iter()
            .map(|unit| unit.name.as_str())
            .collect::<Vec<_>>(),
        ["marked", "plain", "test_named", "case_a", "stops at binary"]
    );
}

#[test]
fn a_unit_without_braces_ends_where_the_indentation_does_and_not_on_a_blank_line() {
    let lines = strings(&[
        "def test_a():",
        "    x = 1",
        "",
        "    assert x",
        "",
        "def helper():",
        "    pass",
    ]);
    assert_eq!(
        units("tests/test_a.py", &lines),
        vec![unit(1, 4, "test_a"), unit(6, 7, "helper")]
    );
    assert_eq!(
        units(
            "pkg/a.py",
            &strings(&["@pytest.mark.x", "def helper():", "    pass"])
        ),
        vec![unit(1, 3, "helper")]
    );
}

#[test]
fn a_marker_above_a_blank_line_does_not_reach_the_function() {
    let lines = strings(&["#[test]", "", "fn plain() {", "}"]);
    assert_eq!(units("src/lib.rs", &lines), Vec::new());
}

fn zz() -> Vec<Group> {
    vec![group(1.0, &["zz"])]
}

/// `n` lines, `zz` on the given ones.
fn lines_with_zz(n: usize, at: &[usize]) -> Source {
    let mut lines: Vec<String> = (1..=n).map(|i| format!("line {i}")).collect();
    for line in at {
        lines[line - 1] = format!("line {line} zz");
    }
    Source::new(lines)
}

#[test]
fn a_window_is_recentred_on_the_weighted_centroid_of_its_group_words() {
    assert_eq!(
        recentre(&lines_with_zz(40, &[12, 13]), 10, 13, &zz()),
        (10, 13),
        "a centre of 12.5 rounds to even"
    );
    assert_eq!(
        recentre(&lines_with_zz(40, &[14, 15]), 12, 17, &zz()),
        (12, 17),
        "a centre of 14.5 less 3 rounds to even"
    );
    assert_eq!(recentre(&lines_with_zz(40, &[30]), 1, 10, &zz()), (1, 10));
    assert_eq!(recentre(&lines_with_zz(20, &[14]), 5, 14, &zz()), (9, 18));
    assert_eq!(recentre(&lines_with_zz(20, &[19]), 10, 19, &zz()), (11, 20));
    assert_eq!(recentre(&lines_with_zz(20, &[2]), 1, 10, &zz()), (1, 10));
    let groups = vec![group(1.0, &["aa"]), group(2.0, &["bb"])];
    let mut lines: Vec<String> = (1..=30).map(|i| format!("line {i}")).collect();
    lines[9] = "aa".to_string();
    lines[13] = "bb".to_string();
    assert_eq!(recentre(&Source::new(lines), 8, 15, &groups), (9, 16));
}

#[test]
fn a_window_is_cut_to_the_group_words_with_two_lines_around_and_grown_to_eight() {
    let cut = |at: &[usize], a: usize, b: usize| trim(&lines_with_zz(40, at), a, b, &zz());
    assert_eq!(cut(&[10, 25], 1, 30), (8, 27), "twenty lines stay");
    assert_eq!(cut(&[15], 1, 30), (11, 19), "five lines grow to nine");
    assert_eq!(cut(&[2], 1, 30), (1, 8), "the start bounds the growth");
    assert_eq!(cut(&[29], 1, 30), (23, 30), "the end bounds the growth");
    assert_eq!(cut(&[3], 1, 5), (1, 5), "both bounds hold");
    assert_eq!(cut(&[], 4, 20), (4, 20), "no word, no cut");
    assert_eq!(cut(&[15], 14, 16), (14, 16), "a window already inside");
}

fn snapped(raw: &[&str], a: usize, b: usize) -> (usize, usize) {
    snap(&source(raw), a, b)
}

#[test]
fn a_window_starting_in_a_comment_run_starts_at_the_run() {
    let mut lines = vec!["code();"; 3];
    lines.extend(["// c1", "// c2", "# c3", "* c4", "/* c5"]);
    lines.extend(["code();"; 3]);
    assert_eq!(snapped(&lines, 7, 8), (4, 8));
    assert_eq!(snapped(&lines, 4, 8), (4, 8));
    assert_eq!(snapped(&lines, 9, 10), (9, 10), "not a comment");
    let long = vec!["// c"; 20];
    assert_eq!(snapped(&long, 20, 20), (10, 20), "ten lines at most");
}

#[test]
fn a_window_ending_inside_an_open_brace_ends_where_it_closes_within_ten_lines() {
    let mut lines = vec!["fn a() {"];
    lines.extend(["    x();"; 4]);
    lines.push("}");
    lines.extend(["y();"; 3]);
    assert_eq!(snapped(&lines, 1, 3), (1, 6));
    assert_eq!(snapped(&lines, 1, 6), (1, 6), "already closed");
    let far = |body: usize| {
        let mut lines = vec!["fn a() {"];
        lines.extend(vec!["    x();"; body]);
        lines.push("}");
        lines
    };
    assert_eq!(
        snapped(&far(19), 1, 11),
        (1, 21),
        "the brace is ten lines on"
    );
    assert_eq!(snapped(&far(20), 1, 11), (1, 11), "eleven lines on");
    assert_eq!(
        snapped(&far(3), 2, 4),
        (2, 4),
        "the open brace is before the window"
    );
    assert_eq!(snapped(&far(3), 1, 4), (1, 5), "the brace is the last line");
}

/// Eight code lines; the fourth carries the three words `reps` times.
fn trio(reps: usize) -> Vec<String> {
    let mut lines = pad(8);
    lines[3] = format!(
        "    f(); // {}",
        "stop print match ".repeat(reps).trim_end()
    );
    lines
}

fn path_heads(shown: &[String]) -> Vec<String> {
    shown
        .iter()
        .map(|block| {
            let head = block.lines().next().unwrap();
            let range = head.split(" — ").next().unwrap();
            range.trim_start_matches("### ").to_string()
        })
        .collect()
}

fn paths(shown: &[String]) -> Vec<String> {
    path_heads(shown)
        .iter()
        .map(|head| head.split(':').next().unwrap().to_string())
        .collect()
}

fn with_fillers(mut files: Vec<(String, Vec<String>)>, n: usize) -> Vec<(String, Vec<String>)> {
    files.extend(fillers(n));
    files
}

#[test]
fn the_five_best_files_are_shown_with_the_best_window_of_each() {
    let files: Vec<(String, Vec<String>)> = (1..=6)
        .map(|reps| (format!("src/s{reps}.rs"), trio(reps)))
        .collect();
    let shown = ask(&repo_of(&with_fillers(files, 8)), SHORT);
    assert_eq!(
        path_heads(&shown),
        [
            "src/s2.rs:1-8",
            "src/s3.rs:1-8",
            "src/s4.rs:1-8",
            "src/s5.rs:1-8",
            "src/s6.rs:1-8"
        ]
    );
}

#[test]
fn a_file_shows_its_best_window_and_the_first_of_two_equal_ones() {
    let cluster = |lines: &mut Vec<String>, at: usize, twice: bool| {
        let (a, b) = if twice {
            (", stop", ", print")
        } else {
            ("", "")
        };
        lines[at - 1] = format!("    f(stop{a});");
        lines[at] = format!("    f(print{b});");
        lines[at + 1] = format!("    f(match{});", if twice { ", match" } else { "" });
    };
    let mut better_later = pad(100);
    cluster(&mut better_later, 10, false);
    cluster(&mut better_later, 70, true);
    let mut equal = pad(100);
    cluster(&mut equal, 10, false);
    cluster(&mut equal, 70, false);
    let shown = |lines: Vec<String>| {
        let files = with_fillers(vec![("src/big.rs".to_string(), lines)], 4);
        path_heads(&ask(&repo_of(&files), SHORT))
    };
    assert_eq!(shown(better_later), ["src/big.rs:67-75"]);
    assert_eq!(shown(equal), ["src/big.rs:7-15"]);
}

#[test]
fn a_file_named_after_a_group_word_weighs_more_and_so_does_a_file_the_brief_points_at() {
    let files = |extra: &str| -> Vec<(String, Vec<String>)> {
        let mut files: Vec<(String, Vec<String>)> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|name| (format!("src/{name}.rs"), trio(1)))
            .collect();
        files.push((format!("src/{extra}.rs"), trio(1)));
        with_fillers(files, 8)
    };
    let shown = |extra: &str, near: &[&str]| {
        let out = ask_as(
            &repo_of(&files(extra)),
            SHORT,
            QuestionKind::Bugfix,
            near,
            only_stop_cause(),
        );
        paths(&texts(&out.lead))
    };
    let plain = ["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs", "src/e.rs"];
    assert_eq!(shown("zz", &[]), plain);
    assert_eq!(shown("stop/zz", &[]), plain, "a directory is no file name");
    assert_eq!(
        shown("zz_stop", &[]),
        [
            "src/a.rs",
            "src/b.rs",
            "src/c.rs",
            "src/d.rs",
            "src/zz_stop.rs"
        ]
    );
    assert_eq!(
        shown("zz", &["src/zz.rs"]),
        ["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs", "src/zz.rs"]
    );
}

/// Eight code lines of `lens` characters each, the fourth with the three words.
fn sized(reps: usize, lens: [usize; 8]) -> Vec<String> {
    let mut lines = trio(reps);
    for (line, len) in lines.iter_mut().zip(lens) {
        *line = format!("{line:<len$}");
        assert_eq!(line.chars().count(), len, "{line}");
    }
    lines
}

#[test]
fn the_windows_stop_at_five_thousand_three_hundred_bytes_and_a_smaller_one_still_fits() {
    // 8 lines of 192 characters cost 8 * (192 + 8) = 1 600 bytes.
    let big = |reps| sized(reps, [192; 8]);
    // 7 lines of 40 and one of 156 cost 7 * 48 + 164 = 500 bytes.
    let small = |extra: usize| sized(1, [40, 40, 40, 156 + extra, 40, 40, 40, 40]);
    let shown = |small_lines: Vec<String>| {
        let files = vec![
            ("src/b1.rs".to_string(), big(5)),
            ("src/b2.rs".to_string(), big(4)),
            ("src/b3.rs".to_string(), big(3)),
            ("src/b4.rs".to_string(), big(2)),
            ("src/s.rs".to_string(), small_lines),
        ];
        paths(&ask(&repo_of(&with_fillers(files, 8)), SHORT))
    };
    assert_eq!(
        shown(small(0)),
        ["src/b1.rs", "src/b2.rs", "src/b3.rs", "src/s.rs"]
    );
    assert_eq!(shown(small(1)), ["src/b1.rs", "src/b2.rs", "src/b3.rs"]);
}

#[test]
fn a_window_made_mostly_of_test_code_is_left_out() {
    let mixed = |code: usize| -> Vec<String> {
        let mut lines = pad(code);
        lines[1] = "    f(); // stop print match".to_string();
        lines.push("#[test]".to_string());
        lines.push("fn t() {".to_string());
        lines.extend(vec!["    g(); // stop print match".to_string(); 8 - code]);
        lines.push("}".to_string());
        lines
    };
    let shown = |lines: Vec<String>| {
        let files = with_fillers(vec![("src/mixed.rs".to_string(), lines)], 4);
        paths(&ask(&repo_of(&files), SHORT))
    };
    // Ten lines, the test unit the last five or the last four.
    assert_eq!(shown(mixed(5)), Vec::<String>::new());
    assert_eq!(shown(mixed(6)), ["src/mixed.rs"]);
}

#[test]
fn a_test_file_and_a_document_are_no_source_window() {
    let mut files: Vec<(&str, &[&str])> = vec![
        ("src/hiargs.rs", HIARGS),
        ("src/summary.rs", SUMMARY),
        ("src/line_buffer.rs", BUFFER),
        ("tests/binary.rs", BINARY_TESTS),
        ("docs/guide.md", HIARGS),
    ];
    files.extend(FILLERS);
    assert_eq!(
        paths(&ask(&repo(&files), WHY_NO_TESTS)),
        ["src/hiargs.rs", "src/line_buffer.rs", "src/summary.rs"]
    );
}

#[test]
fn the_step_needs_four_source_files_three_groups_and_files_it_can_read() {
    let sized_up = |fillers_n: usize, filler_len: usize, typed: &str| {
        let mut files = vec![("src/t.rs".to_string(), trio(1))];
        files.extend(fillers(fillers_n.saturating_sub(1)));
        if filler_len > 0 {
            files.push(("src/huge.rs".to_string(), vec!["x".repeat(filler_len)]));
        }
        ask(&repo_of(&files), typed)
    };
    assert_eq!(sized_up(2, 0, SHORT), Vec::<String>::new(), "two files");
    assert_eq!(sized_up(3, 0, SHORT), Vec::<String>::new(), "three files");
    assert_eq!(sized_up(4, 0, SHORT).len(), 1, "four files");
    assert_eq!(
        sized_up(4, 0, "Why does it stop printing?"),
        Vec::<String>::new(),
        "two groups"
    );
    assert_eq!(
        sized_up(3, 299_999, SHORT).len(),
        1,
        "a file at the size cap"
    );
    assert_eq!(
        sized_up(3, 300_000, SHORT),
        Vec::<String>::new(),
        "a file over it is not read"
    );
}

#[test]
fn only_the_first_four_thousand_source_files_are_read() {
    let files = |fillers_n: usize| -> Vec<(String, Vec<String>)> {
        let mut files: Vec<(String, Vec<String>)> = (0..fillers_n)
            .map(|i| (format!("src/f{i:04}.rs"), strings(&["pub fn f() {}"])))
            .collect();
        files.push(("src/z.rs".to_string(), trio(1)));
        files
    };
    assert_eq!(paths(&ask(&repo_of(&files(3_999)), SHORT)), ["src/z.rs"]);
    assert_eq!(ask(&repo_of(&files(4_000)), SHORT), Vec::<String>::new());
}

/// A five-line test unit whose comment carries `words`, padded with spaces so
/// that the unit costs `cost` bytes when `cost` is given.
fn unit_lines(name: &str, words: &str, cost: Option<usize>) -> Vec<String> {
    let mut lines = strings(&["#[test]"]);
    lines.push(format!("fn {name}() {{"));
    lines.push(format!("    // {words}"));
    lines.extend(strings(&["    run();", "}"]));
    if let Some(cost) = cost {
        let have: usize = lines.iter().map(|line| line.chars().count() + 8).sum();
        let comment = &mut lines[2];
        let wanted = comment.chars().count() + cost - have;
        *comment = format!("{comment:<wanted$}");
    }
    lines
}

const ALL_WORDS: &str = "stop print match binary";

fn tests_repo(files: &[(&str, Vec<String>)]) -> Repo {
    let owned: Vec<(String, Vec<String>)> = files
        .iter()
        .map(|(path, lines)| ((*path).to_string(), lines.clone()))
        .collect();
    repo_of(&with_fillers(owned, 8))
}

fn joined(units: &[Vec<String>]) -> Vec<String> {
    units.iter().flatten().cloned().collect()
}

#[test]
fn a_file_gives_one_unit_and_a_file_named_after_a_group_word_gives_two() {
    let units = joined(&[
        unit_lines("alpha_one", ALL_WORDS, None),
        unit_lines("beta_two", "stop print", None),
    ]);
    let repo = tests_repo(&[("tests/cli.rs", units.clone()), ("tests/stop.rs", units)]);
    assert_eq!(
        path_heads(&ask(&repo, WHY)),
        [
            "tests/cli.rs:1-5",
            "tests/stop.rs:1-5",
            "tests/stop.rs:6-10"
        ]
    );
}

#[test]
fn a_unit_comes_with_the_siblings_of_its_family_after_it() {
    let units = joined(&[
        unit_lines("case_alpha_1", ALL_WORDS, None),
        unit_lines("case_alpha_2", "none", None),
        unit_lines("case_alpha_3", "none", None),
        unit_lines("case_alpha_4", "none", None),
        unit_lines("case_other_1", "none", None),
    ]);
    let shown = ask(&tests_repo(&[("tests/cli.rs", units)]), WHY);
    assert_eq!(
        path_heads(&shown),
        [
            "tests/cli.rs:1-5",
            "tests/cli.rs:6-10",
            "tests/cli.rs:11-15"
        ]
    );
}

#[test]
fn a_name_with_fewer_than_three_words_is_its_own_family() {
    let units = joined(&[
        unit_lines("caseone_1", ALL_WORDS, None),
        unit_lines("caseone_2", "none", None),
    ]);
    let shown = ask(&tests_repo(&[("tests/cli.rs", units)]), WHY);
    assert_eq!(path_heads(&shown), ["tests/cli.rs:1-5"]);
}

#[test]
fn a_family_takes_one_slot_of_a_dedicated_file_and_leaves_the_other_to_another_family() {
    let units = joined(&[
        unit_lines("case_x_1", ALL_WORDS, None),
        unit_lines("case_x_2", "stop print", None),
        unit_lines("other_y", "stop print", None),
    ]);
    let shown = ask(&tests_repo(&[("tests/stop.rs", units)]), WHY);
    assert_eq!(
        path_heads(&shown),
        [
            "tests/stop.rs:1-5",
            "tests/stop.rs:6-10",
            "tests/stop.rs:11-15"
        ]
    );
}

#[test]
fn a_unit_is_cut_at_twelve_lines() {
    let mut unit = unit_lines("long_case", ALL_WORDS, None);
    unit.splice(3..3, vec!["    g();".to_string(); 15]);
    let shown = ask(&tests_repo(&[("tests/cli.rs", unit)]), WHY);
    assert_eq!(path_heads(&shown), ["tests/cli.rs:1-12"]);
    assert_eq!(shown[0].lines().count(), 13, "the head and twelve lines");
}

#[test]
fn a_unit_must_hold_two_groups() {
    let shown = |words: &str| {
        let mut lines = strings(&["// match binary", ""]);
        lines.extend(unit_lines("lone_case", words, None));
        ask(&tests_repo(&[("tests/cli.rs", lines)]), WHY)
    };
    assert_eq!(shown("stop"), Vec::<String>::new());
    assert_eq!(path_heads(&shown("stop print")), ["tests/cli.rs:3-7"]);
}

#[test]
fn only_the_three_best_test_files_are_shown() {
    let strong = |reps: usize| unit_lines("one_case", &vec![ALL_WORDS; reps].join(" "), None);
    let repo = tests_repo(&[
        ("tests/a.rs", unit_lines("one_case", "stop print", None)),
        ("tests/b.rs", strong(1)),
        ("tests/c.rs", strong(2)),
        ("tests/d.rs", strong(3)),
    ]);
    assert_eq!(
        paths(&ask(&repo, WHY)),
        ["tests/b.rs", "tests/c.rs", "tests/d.rs"]
    );
}

#[test]
fn the_units_stop_at_three_thousand_bytes_and_a_smaller_one_still_fits() {
    let shown = |last: usize| {
        let big = |name: &str, reps: usize, cost: usize| {
            unit_lines(name, &vec![ALL_WORDS; reps].join(" "), Some(cost))
        };
        let repo = tests_repo(&[
            ("tests/a_stop.rs", big("first_one", 4, 1_400)),
            ("tests/b_stop.rs", big("second_one", 3, 1_400)),
            (
                "tests/c_stop.rs",
                joined(&[big("third_one", 2, 1_500), big("fourth_one", 1, last)]),
            ),
        ]);
        path_heads(&ask(&repo, WHY))
    };
    assert_eq!(
        shown(200),
        [
            "tests/a_stop.rs:1-5",
            "tests/b_stop.rs:1-5",
            "tests/c_stop.rs:6-10"
        ]
    );
    assert_eq!(shown(201), ["tests/a_stop.rs:1-5", "tests/b_stop.rs:1-5"]);
}

#[test]
fn a_unit_of_a_source_file_counts_and_is_headed_as_code() {
    let shown = ask(
        &tests_repo(&[("src/inline.rs", unit_lines("inline_case", ALL_WORDS, None))]),
        WHY,
    );
    assert_eq!(
        heads(&shown),
        ["### src/inline.rs:1-5 — where the question's words meet (code)"]
    );
}

/// A repository whose graph answers.
struct Graph {
    repo: Repo,
    symbols: Vec<SymbolHit>,
    callers: BTreeMap<String, Vec<CallerHit>>,
}

impl Evidence for Graph {
    fn files_with(&self, anchor: &str, deadline: Instant) -> Result<Found, String> {
        self.repo.files_with(anchor, deadline)
    }
    fn concept(&self, phrase: &str, deadline: Instant) -> Result<Found, String> {
        self.repo.concept(phrase, deadline)
    }
    fn symbols(&self, name: &str, _deadline: Instant) -> Result<Vec<SymbolHit>, String> {
        Ok(self
            .symbols
            .iter()
            .filter(|symbol| symbol.name == name)
            .cloned()
            .collect())
    }
    fn callers(&self, target: &str, _deadline: Instant) -> Result<Vec<CallerHit>, String> {
        Ok(self.callers.get(target).cloned().unwrap_or_default())
    }
    fn tracked_files(&self, deadline: Instant) -> Result<Vec<String>, String> {
        self.repo.tracked_files(deadline)
    }
    fn read_file(&self, path: &str, max: u64, deadline: Instant) -> Result<String, String> {
        self.repo.read_file(path, max, deadline)
    }
}

fn symbol(path: &str, name: &str, start: u64, end: u64) -> SymbolHit {
    SymbolHit {
        uid: format!("{path}#{name}"),
        name: name.to_string(),
        kind: "function".to_string(),
        path: path.to_string(),
        start_line: start,
        end_line: end,
    }
}

fn call(path: &str, via: &str, line: u64) -> CallerHit {
    CallerHit {
        path: path.to_string(),
        via: via.to_string(),
        line,
    }
}

/// `src/args.rs` declares `name` over `span` lines past its first, with the
/// question's words in its second; `src/main.rs` has 60 lines.
fn args_graph(
    name: &str,
    span: usize,
    symbols: Vec<SymbolHit>,
    callers: Vec<CallerHit>,
    main_lines: usize,
) -> Graph {
    let mut args = vec![
        format!("pub fn {name}() -> bool {{"),
        "    // stop printing matches".to_string(),
    ];
    args.extend(pad(span - 2));
    args.push("}".to_string());
    let files = vec![
        ("src/args.rs".to_string(), args),
        ("src/main.rs".to_string(), pad(main_lines)),
        ("src/z.rs".to_string(), pad(60)),
    ];
    Graph {
        repo: repo_of(&with_fillers(files, 4)),
        symbols,
        callers: BTreeMap::from([(format!("src/args.rs#{name}"), callers)]),
    }
}

fn caller_graph(callers: Vec<CallerHit>, main_lines: usize) -> Graph {
    args_graph(
        "quit_one",
        12,
        vec![
            symbol("src/args.rs", "quit_one", 1, 13),
            symbol("src/main.rs", "run_one", 30, 60),
        ],
        callers,
        main_lines,
    )
}

fn shown_by(graph: &Graph) -> Vec<String> {
    ask(graph, SHORT)
}

#[test]
fn the_first_caller_of_a_small_function_that_carries_a_group_word_is_shown_with_its_call() {
    let graph = caller_graph(
        vec![
            call("src/z.rs", "run_z", 40),
            call("src/main.rs", "run_one", 40),
            call("src/main.rs", "run_one", 35),
        ],
        60,
    );
    let shown = shown_by(&graph);
    assert_eq!(
        path_heads(&shown),
        ["src/args.rs:1-13", "src/main.rs:30-47"]
    );
    let lines: Vec<String> = (30..=47)
        .map(|n| format!("{}| {}", n, pad(60)[n - 1]))
        .collect();
    assert_eq!(
        shown[1],
        block(
            "src/main.rs:30-47 — where the question's words meet (code)",
            &lines
        )
    );
}

#[test]
fn a_caller_is_cut_to_fourteen_lines_before_the_call_and_twelve_after_within_its_span() {
    let at = |site: u64| {
        let graph = caller_graph(vec![call("src/main.rs", "run_one", site)], 60);
        path_heads(&shown_by(&graph))[1].clone()
    };
    assert_eq!(at(50), "src/main.rs:36-60");
    assert_eq!(at(45), "src/main.rs:31-57");
    assert_eq!(at(30), "src/main.rs:30-42");
}

#[test]
fn a_caller_the_graph_cannot_place_is_shown_around_its_call_within_the_file() {
    let shown = |site: u64, lines: usize| {
        let graph = caller_graph(vec![call("src/main.rs", "unknown", site)], lines);
        path_heads(&shown_by(&graph))
    };
    assert_eq!(shown(20, 25), ["src/args.rs:1-13", "src/main.rs:6-25"]);
    assert_eq!(shown(3, 25), ["src/args.rs:1-13", "src/main.rs:1-15"]);
    assert_eq!(
        shown(100, 25),
        ["src/args.rs:1-13"],
        "past the end of the file"
    );
}

#[test]
fn a_caller_outside_the_source_files_or_no_caller_adds_nothing() {
    let docs = caller_graph(vec![call("docs/guide.md", "run_one", 40)], 60);
    assert_eq!(path_heads(&shown_by(&docs)), ["src/args.rs:1-13"]);
    let none = caller_graph(Vec::new(), 60);
    assert_eq!(path_heads(&shown_by(&none)), ["src/args.rs:1-13"]);
}

#[test]
fn a_function_is_small_up_to_twelve_lines_past_its_first_and_must_be_the_graphs_declaration() {
    let call_main = vec![call("src/main.rs", "run_one", 40)];
    let heads_for = |span: usize, symbols: Vec<SymbolHit>| {
        let end = span as u64 + 1;
        let symbols = symbols
            .into_iter()
            .map(|mut s| {
                if s.name == "quit_one" {
                    s.end_line = end;
                }
                s
            })
            .collect();
        path_heads(&shown_by(&args_graph(
            "quit_one",
            span,
            symbols,
            call_main.clone(),
            60,
        )))
    };
    let base = || {
        vec![
            symbol("src/args.rs", "quit_one", 1, 13),
            symbol("src/main.rs", "run_one", 30, 60),
        ]
    };
    assert_eq!(
        heads_for(12, base()),
        ["src/args.rs:1-13", "src/main.rs:30-52"]
    );
    assert_eq!(heads_for(13, base()), ["src/args.rs:1-14"]);
    let moved = vec![
        symbol("src/args.rs", "quit_one", 2, 13),
        symbol("src/main.rs", "run_one", 30, 60),
    ];
    assert_eq!(heads_for(12, moved), ["src/args.rs:1-13"]);
    let elsewhere = vec![
        symbol("src/z.rs", "quit_one", 1, 13),
        symbol("src/main.rs", "run_one", 30, 60),
    ];
    assert_eq!(heads_for(12, elsewhere), ["src/args.rs:1-13"]);
}

#[test]
fn a_function_whose_name_carries_no_group_word_is_not_looked_up() {
    let graph = args_graph(
        "plain_one",
        12,
        vec![
            symbol("src/args.rs", "plain_one", 1, 13),
            symbol("src/main.rs", "run_one", 30, 60),
        ],
        vec![call("src/main.rs", "run_one", 40)],
        60,
    );
    assert_eq!(path_heads(&shown_by(&graph)), ["src/args.rs:1-13"]);
}

#[test]
fn a_unit_is_looked_for_its_closing_brace_over_four_hundred_lines() {
    let unit_of = |closing_at: usize| {
        let mut lines = vec!["fn test_far() {".to_string()];
        lines.extend(vec!["    body();".to_string(); closing_at - 2]);
        lines.push("}".to_string());
        units("src/lib.rs", &lines)
    };
    assert_eq!(unit_of(400), vec![unit(1, 400, "test_far")]);
    assert_eq!(unit_of(401), vec![unit(1, 400, "test_far")]);
    assert_eq!(unit_of(10), vec![unit(1, 10, "test_far")]);
}

#[test]
fn a_file_named_after_a_group_word_outranks_a_better_unit_of_another_file() {
    let strong = unit_lines("one_case", &format!("{ALL_WORDS} {ALL_WORDS}"), None);
    let weaker = unit_lines("one_case", ALL_WORDS, None);
    let repo = tests_repo(&[
        ("tests/x1.rs", strong.clone()),
        ("tests/x2.rs", strong.clone()),
        ("tests/x3.rs", strong),
        ("tests/stop.rs", weaker),
    ]);
    assert_eq!(
        paths(&ask(&repo, WHY)),
        ["tests/stop.rs", "tests/x1.rs", "tests/x2.rs"]
    );
}
