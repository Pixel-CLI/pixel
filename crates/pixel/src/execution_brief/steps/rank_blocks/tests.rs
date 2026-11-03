// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

use std::path::Path;

use super::super::testing::{applied, headers, outcomes_of, repo, run};
use super::*;

/// `count` lines `// line N`, but for the `special` ones (`(line, text)`).
fn source(count: usize, special: &[(usize, &str)]) -> String {
    (1..=count)
        .map(|n| {
            special
                .iter()
                .find(|(line, _)| *line == n)
                .map_or_else(|| format!("// line {n}\n"), |(_, text)| format!("{text}\n"))
        })
        .collect()
}

const DECLARATION: &str = "pub const MAX_RETRIES: u32 = 5;";

fn limits() -> tempfile::TempDir {
    repo(&[
        ("src/limits.rs", &source(30, &[(15, DECLARATION)])),
        ("src/client.rs", "fn a() {}\nfn retry() { go(MAX_RETRIES); }\n"),
    ])
}

fn ask(root: &Path, prompt: &str, brief: &str) -> String {
    run(&STEP, root, prompt, brief)
}

const CONSTANT_BLOCK: &str = "### src/limits.rs:15-15 — MAX_RETRIES (constant)\n15| pub const MAX_RETRIES: u32 = 5;\n";
const UNRELATED_BLOCK: &str = "### src/other.rs:1-1 — unrelated thing\n1| fn unrelated() {}\n";

#[test]
fn identifiers_are_cut_into_the_parts_the_script_regex_finds() {
    assert_eq!(
        parts("HTTPServerError2 getX iPhone ABc aBc a_b"),
        ["HTTP", "Server", "Error", "get", "X", "Phone", "A", "Bc", "Bc"]
    );
    assert_eq!(parts("MAX_RETRIES"), ["MAX", "RETRIES"]);
    assert_eq!(parts("HTTPserver"), ["HTT", "Pserver"]);
    assert_eq!(parts("é1 -- 9"), Vec::<&str>::new());
}

#[test]
fn a_word_loses_one_ing_es_or_s_when_it_is_longer_than_four_letters() {
    assert_eq!(stem_ing_es_s("Parsing"), "pars");
    assert_eq!(stem_ing_es_s("Boxes"), "box");
    assert_eq!(stem_ing_es_s("limits"), "limit");
    assert_eq!(stem_ing_es_s("cats"), "cats");
    assert_eq!(stem_ing_es_s("thing"), "th");
    assert_eq!(stem_ing_es_s("class"), "clas");
    assert_eq!(stem_ing_es_s("RETRY"), "retry");
}

#[test]
fn the_words_of_a_text_leave_out_short_parts_and_function_words() {
    let held = words("What is the MaxRetries of an HTTPServer, used by Go?");
    let mut held: Vec<_> = held.into_iter().collect();
    held.sort();
    assert_eq!(held, ["http", "max", "retri", "server"]);
}

#[test]
fn blocks_sharing_no_word_are_dropped_and_the_others_lead_by_score() {
    let dir = repo(&[("a.txt", "x")]);
    let brief = [
        "[PIXEL:BRIEF]",
        "### src/a.rs:1-2 — parse the config file",
        "1| x",
        "### src/b.rs:1-2 — unrelated thing",
        "1| y",
        "### src/c.rs:1-2 — config parser reads the config",
        "1| z",
        "### src/d.rs:1-2 — parse config",
        "1| w",
        "",
    ]
    .join("\n");
    let out = ask(dir.path(), "where does the parser read the config", &brief);
    assert_eq!(
        out,
        [
            "drop: ^src/b\\.rs:1\\-2\\ —\\ unrelated\\ thing$",
            "first: ^src/c\\.rs:1\\-2\\ —\\ config\\ parser\\ reads\\ the\\ config$",
            "first: ^src/a\\.rs:1\\-2\\ —\\ parse\\ the\\ config\\ file$",
            "first: ^src/d\\.rs:1\\-2\\ —\\ parse\\ config$",
            "",
        ]
        .join("\n")
    );
}

#[test]
fn a_brief_with_no_block_sharing_a_word_is_left_alone() {
    let dir = limits();
    assert_eq!(
        ask(dir.path(), "tell me about zebras", &format!("[PIXEL:BRIEF]\n{CONSTANT_BLOCK}{UNRELATED_BLOCK}")),
        ""
    );
    assert_eq!(ask(dir.path(), "what is MAX_RETRIES", "[PIXEL:BRIEF]\n"), "");
    assert_eq!(ask(dir.path(), "what is MAX_RETRIES", ""), "");
}

