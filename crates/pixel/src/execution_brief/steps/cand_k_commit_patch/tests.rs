// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The cases of `probes/cand-k-commit-patch/tests/test_probe.py` and the
//! cand-k half of `probes/history-fallback/tests/test_subjects.py`, run on the
//! step, and one case per rule of the block.

use std::path::Path;

use super::super::testing::{
    applied, commit_line, commit_staged, git, outcomes_of, repo, run, short_head,
};
use super::*;

const TITLE: &str = "rework every widget module";

fn ask(root: &Path, prompt: &str, brief: &str) -> String {
    run(&STEP, root, prompt, brief)
}

fn about_title(root: &Path) -> String {
    ask(root, &format!("what did the commit \"{TITLE}\" change?"), "")
}

/// A base commit, then one commit titled [`TITLE`] that rewrites `files` source
/// files of 40 lines each.
fn wide(files: usize) -> tempfile::TempDir {
    let dir = repo(&[("README.md", "widgets\n")]);
    commit_staged(dir.path(), "base", "");
    for n in 0..files {
        let lines: String = (0..40)
            .map(|i| format!("pub fn widget_{n}_part_{i}() -> u32 {{ {i} }}\n"))
            .collect();
        let path = dir.path().join(format!("src/widget_{n:03}.rs"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, lines).unwrap();
    }
    git(dir.path(), &["add", "."]);
    commit_staged(dir.path(), TITLE, "");
    dir
}

/// The cut of a commit of `files` files: the stat, the hunks shown, the count
/// of the files left out.
fn assert_budget(files: usize) -> (usize, String) {
    let dir = wide(files);
    let out = about_title(dir.path());
    let (stat, hunks) = out.split_once("\n@@ ").unwrap_or((&out, ""));
    let named = (0..files)
        .filter(|n| stat.contains(&format!("src/widget_{n:03}.rs |")))
        .count();
    assert_eq!(named, files, "every file is named once, in the stat");
    assert!(out.len() <= stat.len() + 4_800 + 200, "{} bytes", out.len());
    let shown = format!("@@ {hunks}")
        .lines()
        .filter(|line| line.starts_with("@@ src/"))
        .count();
    assert!(
        out.ends_with(&format!(
            "... hunks of {} more files cut (named in the list above)\n",
            files - shown
        )),
        "{shown} shown\n{out}"
    );
    (shown, out)
}

#[test]
fn a_wide_commit_stops_its_hunks_at_the_budget_and_counts_the_rest() {
    // The stat alone is past the budget: no hunk at all.
    let (shown, out) = assert_budget(120);
    assert_eq!((shown, out.matches("(hunks cut)").count()), (0, 0));
    // The stat leaves room for a few hunks, each cut to what is left: the
    // numbers the script gives on the same repository.
    let (shown, out) = assert_budget(60);
    assert_eq!((shown, out.matches("(hunks cut)").count()), (5, 5));
}

#[test]
fn a_small_commit_shows_every_hunk_and_no_cut_line() {
    let dir = wide(2);
    let out = about_title(dir.path());
    assert!(out.contains("\n@@ src/widget_000.rs\n"), "{out}");
    assert!(out.contains("\n@@ src/widget_001.rs\n"), "{out}");
    assert!(!out.contains("more files cut"), "{out}");
    assert!(!out.contains("(hunks cut)"), "{out}");
    assert!(out.contains("+pub fn widget_0_part_39() -> u32 { 39 }"), "{out}");
}

#[test]
fn the_block_opens_with_the_history_header_and_the_commit_line() {
    let dir = wide(1);
    let out = about_title(dir.path());
    let head = short_head(dir.path());
    let first = out.lines().next().unwrap();
    assert_eq!(first, format!("### commit {head} — message, files and patch"));
    assert_eq!(out.lines().nth(2), Some(TITLE));
    assert!(out.contains("\nfiles in the commit:\n src/widget_000.rs | 40 +"), "{out}");
}

/// The commit a `### commit <id> — …` header names.
fn commit_of(out: &str) -> &str {
    out.lines()
        .find_map(|line| line.strip_prefix("### commit "))
        .and_then(|rest| rest.split(' ').next())
        .unwrap_or_else(|| panic!("no commit block in {out:?}"))
}

fn assert_commit(root: &Path, prompt: &str, expected: &str) {
    let out = ask(root, prompt, "");
    assert!(expected.starts_with(commit_of(&out)), "{out}");
    assert!(out.contains("+feature = True"), "{out}");
    assert_eq!(out, ask(root, prompt, ""), "deterministic");
}

#[test]
fn subject_evidence_beats_incidental_body_and_keeps_short_words() {
    let dir = repo(&[]);
    let expected = commit_line(dir.path(), "feat: compact data pack", "");
    commit_line(
        dir.path(),
        "docs: refresh output",
        "introduced compact data in a later manual",
    );
    assert_commit(
        dir.path(),
        "which commit introduced the compact data pack and what did it add",
        &expected,
    );
}

#[test]
fn a_two_word_subject_phrase_is_enough() {
    let dir = repo(&[]);
    let expected = commit_line(dir.path(), "feat: delta compression", "");
    commit_line(
        dir.path(),
        "docs: refresh manual",
        "introduced delta behavior and compression examples",
    );
    assert_commit(dir.path(), "which commit introduced delta compression", &expected);
}

#[test]
fn an_exact_quoted_title_keeps_priority() {
    let dir = repo(&[]);
    let expected = commit_line(dir.path(), "fix the archive reader", "");
    commit_line(dir.path(), "feat: compact data pack", "");
    assert_commit(
        dir.path(),
        "what did \"fix the archive reader\" change for the compact data pack",
        &expected,
    );
}

#[test]
fn an_exact_release_tag_keeps_priority() {
    let dir = repo(&[]);
    let expected = commit_line(dir.path(), "release 7.2.0", "");
    git(dir.path(), &["tag", "v7.2.0"]);
    commit_line(dir.path(), "feat: compact data pack", "");
    assert_commit(
        dir.path(),
        "what changed in v7.2.0 for the compact data pack",
        &expected,
    );
}

#[test]
fn the_prompts_rarest_words_find_a_commit_but_never_by_a_neighbour_holding_only_one() {
    let dir = repo(&[]);
    commit_line(dir.path(), "chore: tidy", "the wombat frobnicates");
    let expected = commit_line(dir.path(), "chore: update", "a frobnicate pass over wombat code");
    commit_line(dir.path(), "chore: other", "wombat only");
    assert_commit(dir.path(), "who touched wombat and frobnicate code", &expected);
    let lone = repo(&[]);
    let only = commit_line(lone.path(), "chore: update", "the grommet pass");
    assert_commit(lone.path(), "when was the grommet changed", &only);
    let apart = repo(&[]);
    commit_line(apart.path(), "chore: one", "frobnicate things");
    commit_line(apart.path(), "chore: two", "wombat things");
    assert_eq!(
        ask(apart.path(), "who touched wombat and frobnicate", ""),
        "",
        "no commit holds both rarest words: silent, where history-fallback shows one"
    );
}

#[test]
fn a_question_naming_nothing_the_history_holds_gets_nothing() {
    let dir = repo(&[]);
    commit_line(dir.path(), "feat: delta compression", "");
    assert_eq!(ask(dir.path(), "which commit made the zebra stripe", ""), "");
    assert_eq!(ask(dir.path(), "which commit", ""), "");
    let empty = repo(&[("a.txt", "x")]);
    assert_eq!(ask(empty.path(), "which commit introduced delta compression", ""), "");
}

#[test]
fn a_brief_that_holds_a_history_or_commit_block_gets_nothing() {
    let dir = wide(1);
    let prompt = format!("what did the commit \"{TITLE}\" change?");
    for held in ["### history — commits touching it", "### commit abc1234 — message"] {
        let brief = format!("[PIXEL:BRIEF]\n{held}\nx\n");
        assert_eq!(ask(dir.path(), &prompt, &brief), "", "{held}");
    }
    let other = "[PIXEL:BRIEF]\n### a history of it\nx\n";
    assert_ne!(ask(dir.path(), &prompt, other), "");
}

/// flask-05 (#1351): a held block that misses the asked version's commit
/// still gets its patch; one that shows it gets nothing.
#[test]
fn a_version_the_held_block_misses_still_earns_its_patch() {
    let dir = repo(&[]);
    let expected = commit_line(dir.path(), "drop support for Python 3.10", "");
    commit_line(dir.path(), "dropped pkg_resources", "");
    let prompt = "which commit dropped support for Python 3.10?";
    let missed = "[PIXEL:BRIEF]\n### history — commits matching dropped\nfedc06c295 2010-07-04 dropped pkg_resources\n";
    let out = ask(dir.path(), prompt, missed);
    assert!(out.contains(&format!("### commit {}", &expected[..7])), "{out}");
    let held = format!("[PIXEL:BRIEF]\n### history — commits matching 3.10\n{expected} x\n");
    assert_eq!(ask(dir.path(), prompt, &held), "");
}

#[test]
fn every_other_block_of_the_brief_is_dropped_by_its_exact_header() {
    let dir = wide(1);
    let prompt = format!("what did the commit \"{TITLE}\" change?");
    let brief = "[PIXEL:BRIEF]\n### a.rs:1-1 — a\n1| x\n### some (b).rs:2-3 — b-c\n2| y\n";
    let out = ask(dir.path(), &prompt, brief);
    assert!(
        out.starts_with("drop: ^(?:a\\.rs:1\\-1\\ —\\ a|some\\ \\(b\\)\\.rs:2\\-3\\ —\\ b\\-c)$\n### commit "),
        "{out}"
    );
    assert!(!ask(dir.path(), &prompt, "[PIXEL:BRIEF]\n").starts_with("drop:"));
}

#[test]
fn source_files_come_first_then_manifests_and_lockfiles_are_only_summarised() {
    let dir = repo(&[]);
    let root = dir.path();
    commit_line(root, "base", "");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"w\"\n").unwrap();
    let lock: String = (0..20).map(|n| format!("pkg-{n} = \"1.{n}\"\n")).collect();
    std::fs::write(root.join("Cargo.lock"), lock).unwrap();
    std::fs::write(root.join("app.min.js"), "var a=1;\n").unwrap();
    git(root, &["add", "."]);
    commit_staged(root, "tune the widget build", "");
    let out = ask(root, "what did the commit \"tune the widget build\" change?", "");
    let at = |needle: &str| out.find(needle).unwrap_or_else(|| panic!("{needle} in {out}"));
    assert!(at("\n@@ src/a.rs\n") < at("\n@@ Cargo.toml\n"), "{out}");
    assert!(at("\n@@ Cargo.toml\n") < at("\n@@ Cargo.lock (lockfile"), "{out}");
    assert!(
        out.contains("\n@@ Cargo.lock (lockfile, 20 edited lines; first ones)\n+pkg-0 = \"1.0\"\n"),
        "{out}"
    );
    assert!(out.contains("+pkg-5 = \"1.5\"\n@@ app.min.js (lockfile, 1 edited lines; first ones)\n+var a=1;"), "{out}");
    assert!(!out.contains("+pkg-6 = "), "six edited lines of a lockfile at most\n{out}");
}

