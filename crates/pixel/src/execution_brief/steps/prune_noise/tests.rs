// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

use std::path::Path;
use std::time::{Duration, Instant};

use super::super::testing::{applied, headers, outcomes_of, repo};
use super::*;

/// What the step prints for a `kind` question `prompt` and `brief` (the kind
/// of the question is `None` when it was not settled).
fn ask_kind(kind: Option<&str>, prompt: &str, brief: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    (STEP.run)(&Ctx::new(Path::new("."), prompt, brief, deadline).with_kind(kind))
}

fn ask(prompt: &str, brief: &str) -> String {
    ask_kind(None, prompt, brief)
}

/// `count` numbered lines from `from`, `{n}| {text}`.
fn lines(from: usize, count: usize, text: &str) -> String {
    (from..from + count)
        .map(|n| format!("{n}| {text}\n"))
        .collect()
}

/// A usage block of `count` lines of `width` characters each, `tag` at the
/// start of each, so that its size is about `count * (width + 6)` bytes.
fn usage(path: &str, tag: &str, count: usize, width: usize) -> String {
    let text = format!("{tag} {}", "x".repeat(width - tag.len() - 1));
    format!("### {path}:1-{count} — {tag} (usage)\n{}", lines(1, count, &text))
}

const DEFINITION: &str = "### src/a.rs:1-2 — start (function definition)\n1| fn start() {\n2| }\n";
const DOC: &str = "### README.md:1-2 — Install (documentation section)\n1| install it\n2| run it\n";
const COMPLETION: &str = "### completions/_tool.zsh:1-2 — complete (function definition)\n1| _tool() {\n2| }\n";

#[test]
fn a_brief_without_a_block_or_with_nothing_to_shrink_prints_nothing() {
    assert_eq!(ask("what does start do", "[PIXEL:BRIEF]\nno blocks here\n"), "");
    assert_eq!(ask("what does start do", ""), "");
    let brief = format!("[PIXEL:BRIEF]\n{DEFINITION}");
    assert_eq!(ask("what does start do", &brief), "");
}

#[test]
fn a_documentation_section_goes_unless_the_question_asks_for_documentation_and_shares_a_word() {
    let brief = format!("[PIXEL:BRIEF]\n{DOC}{DEFINITION}");
    assert_eq!(
        ask("what does install do", &brief),
        format!("drop: .\n{DEFINITION}")
    );
    assert_eq!(
        ask("overview of the install", &brief),
        "",
        "asked for an overview, and it holds a word of the question: nothing shrank"
    );
    assert_eq!(
        ask("docs of start", &brief),
        format!("drop: .\n{DEFINITION}"),
        "docs were asked for, but the section shares no word with the question"
    );
}

#[test]
fn the_kind_of_the_question_lets_a_documentation_section_stay() {
    let brief = format!("[PIXEL:BRIEF]\n{DOC}{COMPLETION}{DEFINITION}");
    let expected = format!("drop: .\n{DOC}{DEFINITION}");
    assert_eq!(ask_kind(Some("overview"), "install it", &brief), expected);
    assert_eq!(
        ask_kind(Some("lookup"), "install it", &brief),
        format!("drop: .\n{DEFINITION}")
    );
    assert_eq!(
        ask_kind(None, "install it", &brief),
        format!("drop: .\n{DEFINITION}")
    );
}

#[test]
fn a_test_name_list_goes_only_when_the_brief_shows_a_test_body() {
    let list = "### tests matching start\n- test_start\n- test_stop\n";
    let test = "### tests/t.rs:1-2 — test_start (test)\n1| fn test_start() {\n2| }\n";
    let without = format!("[PIXEL:BRIEF]\n{list}{DEFINITION}");
    assert_eq!(ask("tests of start", &without), "", "kept: no test body");
    let with = format!("[PIXEL:BRIEF]\n{list}{test}{DEFINITION}");
    assert_eq!(
        ask("tests of start", &with),
        format!("drop: .\n{test}{DEFINITION}")
    );
    let of = "### tests of start (3)\n- test_a\n";
    let with = format!("[PIXEL:BRIEF]\n{of}{test}");
    assert_eq!(ask("tests of start", &with), format!("drop: .\n{test}"));
}

