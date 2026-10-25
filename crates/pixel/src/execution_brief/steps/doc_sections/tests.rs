// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The cases of `probes/doc-sections/tests/` (issues #1200 and #1234), run on
//! the step.

use super::super::testing::{git, repo, run_kind};
use super::*;

fn output(prompt: &str, files: &[(&str, &str)], kind: Option<&str>, brief: &str) -> String {
    let dir = repo(files);
    run_kind(&STEP, dir.path(), prompt, brief, kind)
}

fn headers(text: &str) -> Vec<&str> {
    text.lines().filter(|line| line.starts_with("### ")).collect()
}

const RST: &str = "Configuration Handling
======================

Applications need some kind of configuration.

Prose before a transition.

----

Configuring from Environment Variables
--------------------------------------

Point an environment variable at a file and load it with from_envvar.

Unrelated Topic
---------------

Nothing about the question here.
";

const GUIDE: &str = "# Environment variables

Environment variables configure loading.
First setting.
Second setting.
Last setting.
";

const ASKED: &str = "environment variables configure loading";

fn guide(extra: &str) -> Vec<(&'static str, String)> {
    vec![("docs/guide.md", format!("{GUIDE}{extra}"))]
}

fn ask_guide(extra: &str, brief: &str) -> String {
    let files = guide(extra);
    let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (*a, b.as_str())).collect();
    output(ASKED, &refs, Some("config"), brief)
}

#[test]
fn an_underlined_rst_title_is_a_section_and_the_matching_one_is_quoted() {
    assert_eq!(
        headers(&output(
            "how do I load configuration from a file named by an environment variable",
            &[("docs/config.rst", RST)],
            Some("config"),
            ""
        )),
        ["### docs/config.rst:10-14 — Configuring from Environment Variables (documentation section)"]
    );
}

#[test]
fn a_transition_rule_after_a_blank_line_is_not_a_title() {
    let found = output("prose transition", &[("docs/config.rst", RST)], Some("config"), "");
    assert_eq!(
        headers(&found),
        ["### docs/config.rst:1-9 — Configuration Handling (documentation section)"]
    );
    let titles: Vec<String> = sections(
        &RST.lines().map(ToString::to_string).collect::<Vec<_>>(),
        "x.rst",
    )
    .into_iter()
    .map(|(start, end, title)| format!("{start}-{end} {title}"))
    .collect();
    assert_eq!(
        titles,
        [
            "1-9 Configuration Handling",
            "10-14 Configuring from Environment Variables",
            "15-18 Unrelated Topic"
        ]
    );
}

#[test]
fn markdown_headings_still_make_sections() {
    let readme = "# Tool\n\nA tool.\nIt sorts.\nIt prints.\nIt stops.\n\n## Environment variables\n\nLoading reads TOOL_CONFIG.\n";
    assert_eq!(
        headers(&output(
            "how do environment variables configure loading",
            &[("README.md", readme)],
            Some("config"),
            ""
        )),
        ["### README.md:8-10 — Environment variables (documentation section)"]
    );
}

#[test]
fn a_tracked_symlink_to_a_file_outside_the_repo_or_to_dev_zero_is_not_read() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(
        outside.path().join("npmrc.md"),
        "# Environment variables\n\nLoading configuration reads SECRET_TOKEN.\n",
    )
    .unwrap();
    for target in [outside.path().join("npmrc.md"), "/dev/zero".into()] {
        let dir = repo(&[]);
        std::fs::create_dir_all(dir.path().join("docs")).unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("docs/x.md")).unwrap();
        git(dir.path(), &["add", "."]);
        let out = run_kind(&STEP, dir.path(), "how do environment variables configure loading", "", Some("config"));
        assert_eq!(out, "", "{target:?}");
    }
}

#[test]
fn source_navigation_and_unclassified_questions_do_not_receive_matching_docs() {
    for kind in [Some("lookup"), Some("tests"), Some("impact"), Some("rationale"), None] {
        let prompt = if kind.is_none() {
            ASKED
        } else {
            "where is the environment variable loading constant defined"
        };
        let files = guide("");
        let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (*a, b.as_str())).collect();
        assert_eq!(output(prompt, &refs, kind, ""), "", "{kind:?}");
    }
}

#[test]
fn config_and_overview_keep_documentation_with_exact_source_rows() {
    for kind in ["config", "overview", "flow", "bugfix"] {
        let files = guide("");
        let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (*a, b.as_str())).collect();
        let text = output("how do environment variables configure loading", &refs, Some(kind), "");
        assert_eq!(
            headers(&text),
            ["### docs/guide.md:1-6 — Environment variables (documentation section)"],
            "{kind}"
        );
        assert!(text.contains("3| Environment variables configure loading.\n"), "{kind}");
    }
}