#[test]
fn a_constant_gets_twelve_lines_of_context_and_where_it_is_used() {
    let dir = limits();
    let brief = format!("[PIXEL:BRIEF]\n{UNRELATED_BLOCK}{CONSTANT_BLOCK}");
    let out = ask(dir.path(), "what is the number of MAX_RETRIES", &brief);
    let context: String = (3..=27)
        .map(|n| match n {
            15 => format!("15| {DECLARATION}\n"),
            n => format!("{n}| // line {n}\n"),
        })
        .collect();
    assert_eq!(
        out,
        format!(
            "drop: ^src/other\\.rs:1\\-1\\ —\\ unrelated\\ thing$\n\
             drop: ^src/limits\\.rs:15\\-15\\ —\\ MAX_RETRIES\\ \\(constant\\)$\n\
             ### src/limits.rs:3-27 — MAX_RETRIES (constant, with context)\n\
             {context}uses of MAX_RETRIES:\n\
             ./src/client.rs:2:fn retry() {{ go(MAX_RETRIES); }}\n"
        )
    );
}

#[test]
fn the_context_stops_at_the_ends_of_the_file() {
    let dir = repo(&[("a.rs", &source(5, &[(1, "const TOP: u8 = 1;"), (5, "const BOTTOM: u8 = 2;")]))]);
    let brief = "### a.rs:1-1 — TOP (constant)\n1| const TOP: u8 = 1;\n### a.rs:5-5 — BOTTOM (constant)\n5| const BOTTOM: u8 = 2;\n";
    let out = ask(dir.path(), "what are TOP and BOTTOM", brief);
    assert!(out.contains("### a.rs:1-5 — TOP (constant, with context)\n1| const TOP: u8 = 1;\n2| // line 2\n"), "{out}");
    assert!(out.contains("### a.rs:1-5 — BOTTOM (constant, with context)\n1| const TOP"), "{out}");
    assert!(!out.contains("uses of"), "a declaration is not a use\n{out}");
}

#[test]
fn only_the_three_best_blocks_get_context_and_a_pixel_prefix_is_not_part_of_the_path() {
    let files: Vec<(String, String)> = (1..=4)
        .map(|n| (format!("c{n}.rs"), format!("const C{n}: u8 = {n};\n")))
        .collect();
    let refs: Vec<(&str, &str)> = files.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    let dir = repo(&refs);
    let brief: String = (1..=4)
        .map(|n| format!("### pixel/c{n}.rs:1-1 — C{n} (constant)\n1| const C{n}: u8 = {n};\n"))
        .collect();
    let out = ask(dir.path(), "what are C1 C2 C3 C4 constant", &brief);
    let kinds: Vec<&str> = out
        .lines()
        .filter(|line| line.starts_with("drop:") || line.starts_with("first:"))
        .map(|line| line.split(' ').next().unwrap())
        .collect();
    assert_eq!(kinds, ["drop:", "drop:", "drop:", "first:"], "{out}");
    assert_eq!(
        headers(&out),
        [
            "### c1.rs:1-1 — C1 (constant, with context)",
            "### c2.rs:1-1 — C2 (constant, with context)",
            "### c3.rs:1-1 — C3 (constant, with context)",
        ]
    );
    assert!(out.ends_with("first: ^pixel/c4\\.rs:1\\-1\\ —\\ C4\\ \\(constant\\)$\n"), "{out}");
}

#[test]
fn a_constant_whose_file_cannot_be_read_keeps_its_block() {
    let dir = repo(&[("a.rs", "x\n")]);
    for header in [
        "missing.rs:1-1 — GONE (constant)",
        "../outside.rs:1-1 — GONE (constant)",
        "/etc/hosts:1-1 — GONE (constant)",
        "a.rs:1-1 — GONE (a constant)",
    ] {
        let out = ask(dir.path(), "what is GONE", &format!("### {header}\n1| x\n"));
        assert_eq!(out, format!("first: ^{}$\n", escape(header)), "{header}");
    }
}

#[test]
fn uses_skip_declarations_tests_hidden_and_binary_files_and_come_in_path_order() {
    let usage = |n: usize| format!("fn f{n}() {{ use_it(LIMIT); }}\n");
    let dir = repo(&[
        ("a.rs", "pub const LIMIT: u8 = 1;\n"),
        ("b/z.rs", &usage(1)),
        ("b.rs", &usage(2)),
        ("tests/t.rs", &usage(3)),
        ("src/tests/t.rs", &usage(4)),
        (".hidden/h.rs", &usage(5)),
        (".dot.rs", &usage(6)),
        ("bin.dat", "LIMIT\0LIMIT\n"),
        ("c.rs", "LIMIT_MAX\nxLIMIT\nLIMITx\nLIMIT as word\n"),
        ("docs/tests", "LIMIT outside a directory\n"),
    ]);
    let brief = "### a.rs:1-1 — LIMIT (constant)\n1| pub const LIMIT: u8 = 1;\n";
    let out = ask(dir.path(), "what is LIMIT", brief);
    let uses: Vec<&str> = out.lines().skip_while(|l| *l != "uses of LIMIT:").skip(1).collect();
    assert_eq!(
        uses,
        [
            "./b/z.rs:1:fn f1() { use_it(LIMIT); }",
            "./b.rs:1:fn f2() { use_it(LIMIT); }",
            "./c.rs:4:LIMIT as word",
            "./docs/tests:1:LIMIT outside a directory",
        ]
    );
}