#[test]
fn a_merge_shows_what_it_brought_to_its_first_parent() {
    let dir = repo(&[]);
    let root = dir.path();
    commit_line(root, "feat: base work", "");
    git(root, &["checkout", "-q", "-b", "side"]);
    std::fs::write(root.join("sprocket.txt"), "teeth\n").unwrap();
    git(root, &["add", "sprocket.txt"]);
    commit_staged(root, "side: sprocket", "");
    git(root, &["checkout", "-q", "-"]);
    git(
        root,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@t",
            "merge",
            "--no-ff",
            "-q",
            "-m",
            "land the sprocket work",
            "side",
        ],
    );
    let out = ask(root, "what did \"land the sprocket work\" change", "");
    assert!(out.contains("\n sprocket.txt | 1 +\n"), "{out}");
    assert!(out.contains("\n@@ sprocket.txt\n@@ -0,0 +1 @@\n+teeth\n"), "{out}");
}

#[test]
fn fenced_output_of_a_message_is_cut_but_for_its_figures() {
    let dir = repo(&[]);
    let body = [
        "Faster parsing.",
        "",
        "",
        "```",
        "Summary 1.00 \u{b1} 0.1",
        "  fast: 3.2x faster",
        "Range: 5 ms",
        "no figure here",
        "a: 1 ms",
        "b: 2 ms",
        "c: 3 ms",
        "d: 4 ms",
        "e: 5 ms",
        "f: 6 ms",
        "```",
        "Closing words.",
    ]
    .join("\n");
    commit_line(dir.path(), "perf: parse quickly", &body);
    let out = ask(dir.path(), "what did the commit \"perf: parse quickly\" change?", "");
    let message = out
        .split("\nfiles in the commit:")
        .next()
        .unwrap()
        .lines()
        .skip(2)
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        message,
        "perf: parse quickly\n\nFaster parsing.\n\n  fast: 3.2x faster\n  a: 1 ms\n  b: 2 ms\n  c: 3 ms\n  d: 4 ms\n  e: 5 ms\nClosing words."
    );
}