#[test]
fn a_config_registry_keeps_the_rows_that_name_a_reader_and_a_key_of_the_question() {
    let registry = "### config registry\n- MAX_RETRIES | int | read at src/a.rs:3\n- OTHER_KEY | int | read at src/b.rs:1\n- QUIET_KEY | int | not read\n";
    let cited = "### src/a.rs:3-3\n3| use(MAX_RETRIES)\n";
    let uncited = "### src/c.rs:1-1\n1| use(OTHER)\n";
    let brief = format!("[PIXEL:BRIEF]\n{registry}{cited}{uncited}{DEFINITION}");
    assert_eq!(
        ask("what reads MAX_RETRIES", &brief),
        format!(
            "drop: .\n### config registry\n- MAX_RETRIES | int | read at src/a.rs:3\n### src/a.rs:3\n3| use(MAX_RETRIES)\n{DEFINITION}"
        ),
        "one row stays, the slice it cites stays, the other slice goes"
    );
    assert_eq!(
        ask("what reads NOTHING_HERE", &brief),
        format!("drop: .\n{DEFINITION}"),
        "no row is named: the registry goes, and the slices after it with it, none being cited"
    );
}

#[test]
fn a_bare_slice_before_a_registry_is_not_dropped_for_it() {
    let registry = "### config registry\n- MAX_RETRIES | int | read at src/a.rs:3\n";
    let bare = "### src/c.rs:1-1\n1| use(OTHER)\n";
    let brief = format!("[PIXEL:BRIEF]\n{bare}{registry}{DEFINITION}");
    assert_eq!(
        ask("what reads MAX_RETRIES", &brief),
        format!(
            "drop: .\n### src/c.rs:1\n1| use(OTHER)\n{registry}{DEFINITION}"
        ),
        "only the slices after a registry are tied to it; this one keeps its place (a one-line span is one number)"
    );
}

#[test]
fn a_shell_completion_file_goes_unless_the_question_names_completions() {
    let brief = format!("[PIXEL:BRIEF]\n{COMPLETION}{DEFINITION}");
    assert_eq!(ask("what does start do", &brief), format!("drop: .\n{DEFINITION}"));
    assert_eq!(ask("what does the completion do", &brief), "");
    assert_eq!(ask("which shell runs it", &brief), "");
    let fish = "### share/tool.fish:1-1 — x (function definition)\n1| function x\n";
    let brief = format!("[PIXEL:BRIEF]\n{fish}{DEFINITION}");
    assert_eq!(ask("what does start do", &brief), format!("drop: .\n{DEFINITION}"));
}

#[test]
fn overlapping_ranges_of_one_file_become_one_block() {
    let first = "### src/a.rs:1-3 — start (function definition)\n1| one\n2| two\n3| three\n";
    let second = "### src/a.rs:3-5 — start (function usage)\n3| three\n4| four\n5| five\n";
    let apart = "### src/a.rs:9-9 — later (function usage)\n9| nine\n";
    let brief = format!("[PIXEL:BRIEF]\n{first}{second}{apart}");
    assert_eq!(
        ask("what does start do", &brief),
        "drop: .\n### src/a.rs:1-5 — start (function definition)\n1| one\n2| two\n3| three\n4| four\n5| five\n### src/a.rs:9 — later (function usage)\n9| nine\n",
        "the first block takes the second's lines (a one-line span is one number)"
    );
    let other = "### src/b.rs:3-4 — start (function usage)\n3| three\n4| four\n";
    let brief = format!("[PIXEL:BRIEF]\n{first}{other}");
    assert_eq!(ask("what does start do", &brief), "", "another file is not merged");
}

