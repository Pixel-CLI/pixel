// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `probes/cand-k-member-manifest` has no test file: these are the cases its
//! docstring promises, run on the step.

use super::super::testing::{repo, run};
use super::*;

fn ask(files: &[(&str, &str)], prompt: &str) -> String {
    let dir = repo(files);
    run(&STEP, dir.path(), prompt, "")
}

const GREP: &str = "[package]
name = \"grep-cli\"
version = \"0.1.7\"
authors = [\"A\"]
  homepage = \"https://x\"
edition = \"2021\"
edition.workspace = true
license = \"MIT\"
readme = \"README.md\"
description = \"Utilities\"

[features]
default = [\"x\"]
";

fn crates() -> Vec<(&'static str, &'static str)> {
    vec![
        ("crates/cli/Cargo.toml", GREP),
        ("crates/cli/README.md", "r"),
        ("crates/cli/src/lib.rs", "l"),
        ("crates/cli/tests/it.rs", "t"),
        ("crates/other/Cargo.toml", "[package]\nname = \"other\"\n"),
        ("docs/guide.md", "g"),
    ]
}

#[test]
fn the_step_applies_to_a_content_or_requirement_question_about_a_crate() {
    for prompt in [
        "what does the cli crate contain",
        "which features does the cli crate have",
        "what does the cli module depend on",
        "contains the cli crate anything",
    ] {
        assert!(STEP.applies(None, prompt, true), "{prompt}");
    }
    assert!(!STEP.applies(None, "where is the cli crate", true));
    for kind in ["history", "impact", "flow"] {
        assert!(!STEP.applies(Some(kind), "what does the cli crate contain", false), "{kind}");
    }
}

#[test]
fn the_named_member_gets_its_manifest_without_identity_lines_and_its_files() {
    let out = ask(&crates(), "what does the cli crate contain");
    assert_eq!(
        out,
        [
            "drop: ^(?!crates/cli[/ ])",
            "### crates/cli/Cargo.toml:1-13 — manifest of cli (identity lines cut)",
            "1| [package]",
            "2| name = \"grep-cli\"",
            "3| version = \"0.1.7\"",
            "10| description = \"Utilities\"",
            "11| ",
            "12| [features]",
            "13| default = [\"x\"]",
            "### crates/cli/ — 4 tracked files",
            "Cargo.toml, README.md, src/lib.rs, tests/it.rs",
            "",
        ]
        .join("\n")
    );
}

#[test]
fn a_prompt_naming_no_member_or_a_word_that_names_nothing_says_nothing() {
    assert_eq!(ask(&crates(), "what does the missing crate contain"), "");
    assert_eq!(ask(&crates(), "what does the crate contain"), "");
    for filler in ["this", "that", "each", "which", "other", "a"] {
        let prompt = format!("what does {filler} crate contain");
        let files = [(format!("{filler}/Cargo.toml"), "[package]\nname = \"x\"\n")];
        let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), *b)).collect();
        // `other` is a member directory here: the filler still names nothing.
        assert_eq!(ask(&refs, &prompt), "", "{filler}");
    }
    // A manifest at the repository root belongs to no member directory.
    assert_eq!(ask(&[("Cargo.toml", "[package]\nname=\"cli\"\n")], "the cli crate"), "");
}

#[test]
fn a_later_name_is_tried_when_the_first_names_no_member() {
    let out = ask(&crates(), "does the ghost crate or the cli crate depend on it");
    assert!(out.contains("manifest of cli (identity lines cut)"), "{out}");
}

#[test]
fn the_directory_name_matches_without_case_and_every_manifest_kind_counts() {
    let out = ask(&[("pkgs/Web/package.json", "{\n  \"name\": \"web\",\n  \"author\": \"x\"\n}\n")], "the web package depends on?");
    assert_eq!(
        out,
        "drop: ^(?!pkgs/Web[/ ])\n### pkgs/Web/package.json:1-4 — manifest of web (identity lines cut)\n1| {\n2|   \"name\": \"web\",\n3|   \"author\": \"x\"\n4| }\n### pkgs/Web/ — 1 tracked files\npackage.json\n"
    );
    for (path, text) in [
        ("m/pyproject.toml", "[project]\nname = \"m\"\n"),
        ("m/go.mod", "module m\n"),
    ] {
        assert!(ask(&[(path, text)], "the m module contains?").contains(path), "{path}");
    }
}

#[test]
fn only_seventy_manifest_lines_and_twenty_four_files_are_shown() {
    let long: String = (1..=100).map(|n| format!("k{n} = {n}\n")).collect();
    let mut files: Vec<(String, String)> = vec![("m/Cargo.toml".into(), long)];
    for n in 0..30 {
        files.push((format!("m/f{n:02}"), String::new()));
    }
    let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let out = ask(&refs, "the m crate contains?");
    assert!(out.contains("### m/Cargo.toml:1-100 — manifest of m"), "{out}");
    assert!(out.contains("\n70| k70 = 70\n### m/ — 31 tracked files\n"), "{out}");
    assert!(!out.contains("71|"), "{out}");
    assert!(out.ends_with("f22, +7 more\n"), "{out}");
    // Cut lines do not count against the seventy.
    let noisy: String = "license = \"MIT\"\n".repeat(10) + &"k = 1\n".repeat(70);
    let out = ask(&[("m/Cargo.toml", &noisy)], "the m crate contains?");
    assert!(out.contains("\n80| k = 1\n### m/"), "{out}");
}

#[test]
fn a_long_line_is_cut_to_140_characters_and_line_ends_are_python_s() {
    let text = format!("{}\r\nb\x0cc\r", "é".repeat(150));
    let out = ask(&[("m/Cargo.toml", &text)], "the m crate contains?");
    assert!(out.contains(&format!("\n1| {}\n2| b\n3| c\n", "é".repeat(140))), "{out}");
    assert!(out.contains(":1-3 — manifest of m"), "{out}");
}

#[test]
fn an_empty_manifest_leaves_an_empty_line_where_its_lines_would_be() {
    let out = ask(&[("m/Cargo.toml", "")], "the m crate contains?");
    assert_eq!(
        out,
        "drop: ^(?!m[/ ])\n### m/Cargo.toml:1-0 — manifest of m (identity lines cut)\n\n### m/ — 1 tracked files\nCargo.toml\n"
    );
}

#[test]
fn a_manifest_that_cannot_be_read_ends_the_step() {
    let dir = repo(&[("m/Cargo.toml", "x"), ("n/Cargo.toml", "y")]);
    std::fs::remove_file(dir.path().join("m/Cargo.toml")).unwrap();
    std::os::unix::fs::symlink("n/Cargo.toml", dir.path().join("m/Cargo.toml")).unwrap();
    assert_eq!(run(&STEP, dir.path(), "the m crate or the n crate contains?", ""), "");
}
