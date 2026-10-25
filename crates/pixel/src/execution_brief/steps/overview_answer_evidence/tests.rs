// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The cases of `probes/overview-answer-evidence/tests/test_probe.py` (the
//! library root and the tracked FIFO), and the rest of the script's contract,
//! run on the step.

use std::time::{Duration, Instant};

use super::super::testing::{repo, run};
use super::*;

fn ask(files: &[(&str, &str)], prompt: &str) -> String {
    let dir = repo(files);
    run(&STEP, dir.path(), prompt, "")
}

const APP: &str = "[package]
name = \"acme-app\"
description = \"  The   app  \"

[dependencies]
acme-lib = { path = \"../lib\", optional = true }
serde = \"1\"

[dev-dependencies]
tempfile = \"3\"
";

fn workspace() -> Vec<(&'static str, &'static str)> {
    vec![
        ("AGENTS.md", "# Rules\nBe nice.\n"),
        ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
        ("crates/app/Cargo.toml", APP),
        (
            "crates/app/src/lib.rs",
            "//! The app facade.\n//! Second line.\n\npub fn run() {}\n",
        ),
        ("crates/app/examples/hello.rs", "fn main() {}\n"),
        ("crates/app/README.md", "x"),
        (
            "crates/lib/Cargo.toml",
            "[package]\nname = \"acme-lib\"\ndescription = \"Library\"\n",
        ),
        ("crates/lib/src/lib.rs", "//! Lib docs.\n"),
    ]
}

#[test]
fn the_step_applies_to_a_crate_question_and_not_to_history_impact_or_review() {
    let asked = "what does the app crate expose";
    assert!(STEP.applies(None, asked, true));
    assert!(STEP.applies(Some("lookup"), "the library", false));
    for kind in ["history", "impact", "review"] {
        assert!(!STEP.applies(Some(kind), asked, false), "{kind}");
    }
    assert!(!STEP.applies(None, "what does app expose", true));
}

#[test]
fn a_named_member_gets_its_docs_manifest_files_dependencies_and_examples() {
    let out = ask(&workspace(), "what does the acme-app crate expose");
    assert_eq!(
        out,
        [
            "### Repository instructions",
            "AGENTS.md:",
            "# Rules",
            "Be nice.",
            "### crates/app/src/lib.rs — crate purpose and public surface",
            "The app",
            "The app facade.",
            "Second line.",
            "### crates/app/Cargo.toml — line-numbered package manifest",
            "1| [package]",
            "2| name = \"acme-app\"",
            "3| description = \"  The   app  \"",
            "4| ",
            "5| [dependencies]",
            "6| acme-lib = { path = \"../lib\", optional = true }",
            "7| serde = \"1\"",
            "8| ",
            "9| [dev-dependencies]",
            "10| tempfile = \"3\"",
            "### crates/app/src/lib.rs — line-numbered crate facade",
            "1| //! The app facade.",
            "2| //! Second line.",
            "3| ",
            "4| pub fn run() {}",
            "### Tracked files in crates/app (4 total)",
            "Cargo.toml",
            "README.md",
            "examples/hello.rs",
            "src/lib.rs",
            "dependencies:",
            "- acme-lib (optional) — Library",
            "  crates/lib/src/lib.rs — leading module documentation",
            "  Lib docs.",
            "- serde",
            "development dependencies:",
            "- tempfile",
            "### crates/app/examples/hello.rs — example source",
            "1| fn main() {}",
            "",
        ]
        .join("\n")
    );
}

#[test]
fn the_directory_name_names_the_member_too_and_without_agents_it_says_so() {
    let mut files = workspace();
    files.remove(0);
    let out = ask(&files, "tell me about the app library");
    assert!(
        out.starts_with(
            "### Repository instructions\nNo tracked AGENTS.md instruction files are present.\n### crates/app/src/lib.rs"
        ),
        "{out}"
    );
}

#[test]
fn no_member_two_members_or_a_name_inside_a_longer_one_say_nothing() {
    assert_eq!(ask(&workspace(), "what is the workspace crate"), "");
    assert_eq!(
        ask(&workspace(), "compare the acme-app crate with the acme-lib crate"),
        ""
    );
    assert_eq!(ask(&workspace(), "what does the acme-app-extra crate do"), "");
    assert_eq!(ask(&workspace(), "what does the my_app crate do"), "");
    assert_eq!(ask(&workspace(), "what does the xacme-app crate do"), "");
    // A name beside punctuation, in any case, is a name.
    let out = ask(&workspace(), "(The `ACME-APP`) crate?");
    assert!(out.starts_with("### Repository instructions"), "{out}");
}

#[test]
fn a_member_without_a_library_root_says_nothing() {
    let mut files = workspace();
    files.retain(|(path, _)| *path != "crates/app/src/lib.rs");
    assert_eq!(ask(&files, "what does the acme-app crate expose"), "");
}

#[test]
fn a_package_without_a_name_ends_the_step_like_the_script() {
    let mut files = workspace();
    files.push(("crates/odd/Cargo.toml", "[package]\nversion = \"1\"\n"));
    assert_eq!(ask(&files, "what does the acme-app crate expose"), "");
}

#[test]
fn more_than_4000_tracked_paths_say_nothing() {
    let mut files = workspace();
    let extra: Vec<String> = (0..3_993).map(|n| format!("pad/f{n}")).collect();
    assert_eq!(files.len() + extra.len(), 4_001);
    for path in &extra {
        files.push((path.as_str(), ""));
    }
    assert_eq!(ask(&files, "what does the acme-app crate expose"), "");
    files.pop();
    assert!(ask(&files, "what does the acme-app crate expose").starts_with("### "));
}

#[test]
fn default_library_root_wins_over_private_module_documentation() {
    let files = [
        ("member/Cargo.toml", "[package]\nname = \"member\"\n"),
        ("member/src/lib.rs", "//! The public crate facade.\npub struct Api;\n"),
        (
            "member/src/internal.rs",
            "//! Private implementation details.\npub(crate) struct Internal;\n",
        ),
        ("other/Cargo.toml", "[package]\nname = \"other\"\n"),
    ];
    let out = ask(&files, "the member crate");
    assert!(out.contains("### member/src/lib.rs — crate purpose and public surface\ncrate-level documentation:\nThe public crate facade.\n"), "{out}");
    assert!(!out.contains("Private implementation"), "{out}");
}

#[test]
fn manifest_library_path_overrides_the_default() {
    let files = [
        (
            "member/Cargo.toml",
            "[package]\nname = \"member\"\n\n[lib]\npath = \"src/api.rs\"\n",
        ),
        ("member/src/api.rs", "//! The configured library target.\npub struct Api;\n"),
        ("member/src/lib.rs", "//! This file is not the selected target.\npub struct Private;\n"),
        ("other/Cargo.toml", "[package]\nname = \"other\"\n"),
    ];
    let out = ask(&files, "the member crate");
    assert!(out.contains("### member/src/api.rs — crate purpose and public surface\n"), "{out}");
    assert!(out.contains("The configured library target.\n"), "{out}");
    assert!(!out.contains("not the selected target"), "{out}");
}

#[test]
fn tracked_fifo_is_rejected_without_blocking() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("member")).unwrap();
    let fifo = std::ffi::CString::new(dir.path().join("member/pipe").to_str().unwrap()).unwrap();
    // SAFETY: `fifo` is a valid NUL-terminated path that outlives the call.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let ctx = Ctx::new(dir.path(), "p", "b", Instant::now() + Duration::from_secs(5));
    let tracked = ["member/pipe".to_owned()];
    assert_eq!(read(&ctx, &tracked, "member/pipe"), None);
}