#[test]
fn lines_of_a_region_that_another_block_shows_are_removed() {
    let region = format!("### src/a.rs:1-8 — src/a.rs (whole file region)\n{}", lines(1, 8, "r"));
    let shown = "### src/a.rs:2-3 — start (function definition)\n2| r\n3| r\n";
    let brief = format!("[PIXEL:BRIEF]\n{region}{shown}");
    assert_eq!(
        ask("what does start do", &brief),
        format!(
            "drop: .\n### src/a.rs:4-8 — src/a.rs (whole file region)\n{}{shown}",
            lines(4, 5, "r")
        ),
        "line 1 is a run of one and goes with the lines shown"
    );
    let all = "### src/a.rs:1-8 — start (function definition)\n1| r\n2| r\n3| r\n4| r\n5| r\n6| r\n7| r\n8| r\n";
    let brief = format!("[PIXEL:BRIEF]\n{region}{all}");
    assert_eq!(
        ask("what does start do", &brief),
        format!("drop: .\n{all}"),
        "a region shown whole by another block disappears"
    );
}

#[test]
fn the_call_trace_becomes_one_edge_per_line_without_constructors() {
    let trace = "### call trace from main\nentry main — src/a.rs:1\n  calls helper — src/b.rs:5\n    calls Widget — src/c.rs:9\n    calls leaf — src/d.rs:2\n  calls last — src/e.rs:7\n";
    let brief = format!("[PIXEL:BRIEF]\n{trace}{DEFINITION}");
    assert_eq!(
        ask("what does start do", &brief),
        format!(
            "drop: .\n### call trace from main\nmain -> helper src/b.rs:5\nhelper -> leaf src/d.rs:2\nmain -> last src/e.rs:7\n{DEFINITION}"
        )
    );
    let bare = "### call trace from main\nentry main — src/a.rs:1\n  calls Widget — src/c.rs:9\n";
    let brief = format!("[PIXEL:BRIEF]\n{bare}{DEFINITION}");
    assert_eq!(
        ask("what does start do", &brief),
        format!("drop: .\n{DEFINITION}"),
        "a trace with no edge left is dropped"
    );
}

#[test]
fn a_long_block_keeps_the_window_with_the_most_words_of_the_question() {
    let text = "x".repeat(80);
    let mut body = lines(1, 100, &text);
    body = body.replace("50| xxxxxxxx", "50| needle xx");
    let block = format!("### src/big.rs:1-100 — big (function usage)\n{body}");
    let out = ask("where is the needle", &format!("[PIXEL:BRIEF]\n{block}"));
    let kept: Vec<usize> = out
        .lines()
        .filter_map(|line| line.split_once("| ")?.0.parse().ok())
        .collect();
    assert!(kept.contains(&50), "{out}");
    assert!(kept.windows(2).all(|pair| pair[1] == pair[0] + 1), "contiguous: {kept:?}");
    let header = out.lines().nth(1).unwrap();
    assert_eq!(
        header,
        format!("### src/big.rs:{}-{} — big (function usage)", kept[0], kept[kept.len() - 1])
    );
    assert!(out.len() <= 6_100 && out.len() > 5_000, "{}", out.len());
}

#[test]
fn a_test_body_keeps_eighteen_lines_around_its_assertions() {
    let mut body = lines(1, 30, "step();");
    body = body.replace("25| step();", "25| assert_eq!(a, b);");
    let block = format!("### tests/t.rs:1-30 — test_it (test)\n{body}");
    let out = ask("what does test_it check", &format!("[PIXEL:BRIEF]\n{block}"));
    assert_eq!(
        out,
        format!(
            "drop: .\n### tests/t.rs:8-25 — test_it (test)\n{}25| assert_eq!(a, b);\n",
            lines(8, 17, "step();")
        )
    );
}

