// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The cases of `probes/name-files/tests/test_probe.py`, then the ones its
//! docstring promises, run on the step.

use super::super::testing::{headers, repo, run};
use super::*;

fn ask(files: &[(&str, &str)], prompt: &str, brief: &str) -> String {
    let dir = repo(files);
    run(&STEP, dir.path(), prompt, brief)
}

const EXIT_CODES: [(&str, &str); 4] = [
    ("src/exit_codes.rs", "pub enum ExitCode { Success, Failure }\n"),
    (
        "src/main.rs",
        "mod exit_codes;\nuse crate::exit_codes::ExitCode;\nfn main() { let _ = ExitCode::Success; }\n",
    ),
    (
        "src/worker.rs",
        "use crate::exit_codes::ExitCode;\npub fn run() -> ExitCode { ExitCode::Success }\n",
    ),
    (
        "tests/test_exit_codes.rs",
        "use crate::exit_codes::ExitCode;\n#[test]\nfn checks_exit_code() { let _ = ExitCode::Success; }\n",
    ),
];
const EXIT_PROMPT: &str =
    "if I change the exit codes, which parts of the code use the exit code module";

#[test]
fn the_step_applies_to_a_module_question_even_when_the_gate_is_closed() {
    assert!(STEP.applies(Some("impact"), EXIT_PROMPT, true));
    assert!(STEP.applies(Some("lookup"), "an unrelated prompt", true));
    assert!(STEP.applies(None, "which files depend on it", true));
    assert!(!STEP.applies(None, "an unrelated prompt", true));
    for kind in ["overview", "review", "flow", "history"] {
        assert!(!STEP.applies(Some(kind), EXIT_PROMPT, true), "{kind}");
    }
}

#[test]
fn a_plural_prompt_names_the_module_and_the_source_that_uses_it_but_not_a_test() {
    let out = ask(&EXIT_CODES, EXIT_PROMPT, "");
    assert_eq!(
        out,
        [
            "### src/exit_codes.rs:1-1 — module named by the prompt",
            "1| pub enum ExitCode { Success, Failure }",
            "### src/main.rs:1-1 — source reference to exit_codes",
            "1| mod exit_codes;",
            "### src/worker.rs:1-1 — source reference to exit_codes",
            "1| use crate::exit_codes::ExitCode;",
            "",
        ]
        .join("\n")
    );
}

#[test]
fn a_prompt_without_module_intent_adds_no_module_block() {
    let out = ask(
        &EXIT_CODES,
        "what did the team say about exit code review comments",
        "",
    );
    assert_eq!(out, "");
}

#[test]
fn one_filename_word_is_not_enough_to_name_a_module() {
    assert_eq!(ask(&EXIT_CODES, "what does exit do", ""), "");
}

#[test]
fn a_module_shared_with_no_second_word_or_a_single_word_stem_is_not_named() {
    let files = [
        ("src/exit.rs", "x\n"),
        ("src/exit_code_helper.rs", "h\n"),
        ("src/main.rs", "use crate::exit_code_helper;\n"),
    ];
    let out = ask(&files, "which files use the exit code helper module", "");
    assert_eq!(
        headers(&out),
        [
            "### src/exit_code_helper.rs:1-1 — module named by the prompt",
            "### src/main.rs:1-1 — source reference to exit_code_helper",
        ]
    );
}

#[test]
fn the_best_two_modules_win_by_shared_words_then_fewest_extra_words() {
    let files = [
        ("a/exit_code_extra.rs", "1\n"),
        ("b/exit_code.rs", "2\n"),
        ("c/exit_code_more_stuff.rs", "3\n"),
        ("d/code_exit.rs", "4\n"),
    ];
    let out = ask(&files, "which module sets the exit code", "");
    assert_eq!(
        headers(&out),
        [
            "### b/exit_code.rs:1-1 — module named by the prompt",
            "### d/code_exit.rs:1-1 — module named by the prompt",
        ]
    );
}