#[test]
fn the_message_is_cut_after_thirty_four_lines_and_every_line_at_150_characters() {
    let dir = repo(&[]);
    let body: Vec<String> = (1..=60).map(|n| format!("line{n}")).collect();
    commit_line(dir.path(), "feat: delta compression", &body.join("\n"));
    let out = ask(dir.path(), "which commit introduced delta compression", "");
    let message: Vec<&str> = out
        .split("\nfiles in the commit:")
        .next()
        .unwrap()
        .lines()
        .skip(1)
        .collect();
    // The author line, the subject, a blank line and the first 32 lines of the body.
    assert_eq!(message.len(), 35, "{out}");
    assert_eq!(message.last(), Some(&"line32"));
    let dir = repo(&[]);
    commit_line(dir.path(), "feat: delta compression", &"w".repeat(300));
    let out = ask(dir.path(), "which commit introduced delta compression", "");
    assert!(out.contains(&format!("\n{}\n", "w".repeat(150))), "{out}");
    assert!(!out.contains(&"w".repeat(151)), "{out}");
}

#[test]
fn sections_drop_the_index_and_mode_lines_and_keep_the_rest_in_order() {
    let diff = [
        "diff --git a/a b.rs b/a b.rs",
        "new file mode 100644",
        "index 0000000..1111111",
        "--- /dev/null",
        "+++ b/a b.rs",
        "@@ -0,0 +1 @@",
        "+x",
        "diff --git a/c.rs b/c.rs",
        "similarity index 90%",
        "deleted file mode 100644",
        "@@ -1 +0,0 @@",
        "-y",
    ]
    .join("\n");
    assert_eq!(
        sections(&diff),
        [
            ("a b.rs".to_owned(), vec!["@@ -0,0 +1 @@".to_owned(), "+x".to_owned()]),
            ("c.rs".to_owned(), vec!["@@ -1 +0,0 @@".to_owned(), "-y".to_owned()]),
        ]
    );
    assert!(sections("stray line\nindex 1..2").is_empty());
}