#[test]
fn an_untracked_or_oversized_file_is_not_read() {
    let big = "x".repeat(300_001);
    let dir = repo(&[("a.txt", "ok"), ("big.txt", &big), ("exact.txt", &big[1..])]);
    let ctx = Ctx::new(dir.path(), "p", "b", Instant::now() + Duration::from_secs(5));
    let tracked = ["a.txt".to_owned(), "big.txt".to_owned(), "exact.txt".to_owned()];
    assert_eq!(read(&ctx, &tracked, "a.txt").as_deref(), Some("ok"));
    assert_eq!(read(&ctx, &tracked, "big.txt"), None, "300 001 bytes");
    assert_eq!(read(&ctx, &tracked, "exact.txt").map(|t| t.len()), Some(300_000));
    assert_eq!(read(&ctx, &tracked[..1], "exact.txt"), None, "not in the list");
}

#[test]
fn a_dependency_description_that_is_not_a_string_prints_as_python_would() {
    let mut files = workspace();
    files[6] = (
        "crates/lib/Cargo.toml",
        "[package]\nname = \"acme-lib\"\ndescription.workspace = true\n",
    );
    let out = ask(&files, "what does the acme-app crate expose");
    assert!(out.contains("\n- acme-lib (optional) — {'workspace': True}\n"), "{out}");
}