#[test]
fn a_reference_is_one_per_file_in_path_order_and_at_most_eight() {
    let mut files: Vec<(String, String)> = (0..10)
        .map(|n| {
            (
                format!("src/m{n}.rs"),
                "use crate::exit_codes;\nuse crate::exit_codes::A;\n".to_owned(),
            )
        })
        .collect();
    files.push(("src/exit_codes.rs".to_owned(), "x\n".to_owned()));
    let borrowed: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let out = ask(&borrowed, EXIT_PROMPT, "");
    let refs: Vec<&str> = headers(&out).into_iter().skip(1).collect();
    assert_eq!(refs.len(), 8);
    assert_eq!(refs[0], "### src/m0.rs:1-1 — source reference to exit_codes");
    assert_eq!(refs[7], "### src/m7.rs:1-1 — source reference to exit_codes");
}

#[test]
fn a_mention_that_is_not_an_import_or_use_is_no_reference() {
    let files = [
        ("src/exit_codes.rs", "x\n"),
        ("src/main.rs", "// the exit_codes are plain\nlet a = exit_codes;\n"),
    ];
    let out = ask(&files, EXIT_PROMPT, "");
    assert_eq!(
        headers(&out),
        ["### src/exit_codes.rs:1-1 — module named by the prompt"]
    );
}

#[test]
fn the_module_blocks_stop_at_three_thousand_characters() {
    let big = "x".repeat(150) + "\n";
    let files = [
        ("src/exit_codes.rs", big.repeat(30).as_str().to_owned()),
        ("src/exit_code_two.rs", big.repeat(30)),
    ];
    let borrowed: Vec<(&str, &str)> = files.iter().map(|(a, b)| (*a, b.as_str())).collect();
    let out = ask(&borrowed, EXIT_PROMPT, "");
    // 24 lines of 153 characters is 3 677: the first block alone is too long,
    // and so is the second.
    assert_eq!(out, "");
}

#[test]
fn a_module_file_over_the_size_cap_or_behind_a_link_is_not_quoted() {
    let big = "x".repeat(250_001);
    let dir = repo(&[
        ("src/exit_codes.rs", big.as_str()),
        ("src/real_target.rs", "t\n"),
    ]);
    std::os::unix::fs::symlink(
        dir.path().join("src/real_target.rs"),
        dir.path().join("src/exit_code_link.rs"),
    )
    .unwrap();
    super::super::testing::git(dir.path(), &["add", "."]);
    assert_eq!(run(&STEP, dir.path(), EXIT_PROMPT, ""), "");
}

#[test]
fn a_compound_of_the_prompt_finds_its_definition_in_either_spelling() {
    let files = [
        ("src/a.rs", "// one\npub struct SmartCase {\n    x: u8,\n}\n"),
        ("src/b.py", "def smart_case(x):\n    return x\n"),
        ("src/c.rs", "struct SmartCaseExtra;\n"),
    ];
    let out = ask(&files, "how does smart case work", "");
    assert_eq!(
        out,
        [
            "### src/a.rs:2 — SmartCase definition (named by the prompt: 'smart case')",
            "1| // one",
            "2| pub struct SmartCase {",
            "3|     x: u8,",
            "4| }",
            "### src/b.py:1 — SmartCase definition (named by the prompt: 'smart case')",
            "1| def smart_case(x):",
            "2|     return x",
            "",
        ]
        .join("\n")
    );
}

#[test]
fn only_the_first_two_definitions_of_a_compound_are_shown() {
    let files = [
        ("a.rs", "struct SmartCase;\n"),
        ("b.rs", "struct SmartCase;\n"),
        ("c.rs", "struct SmartCase;\n"),
    ];
    let out = ask(&files, "smart case", "");
    assert_eq!(
        headers(&out),
        [
            "### a.rs:1 — SmartCase definition (named by the prompt: 'smart case')",
            "### b.rs:1 — SmartCase definition (named by the prompt: 'smart case')",
        ]
    );
}