#[test]
fn the_weak_kinds_are_vetoed_only_by_a_source_word() {
    for prompt in [
        "which callers load environment variables",
        "what is the loading constant of the environment",
        "does the callee read environment variables",
        "trace environment variable loading through load_config",
        "environment variables CONFIG_PATH loading",
    ] {
        assert!(source_question(Some("flow"), prompt), "{prompt}");
    }
    assert!(!source_question(Some("flow"), "how do environment variables configure loading"));
    assert!(!source_question(Some("overview"), "load_config constants"));
    assert!(!source_question(Some("config"), "callers of load_config"));
    assert!(source_question(None, "anything"));
}

#[test]
fn overlap_is_split_into_exact_unquoted_ranges_despite_a_different_label() {
    let brief = "### docs/guide.md:3-4 — native excerpt\n3| Environment variables configure loading.\n4| First setting.\n";
    let text = ask_guide("", brief);
    assert_eq!(
        headers(&text),
        [
            "### docs/guide.md:1-2 — Environment variables (documentation section)",
            "### docs/guide.md:5-6 — Environment variables (documentation section)"
        ]
    );
    assert!(!text.contains("3| ") && !text.contains("4| "), "{text}");
    assert!(text.contains("5| Second setting.\n"), "{text}");
}

#[test]
fn a_fully_quoted_section_is_not_replaced_by_a_lower_ranked_section() {
    let numbered: String = GUIDE
        .lines()
        .enumerate()
        .map(|(n, line)| format!("{}| {line}\n", n + 1))
        .collect();
    let quoted: String = ["docs/a.md", "docs/b.md"]
        .iter()
        .map(|name| format!("### {name}:1-6 — native excerpt\n{numbered}"))
        .collect();
    let files = [
        ("docs/a.md", GUIDE),
        ("docs/b.md", GUIDE),
        ("docs/c.md", "# Environment variables\n\nAnother setting.\n"),
    ];
    assert_eq!(output(ASKED, &files, Some("config"), &quoted), "");
}

#[test]
fn a_pointer_without_quoted_rows_does_not_prevent_inline_documentation() {
    let text = ask_guide("", "read: docs/guide.md:1-6\n");
    assert!(text.contains("### docs/guide.md:1-6"), "{text}");
}

#[test]
fn section_limit_announces_the_unquoted_tail_without_extending_the_header() {
    let text = ask_guide(&"More prose.\n".repeat(150), "");
    assert_eq!(
        headers(&text),
        ["### docs/guide.md:1-121 — Environment variables (documentation section)"]
    );
    assert!(text.contains("121| More prose.\n"), "{text}");
    assert!(!text.contains("122| "), "{text}");
    assert!(
        text.ends_with("excerpt truncated: section continues after line 121 (ends at 156)\n"),
        "{text}"
    );
}

#[test]
fn long_rows_announce_the_cut_and_keep_the_exact_source_prefix() {
    let long = "A".repeat(240);
    let text = ask_guide(&format!("{long}\n"), "");
    assert!(text.contains(&format!("7| {} … [truncated]\n", &long[..160])), "{text}");
    assert!(!text.contains(&long[..161]), "{text}");
    let exact = ask_guide(&format!("{}\n", &long[..160]), "");
    assert!(exact.contains(&format!("7| {}\n", &long[..160])), "{exact}");
}

#[test]
fn the_two_best_sections_are_quoted_by_score_then_path() {
    let same = "# Environment variables loading\n\nbody\n";
    let files = [
        ("docs/b.md", same),
        ("docs/a.md", same),
        ("docs/c.md", same),
        ("docs/d.md", "# Unrelated words only\n\nbody\n"),
    ];
    let text = output(ASKED, &files, Some("config"), "");
    assert_eq!(
        headers(&text),
        [
            "### docs/a.md:1-3 — Environment variables loading (documentation section)",
            "### docs/b.md:1-3 — Environment variables loading (documentation section)"
        ]
    );
}

#[test]
fn a_rarer_word_outweighs_a_common_one() {
    // `loading` heads every section; `kernel` heads one: that section wins
    // over the one that is first by path.
    let files = [
        ("docs/a.md", "# Loading things\n\nloading stuff here\n"),
        ("docs/b.md", "# Loading kernel images\n\nloading kernel\n"),
        ("docs/c.md", "# Loading maps\n\nloading maps\n"),
    ];
    let text = output("how does loading the kernel work", &files, Some("config"), "");
    assert_eq!(
        headers(&text)[0],
        "### docs/b.md:1-3 — Loading kernel images (documentation section)"
    );
}

