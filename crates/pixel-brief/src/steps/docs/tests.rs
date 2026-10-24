// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

use std::time::{Duration, Instant};

use super::*;
use crate::steps::testing::{Repo, texts};
use crate::steps::{Steps, blocks};

fn ask(repo: &Repo, typed: &str, kind: QuestionKind) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    texts(&blocks(typed, kind, Steps::default(), &[], repo, deadline).append)
}

const GUIDE: &str = "# Guide\n\n## Smart case matching\n\nSmart case makes a search case sensitive only when the pattern holds an uppercase letter.\nIt is on by default.\n\n## Other things\n\nUnrelated text about colors.\n";

#[test]
fn a_section_whose_heading_names_the_questions_words_is_quoted() {
    let repo = Repo::new(&[("docs/guide.md", GUIDE)]);
    assert_eq!(
        ask(
            &repo,
            "how does smart case matching decide sensitivity",
            QuestionKind::Rationale
        ),
        vec![
            "### docs/guide.md:3-7 — Smart case matching (documentation section)\n\
             3| ## Smart case matching\n\
             4| \n\
             5| Smart case makes a search case sensitive only when the pattern holds an uppercase letter.\n\
             6| It is on by default.\n\
             7| "
        ]
    );
}

#[test]
fn a_question_about_source_gets_no_documentation() {
    let repo = Repo::new(&[("docs/guide.md", GUIDE)]);
    let prompt = "how does smart case matching decide sensitivity";
    for kind in [
        QuestionKind::Lookup,
        QuestionKind::Tests,
        QuestionKind::Bugfix,
        QuestionKind::Review,
        QuestionKind::History,
    ] {
        assert!(ask(&repo, prompt, kind).is_empty(), "{kind:?}");
    }
    // A named constant, caller or snake_case identifier is a source question
    // unless the kind is one that always welcomes docs.
    for prompt in [
        "how does smart case matching use the constant",
        "who are the callers of smart case matching",
        "how does smart_case matching decide sensitivity",
    ] {
        assert!(
            ask(&repo, prompt, QuestionKind::Flow).is_empty(),
            "{prompt}"
        );
        assert!(
            !ask(&repo, prompt, QuestionKind::Config).is_empty(),
            "{prompt}"
        );
    }
}

#[test]
fn one_content_word_is_not_enough_and_the_flag_switches_the_step_off() {
    let repo = Repo::new(&[("docs/guide.md", GUIDE)]);
    assert!(ask(&repo, "what is smart", QuestionKind::Rationale).is_empty());
    let deadline = Instant::now() + Duration::from_secs(5);
    let off = Steps::default()
        .without(&["doc-sections".to_string()])
        .unwrap();
    assert!(
        blocks(
            "how does smart case matching decide",
            QuestionKind::Rationale,
            off,
            &[],
            &repo,
            deadline
        )
        .append
        .is_empty()
    );
}

#[test]
fn a_word_that_heads_every_section_does_not_outrank_a_rare_one() {
    let pages = "# Pixel tour\n\nPixel basics.\n\n## Pixel install\n\nPixel install steps.\n\n## Pixel search tuning\n\nPixel search tuning notes for regex search.\n";
    let repo = Repo::new(&[("docs/a.md", pages)]);
    let shown = ask(&repo, "pixel search tuning regex", QuestionKind::Rationale);
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert!(
        shown[0].starts_with("### docs/a.md:9-11 — Pixel search tuning (documentation section)"),
        "{}",
        shown[0]
    );
}

#[test]
fn only_the_two_best_sections_are_shown_and_the_changelog_never() {
    let page = |n: u32| {
        format!("# Page\n\n## Cache eviction policy {n}\n\nCache eviction policy details.\n")
    };
    let repo = Repo::new(&[
        ("docs/a.md", &page(1)),
        ("docs/b.md", &page(2)),
        ("docs/c.md", &page(3)),
        (
            "CHANGELOG.md",
            "# Log\n\n## Cache eviction policy\n\nCache eviction policy changed.\n",
        ),
        ("a/b/c/deep.md", &page(4)),
    ]);
    let shown = ask(
        &repo,
        "explain the cache eviction policy",
        QuestionKind::Config,
    );
    let heads: Vec<&str> = shown
        .iter()
        .map(|block| block.lines().next().unwrap())
        .collect();
    assert_eq!(heads.len(), 2, "{heads:?}");
    assert!(heads.iter().all(|head| head.contains("docs/")), "{heads:?}");
}

#[test]
fn a_long_section_is_cut_and_says_where_it_continues() {
    let body: String = (0..200)
        .map(|n| format!("Cache eviction line {n}\n"))
        .collect();
    let wide = format!("{}\n", "w".repeat(200));
    let page = format!("## Cache eviction policy\n{wide}{body}");
    let repo = Repo::new(&[("docs/a.md", &page)]);
    let shown = ask(
        &repo,
        "explain the cache eviction policy",
        QuestionKind::Config,
    );
    assert_eq!(shown.len(), 1);
    assert!(
        shown[0].starts_with("### docs/a.md:1-121 — Cache eviction policy (documentation section)"),
        "{}",
        shown[0].lines().next().unwrap()
    );
    assert!(shown[0].contains(&format!("2| {} … [truncated]", "w".repeat(160))));
    assert!(
        shown[0].ends_with("excerpt truncated: section continues after line 121 (ends at 202)"),
        "{}",
        shown[0].lines().last().unwrap()
    );
}

#[test]
fn restructured_text_sections_are_underlined_titles() {
    let page = "Intro\n=====\n\nHello.\n\nCache eviction policy\n---------------------\n\nCache eviction policy explained.\n";
    let repo = Repo::new(&[("docs/a.rst", page)]);
    let shown = ask(
        &repo,
        "explain the cache eviction policy",
        QuestionKind::Config,
    );
    assert_eq!(
        shown,
        vec![
            "### docs/a.rst:6-9 — Cache eviction policy (documentation section)\n\
             6| Cache eviction policy\n\
             7| ---------------------\n\
             8| \n\
             9| Cache eviction policy explained."
        ]
    );
    // An underline shorter than its title does not make one.
    let short = Repo::new(&[(
        "docs/a.rst",
        "Cache eviction policy\n---\n\nCache eviction policy.\n",
    )]);
    assert!(
        ask(
            &short,
            "explain the cache eviction policy",
            QuestionKind::Config
        )
        .is_empty()
    );
}

#[test]
fn stems_and_rules_follow_the_prototype() {
    assert_eq!(stem("matching"), "match");
    assert_eq!(stem("classes"), "class");
    assert_eq!(stem("rules"), "rul", "the prototype cuts `es` before `s`");
    assert_eq!(stem("cats"), "cats");
    assert_eq!(stem("Smart"), "smart");
    assert!(is_rst_rule("====="));
    assert!(is_rst_rule("~~~  "));
    assert!(!is_rst_rule("=="));
    assert!(!is_rst_rule("=-="));
    assert!(!is_rst_rule("abc"));
}