#[test]
fn files_rank_as_sources_then_manifests_then_lockfiles() {
    for source in ["src/a.rs", "docs/Cargo.toml.md", "Cargo.lock.rs", "x.snapshot"] {
        assert_eq!(rank(source), 0, "{source}");
    }
    for manifest in ["Cargo.toml", "crates/a/Cargo.toml", "package.json", "go.mod", "pom.xml", "Gemfile"] {
        assert_eq!(rank(manifest), 1, "{manifest}");
    }
    for lock in ["Cargo.lock", "a/bun.lockb", "bun.lock", "yarn.lock", "go.sum", "dist/a.min.css", "t/x.snap", "web/pnpm-lock.yaml"] {
        assert_eq!(rank(lock), 2, "{lock}");
    }
    assert_eq!(rank("notCargo.lock"), 0);
}

#[test]
fn the_step_applies_to_history_questions_but_not_to_a_review() {
    assert!(STEP.applies(Some("history"), "anything", true));
    assert!(!STEP.applies(Some("review"), "which commit introduced it", false));
    assert_eq!(STEP.mode, ProbeMode::Lead);
}

#[test]
fn the_registered_step_takes_the_script_s_place_and_replaces_the_history_fallback_block() {
    let dir = wide(1);
    let prompt = format!("what did the commit \"{TITLE}\" change?");
    let (merged, runs) = applied(STEP.name, dir.path(), "history", &prompt, "");
    assert_eq!(outcomes_of(&runs, "cand-k-commit-patch"), ["ok"]);
    assert_eq!(outcomes_of(&runs, "history-fallback"), ["ok"]);
    let merged = merged.unwrap();
    let headers: Vec<&str> = merged.lines().filter(|l| l.starts_with("### ")).collect();
    let head = short_head(dir.path());
    assert_eq!(headers.len(), 2, "{merged}");
    assert!(headers[0].starts_with(&format!("### commit {head}")), "{merged}");
    assert!(headers[1].starts_with("### history — commits matching "), "{merged}");
    assert!(merged.contains("\nfiles in the commit:\n"), "the lead block is cand-k's\n{merged}");
}