#[test]
fn three_matching_lines_of_a_file_are_read_and_four_uses_are_shown() {
    let many = "LIMIT\n".repeat(10);
    let dir = repo(&[
        ("a.rs", &format!("const LIMIT: u8 = 1;\n{many}")),
        ("b.rs", &many),
        ("c.rs", &many),
    ]);
    let brief = "### a.rs:1-1 — LIMIT (constant)\n1| const LIMIT: u8 = 1;\n";
    let out = ask(dir.path(), "what is LIMIT", brief);
    let uses: Vec<&str> = out.lines().skip_while(|l| *l != "uses of LIMIT:").skip(1).collect();
    // `a.rs` has the declaration and two more matches among the first three; `b.rs` three.
    assert_eq!(uses, ["./a.rs:2:LIMIT", "./a.rs:3:LIMIT", "./b.rs:1:LIMIT", "./b.rs:2:LIMIT"]);
}

#[test]
fn only_the_first_two_names_of_a_block_are_looked_up_and_each_has_its_own_list() {
    let dir = repo(&[
        ("a.rs", "const ONE: u8 = 1;\nconst TWO: u8 = 2;\nconst THREE: u8 = 3;\n"),
        ("b.rs", "go(ONE);\ngo(TWO);\ngo(THREE);\n"),
    ]);
    let brief = "### a.rs:1-3 — ONE, TWO , THREE (constant)\n1| x\n";
    let out = ask(dir.path(), "what are ONE TWO THREE", brief);
    assert!(out.contains("\nuses of ONE:\n./b.rs:1:go(ONE);\nuses of TWO:\n./b.rs:2:go(TWO);\n"), "{out}");
    assert!(!out.contains("uses of THREE"), "{out}");
    assert!(out.contains("— ONE, TWO , THREE (constant, with context)"), "{out}");
}

#[test]
fn a_long_source_line_and_a_long_use_are_cut() {
    let long = format!("const WIDE: &str = \"{}\";", "w".repeat(300));
    let used = format!("let x = WIDE; // {}", "u".repeat(300));
    let dir = repo(&[("a.rs", &format!("{long}\n")), ("b.rs", &format!("{used}\n"))]);
    let brief = "### a.rs:1-1 — WIDE (constant)\n1| x\n";
    let out = ask(dir.path(), "what is WIDE", brief);
    let shown: Vec<&str> = out.lines().filter(|l| l.starts_with("1| ") || l.starts_with("./b.rs")).collect();
    assert_eq!(shown[0].chars().count(), 3 + 160);
    assert_eq!(shown[1].chars().count(), 180);
}

#[test]
fn files_untracked_or_a_repository_without_files_have_no_uses() {
    let dir = repo(&[("a.rs", "const LIMIT: u8 = 1;\n")]);
    std::fs::write(dir.path().join("loose.rs"), "go(LIMIT);\n").unwrap();
    let brief = "### a.rs:1-1 — LIMIT (constant)\n1| x\n";
    let out = ask(dir.path(), "what is LIMIT", brief);
    assert!(out.contains("### a.rs:1-1 — LIMIT (constant, with context)"), "{out}");
    assert!(!out.contains("uses of"), "{out}");
    let bare = tempfile::tempdir().unwrap();
    std::fs::write(bare.path().join("a.rs"), "const LIMIT: u8 = 1;\n").unwrap();
    assert!(!ask(bare.path(), "what is LIMIT", brief).contains("uses of"));
}

#[test]
fn the_step_skips_overview_review_flow_and_history_questions_only() {
    for kind in ["overview", "review", "flow", "history"] {
        assert!(!STEP.applies(Some(kind), "what is it", false), "{kind}");
    }
    for kind in [Some("lookup"), Some("tests"), None] {
        assert!(STEP.applies(kind, "what is it", false), "{kind:?}");
    }
    assert!(!STEP.applies(Some("lookup"), "what is it", true), "a closed gate");
}

#[test]
fn the_registered_step_takes_the_script_s_place_and_leads_the_brief() {
    let dir = limits();
    let brief = format!("[PIXEL:BRIEF]\n{UNRELATED_BLOCK}### src/client.rs:2-2 — retry the MAX_RETRIES\n2| x\n{CONSTANT_BLOCK}");
    let (merged, runs) = applied(STEP.name, dir.path(), "lookup", "what is the number of MAX_RETRIES", &brief);
    assert_eq!(outcomes_of(&runs, "rank-blocks"), ["ok"]);
    let merged = merged.unwrap();
    assert_eq!(
        headers(&merged),
        [
            // `cand-k-code-sites` applies too and leads with its read site.
            "### src/client.rs:2-2 — reads MAX_RETRIES",
            "### src/limits.rs:3-27 — MAX_RETRIES (constant, with context)",
            "### src/client.rs:2-2 — retry the MAX_RETRIES",
        ]
    );
    assert!(!merged.contains("unrelated"), "{merged}");
}