#[test]
fn a_line_carrying_two_spellings_is_credited_to_the_longer_one() {
    let files = [("a.rs", "fn booking_availability(x: AvailabilitySlots) {}\n")];
    let out = ask(&files, "booking availability slots", "");
    assert_eq!(
        headers(&out),
        ["### a.rs:1 — BookingAvailability definition (named by the prompt: 'booking availability')"]
    );
}

#[test]
fn a_definition_near_the_top_starts_at_the_first_line() {
    let files = [("a.rs", "struct SmartCase;\nfn x() {}\n")];
    let out = ask(&files, "smart case", "");
    assert_eq!(out.lines().nth(1), Some("1| struct SmartCase;"));
}

#[test]
fn a_definition_body_is_sixty_lines_cut_to_a_hundred_and_sixty_characters() {
    let mut text = String::from("struct SmartCase;\n");
    text.push_str(&format!("{}\n", "y".repeat(200)));
    for n in 0..100 {
        text.push_str(&format!("line {n}\n"));
    }
    let out = ask(&[("a.rs", text.as_str())], "smart case", "");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 1 + 60);
    assert_eq!(lines[2], format!("2| {}", "y".repeat(160)));
    assert_eq!(lines[60], "60| line 57");
}

#[test]
fn the_search_skips_other_extensions_hidden_files_and_ignored_files() {
    let files = [
        ("a.txt", "struct SmartCase;\n"),
        (".hidden/b.rs", "struct SmartCase;\n"),
        ("ignored/c.rs", "struct SmartCase;\n"),
        (".gitignore", "ignored/\n"),
        (".pixel/d.rs", "struct SmartCase;\n"),
        ("e.go", "type SmartCase struct{}\n"),
    ];
    let out = ask(&files, "smart case", "");
    assert_eq!(
        headers(&out),
        ["### e.go:1 — SmartCase definition (named by the prompt: 'smart case')"]
    );
}

#[test]
fn a_file_named_by_a_word_of_the_prompt_is_shown_beside_what_the_brief_shows() {
    let files = [
        ("src/model.rs", "pub struct Model;\n"),
        ("src/relay.rs", "pub fn ask_model_to_relay() {}\n"),
        ("elsewhere/model.rs", "other\n"),
    ];
    let brief = "[PIXEL:BRIEF]\n### src/relay.rs:1 — ask_model_to_relay definition (function)\n1| pub fn ask_model_to_relay() {}\n";
    let out = ask(&files, "what breaks if I change the model", brief);
    assert_eq!(
        out,
        "### src/model.rs:1-1 — file named 'model' (a word of the prompt)\n1| pub struct Model;\n"
    );
}

#[test]
fn a_word_inside_an_identifier_names_no_file() {
    let files = [
        ("src/model.rs", "pub struct Model;\n"),
        ("src/relay.rs", "pub fn ask_model_to_relay() {}\n"),
    ];
    let brief = "[PIXEL:BRIEF]\n### src/relay.rs:1 — ask_model_to_relay definition (function)\n";
    let out = ask(&files, "what breaks if I change ask_model_to_relay", brief);
    assert!(!out.contains("src/model.rs"), "{out}");
}

#[test]
fn a_file_far_from_the_brief_or_a_test_file_or_a_long_one_is_not_shown() {
    let long = "l\n".repeat(221);
    let files = [
        ("far/model.rs", "m\n"),
        ("src/relay.rs", "r\n"),
        ("src/model_test.rs", "t\n"),
        ("src/config.rs", long.as_str()),
    ];
    let brief = "### src/relay.rs:1 — x\n";
    assert_eq!(ask(&files, "model", brief), "");
    assert_eq!(ask(&files, "config", brief), "");
}