#[test]
fn changelogs_vendor_and_deep_paths_are_not_documentation_and_one_word_is_not_enough() {
    let section = "# Environment variables loading\n\nbody\n";
    for path in [
        "CHANGELOG.md",
        "docs/changelog.md",
        "vendor/x.md",
        "website/a.md",
        "benchmarks/a.md",
        "a/b/c/d.md",
        "node_modules/x.md",
        "LICENSE.md",
    ] {
        assert_eq!(output(ASKED, &[(path, section)], Some("config"), ""), "", "{path}");
    }
    assert!(output(ASKED, &[("a/b/c.md", section)], Some("config"), "").starts_with("### a/b/c.md"));
    assert!(output(ASKED, &[("docs/x.mdx", section)], Some("config"), "").starts_with("### docs/x.mdx"));
    assert_eq!(output("environment", &[("a.md", section)], Some("config"), ""), "");
    assert_eq!(output(ASKED, &[("a.txt", section)], Some("config"), ""), "");
}

#[test]
fn stem_and_words_follow_the_script() {
    assert_eq!(stem_ing_es_s("Loading"), "load");
    assert_eq!(stem_ing_es_s("Boxes"), "box");
    assert_eq!(stem_ing_es_s("Cats"), "cats", "four letters are kept");
    assert_eq!(stem_ing_es_s("Parts"), "part");
    assert_eq!(stem_ing_es_s("sings"), "sing", "`s` is tried after `ing` and `es`");
    assert_eq!(stem_ing_es_s("xxes"), "xxes");
    let found = words("The Loading of files, CODE and a Setting: ab 12 x-ray");
    assert_eq!(found.into_iter().collect::<Vec<_>>(), ["load", "ray", "sett"]);
}

#[test]
fn rst_rules_are_one_punctuation_character_three_times_or_more() {
    for rule in ["===", "-----  ", "~~~~", "^^^", "***", "+++", "###", "\"\"\"", "'''", "```", ":::", "..."] {
        assert!(is_rst_rule(rule), "{rule:?}");
    }
    for not in ["", "==", "=-=", "== =", "===x", " ===", "abc", "= = =", "=====a"] {
        assert!(!is_rst_rule(not), "{not:?}");
    }
}

#[test]
fn an_rst_underline_must_be_as_long_as_its_title() {
    let lines = |text: &str| text.lines().map(ToString::to_string).collect::<Vec<_>>();
    assert_eq!(sections(&lines("Title\n====\nbody\n"), "a.rst"), []);
    assert_eq!(
        sections(&lines("Title\n=====\nbody\n"), "a.rst"),
        [(1, 3, "Title".to_owned())]
    );
    assert_eq!(
        sections(&lines("Title  \n=====  \nbody\n"), "a.rst"),
        [(1, 3, "Title".to_owned())]
    );
    assert_eq!(sections(&lines("=====\n=====\n"), "a.rst"), []);
}

#[test]
fn markdown_headings_are_one_to_four_hashes_and_a_space_and_text() {
    let lines = |text: &str| text.lines().map(ToString::to_string).collect::<Vec<_>>();
    let text = "# One\n## Two ##\n#### Four\n##### Five\n#NoSpace\n# \n#  \ntext\n";
    assert_eq!(
        sections(&lines(text), "a.md"),
        [
            (1, 1, "One".to_owned()),
            (2, 2, "Two ##".to_owned()),
            (3, 8, "Four".to_owned())
        ]
    );
}

#[test]
fn quoted_rows_count_only_inside_their_header_bounds() {
    let brief = "[PIXEL:BRIEF]\n### a.md:2-3 — x\n1| no\n2| yes\n3| yes\n4| no\n### b.md:7 — single\n7| yes\n8| no\n### not a header\n9| no\n### c.md:1-2\n1| yes\nnot a row\n2| yes\n";
    let held = quoted_lines(brief);
    assert_eq!(held["a.md"], BTreeSet::from([2, 3]));
    assert_eq!(held["b.md"], BTreeSet::from([7]));
    assert_eq!(held["c.md"], BTreeSet::from([1, 2]));
    assert_eq!(held.len(), 3);
}

#[test]
fn novel_ranges_stop_before_each_held_row() {
    let held = BTreeSet::from([3, 4, 8]);
    assert_eq!(novel_ranges(1, 9, &held), [(1, 2), (5, 7), (9, 9)]);
    assert_eq!(novel_ranges(3, 4, &held), []);
    assert_eq!(novel_ranges(1, 2, &BTreeSet::new()), [(1, 2)]);
    assert_eq!(novel_ranges(5, 5, &held), [(5, 5)]);
}

#[test]
fn the_score_is_rounded_to_three_decimals_like_python() {
    assert_eq!(rounded(1.234_56), 1.235);
    assert_eq!(rounded(1.234_4), 1.234);
    assert_eq!(rounded(0.0005), 0.001, "an exact binary tie goes to even only when it is one");
    assert_eq!(rounded(2.0), 2.0);
}