#[test]
fn leading_docs_reads_line_and_block_documentation_and_stops_at_code() {
    assert_eq!(
        leading_docs("\n//! One\n//!   Two  \nfn x() {}\n//! Three\n", 60),
        ["One", "Two"]
    );
    assert_eq!(
        leading_docs("/*! Head\n * body\n * more */\nfn x() {}\n", 60),
        [" Head", "body", "* more "]
    );
    assert_eq!(leading_docs("fn x() {}\n//! late\n", 60), Vec::<String>::new());
    assert_eq!(leading_docs("//! a\n//! b\n//! c\n", 2), ["a", "b"]);
    let long = format!("//! {}\n", "y".repeat(300));
    let docs = leading_docs(&long, 60);
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].chars().count(), 237);
}

#[test]
fn numbered_lines_are_cut_to_180_characters_and_counted_past_the_limit() {
    let text = format!("{}\nb\nc\n", "a".repeat(200));
    assert_eq!(numbered(&text, 2), [format!("1| {}", "a".repeat(180)), "2| b".to_owned(), "... 1 lines omitted".to_owned()]);
    assert_eq!(numbered(&text, 3).len(), 3);
    assert_eq!(numbered("", 3), Vec::<String>::new());
}

#[test]
fn the_output_stops_at_24_000_characters_and_says_so() {
    let line = |n: usize| "x".repeat(n);
    // Two lines of 11 999 characters and a newline each fill 24 000 exactly.
    let fits = [line(11_999), line(11_999)];
    assert_eq!(bounded(&fits), format!("{}\n{}\n", line(11_999), line(11_999)));
    let over = [line(11_999), line(12_000)];
    assert_eq!(bounded(&over), format!("{}\n{TRUNCATED}\n", line(11_999)));
    // Characters, not bytes, are counted.
    let wide = ["é".repeat(11_999), "é".repeat(11_999)];
    assert_eq!(bounded(&wide).chars().count(), 24_000);
}

#[test]
fn one_line_collapses_whitespace_and_cuts_without_an_ellipsis() {
    assert_eq!(one_line("a \n b", 160), "a b");
    assert_eq!(one_line("abcdef", 3), "abc");
}

#[test]
fn python_text_of_a_manifest_value_is_empty_for_a_falsy_one() {
    let doc: DocumentMut = "s = 'x'\ne = ''\nz = 0\nn = 5\nf = false\nt = true\na = [1, 'b']\nd = {k = \"it's\"}\nm = []\n"
        .parse()
        .unwrap();
    let text = |key: &str| py_text(doc.get(key));
    assert_eq!(text("s"), "x");
    assert_eq!(text("e"), "");
    assert_eq!(text("z"), "");
    assert_eq!(text("n"), "5");
    assert_eq!(text("f"), "");
    assert_eq!(text("t"), "True");
    assert_eq!(text("a"), "[1, 'b']");
    assert_eq!(text("d"), "{'k': \"it's\"}");
    assert_eq!(text("m"), "");
    assert_eq!(text("missing"), "");
}