#[test]
fn a_file_of_exactly_the_longest_size_is_cut_to_a_hundred_and_eighty_lines() {
    let text = "l\n".repeat(220);
    let files = [("src/config.rs", text.as_str()), ("src/other.rs", "o\n")];
    let out = ask(&files, "config", "### src/other.rs:1 — x\n");
    assert_eq!(
        out.lines().next(),
        Some("### src/config.rs:1-180 — file named 'config' (a word of the prompt)")
    );
    assert_eq!(out.lines().count(), 181);
}

#[test]
fn the_closest_file_wins_when_two_are_named() {
    let files = [
        ("a/b/model.rs", "deep\n"),
        ("a/model.rs", "near\n"),
        ("x.rs", "x\n"),
    ];
    let out = ask(&files, "model", "### a/b/other.rs:1 — x\n");
    assert!(out.contains("### a/b/model.rs:1-1"), "{out}");
}

#[test]
fn a_block_of_the_brief_that_shares_no_word_with_the_prompt_is_dropped() {
    let files = [
        ("src/model.rs", "pub struct Model;\n"),
        ("src/relay.rs", "r\n"),
        ("elsewhere/zebra.rs", "z\n"),
    ];
    let brief = "[PIXEL:BRIEF]\n### src/relay.rs:1 — relay (function)\n1| r\n### elsewhere/zebra.rs:1 — zebra (function)\n1| z\n### other/model_note.rs:1 — model.note\n";
    let out = ask(&files, "what breaks if I change the model", brief);
    assert_eq!(
        out,
        "drop: ^elsewhere/zebra\\.rs:1\\ —\\ zebra\\ \\(function\\)$\n\
         ### src/model.rs:1-1 — file named 'model' (a word of the prompt)\n\
         1| pub struct Model;\n"
    );
}

#[test]
fn nothing_found_prints_nothing_even_when_the_brief_has_noise() {
    let out = ask(&[("a.rs", "x\n")], "what is this", "### zebra.rs:1 — z\n");
    assert_eq!(out, "");
}

#[test]
fn words_are_the_stemmed_case_split_parts_longer_than_two_letters() {
    let words = words("HTTPServer parses Files and lines, with an XMLRequest");
    let expected: BTreeSet<String> = ["http", "server", "pars", "xml", "request"]
        .into_iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(words, expected);
}

#[test]
fn identifier_parts_fold_plurals_like_the_script() {
    assert_eq!(
        identifier_parts("Entries boxes ClassES glass codes ab"),
        ["entry", "boxe", "class", "glass", "code"]
    );
}

#[test]
fn common_dirs_counts_the_leading_directories_two_paths_share() {
    assert_eq!(common_dirs("a/b/c.rs", "a/b/d.rs"), 2);
    assert_eq!(common_dirs("a/b/c.rs", "a/x/d.rs"), 1);
    assert_eq!(common_dirs("a.rs", "b.rs"), 0);
    assert_eq!(common_dirs("a/b.rs", "./a/c.rs"), 1);
    assert_eq!(common_dirs("/a/b.rs", "/a/c.rs"), 2);
    assert_eq!(common_dirs("/a/b.rs", "a/c.rs"), 0);
}

#[test]
fn compounds_pair_adjacent_content_words() {
    let pairs = compounds("how does Smart case matching the of ab work");
    let got: Vec<(&str, &str, &str)> = pairs
        .iter()
        .map(|p| (p.camel.as_str(), p.snake.as_str(), p.kebab.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            ("SmartCase", "smart_case", "smart-case"),
            ("CaseMatching", "case_matching", "case-matching"),
        ]
    );
}

#[test]
fn stage_rows_stop_at_the_byte_cap_like_the_script_read() {
    let name = "a".repeat(949);
    let files: Vec<String> = (0..3000).map(|_| name.clone()).collect();
    // Each record is 51 + 949 = 1 000 bytes: 2 000 of them fit.
    assert_eq!(stage_rows(&files).len(), 2000);
    let many: Vec<String> = (0..25_000).map(|n| n.to_string()).collect();
    assert_eq!(stage_rows(&many).len(), 20_000);
}