/// Five usage blocks of about 2.4 KB each: 12 KB, over the 8 KB total and under
/// the 14 KB of a flow question. The question's word is in the last one.
fn five_blocks() -> String {
    let mut brief = String::from("[PIXEL:BRIEF]\n");
    for (index, name) in ["a", "b", "c", "d", "e"].iter().enumerate() {
        let mut block = usage(&format!("src/{name}.rs"), name, 20, 114);
        if index == 4 {
            block = block.replacen(" xx", " needle xx", 1);
        }
        brief.push_str(&block);
    }
    brief
}

#[test]
fn over_the_total_the_least_valuable_blocks_go_first_and_the_later_one_on_a_tie() {
    let brief = five_blocks();
    assert!(brief.len() > 8_000 && brief.len() < 14_000, "{}", brief.len());
    let out = ask("where is the needle", &brief);
    assert_eq!(
        headers(&out),
        [
            "### src/a.rs:1-20 — a (usage)",
            "### src/b.rs:1-20 — b (usage)",
            "### src/e.rs:1-20 — e (usage)",
        ]
    );
    assert!(out.starts_with("drop: .\n"));
}

#[test]
fn a_flow_or_test_question_has_a_larger_total() {
    let brief = five_blocks();
    for (kind, prompt) in [
        (Some("flow"), "where is the needle"),
        (Some("tests"), "where is the needle"),
        (Some("overview"), "where is the needle"),
        (None, "trace the needle"),
        (None, "the tests of the needle"),
    ] {
        assert_eq!(ask_kind(kind, prompt, &brief), "", "{kind:?} {prompt}");
    }
    assert_ne!(ask_kind(Some("lookup"), "where is the needle", &brief), "");
}

#[test]
fn a_block_over_the_block_cap_is_shrunk_before_a_whole_block_is_dropped() {
    let mut brief = String::from("[PIXEL:BRIEF]\n");
    for name in ["a", "b", "c"] {
        brief.push_str(&usage(&format!("src/{name}.rs"), name, 40, 114));
    }
    assert!(brief.len() > 8_000, "{}", brief.len());
    let out = ask("where is the thing", &brief);
    assert_eq!(headers(&out).len(), 3, "{out}");
    assert!(out.len() < brief.len() && out.len() <= 8_000, "{}", out.len());
}

#[test]
fn a_block_with_no_line_left_makes_the_step_print_nothing() {
    let hollow = "### src/z.rs:1-3 — gone (function usage)\n… 3 lines left out\n";
    let brief = format!("[PIXEL:BRIEF]\n{DOC}{hollow}{DEFINITION}");
    assert_eq!(ask("what does start do", &brief), "");
}

#[test]
fn the_step_gives_up_when_its_time_is_up() {
    let brief = five_blocks();
    let late = Instant::now();
    std::thread::sleep(Duration::from_millis(5));
    let ctx = Ctx::new(Path::new("."), "where is the needle", &brief, late);
    assert_eq!((STEP.run)(&ctx), "");
}

#[test]
fn the_step_is_registered_after_the_others_and_takes_the_script_s_place() {
    let dir = repo(&[("src/a.rs", "fn start() {\n}\n")]);
    let doc = "### README.md:1-2 — start (documentation section)\n1| start it\n2| run it\n";
    let completion = "### completions/_start.zsh:1-2 — start (function definition)\n1| _start() {\n2| }\n";
    let brief = format!("[PIXEL:BRIEF]\n{doc}{completion}{DEFINITION}");
    let (merged, runs) = applied(STEP.name, dir.path(), "lookup", "what does start do", &brief);
    assert_eq!(outcomes_of(&runs, "prune-noise"), ["ok"]);
    assert_eq!(
        merged.unwrap(),
        format!("[PIXEL:BRIEF]\n{DEFINITION}"),
        "the documentation and the completion file went; the script did not run"
    );
    let (merged, _) = applied(STEP.name, dir.path(), "overview", "what does start do", &brief);
    let merged = merged.unwrap();
    assert!(merged.contains("README.md:1-2"), "an overview keeps the documentation: {merged}");
    assert!(!merged.contains("_tool.zsh"), "{merged}");
}
