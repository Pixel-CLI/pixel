// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Contract tests for task tokenizing and target fusion: which words become
//! search terms, which files an agent is told to read, and that every list
//! cut short says so instead of claiming to be exhaustive.

use super::*;
use pixel_graph::store::{SymbolKind, SymbolRow};

fn query(keywords: &[&str]) -> TaskQuery {
    TaskQuery {
        exact_tokens: vec![],
        path_tokens: vec![],
        keywords: keywords.iter().map(ToString::to_string).collect(),
        keywords_truncated: false,
        language: TaskLanguage::English,
    }
}

fn content(rows: &[(&str, &str, u32)]) -> BTreeMap<String, Vec<(String, u32)>> {
    let mut map: BTreeMap<String, Vec<(String, u32)>> = BTreeMap::new();
    for (kw, path, n) in rows {
        map.entry((*kw).to_string())
            .or_default()
            .push(((*path).to_string(), *n));
    }
    map
}

fn paths(list: &[&str]) -> Vec<String> {
    list.iter().map(ToString::to_string).collect()
}

fn listed(report: &TargetsReport) -> Vec<(String, String)> {
    report
        .targets
        .iter()
        .map(|t| (t.path.clone(), t.tier.clone()))
        .collect()
}

fn caps(report: &TargetsReport) -> Vec<String> {
    report.envelope["caps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_string())
        .collect()
}

fn opts(limit: usize, max_tier: Option<&str>, precision_mode: bool) -> TargetsOptions {
    TargetsOptions {
        limit,
        max_tier: max_tier.map(ToString::to_string),
        precision_mode,
    }
}

/// `src/login.rs` is P0 (filename + content), `src/b.rs` P1 (content only),
/// `src/near.rs` P2 (cluster only).
fn three_tiers() -> SignalInputs {
    SignalInputs {
        all_paths: paths(&["src/b.rs", "src/login.rs", "src/near.rs"]),
        content_hits: content(&[("login", "src/login.rs", 5), ("login", "src/b.rs", 4)]),
        cluster_neighbors: vec![("src/near.rs".into(), "same cluster".into())],
        graph_available: true,
        ..Default::default()
    }
}

// --- tokenize_task ------------------------------------------------------------

/// An underscore-joined word is a code identifier: it is probed as an exact
/// symbol name even without backticks, once, while its parts stay keywords.
#[test]
fn tokenize_task_should_probe_snake_case_words_as_exact_names_once() {
    let q = tokenize_task("fix parse_header then parse_header again").unwrap();
    assert_eq!(q.exact_tokens, vec!["parse_header"]);
    assert_eq!(q.keywords, vec!["parse", "header", "again"]);
}

/// A quoted span is an exact token only when it is an identifier: a span
/// starting with a digit or carrying punctuation stays plain words.
#[test]
fn tokenize_task_should_not_probe_quoted_non_identifiers_as_exact_names() {
    let q = tokenize_task("crash on `9lives` and \"two words\" and 'snake_case'").unwrap();
    assert_eq!(q.exact_tokens, vec!["snake_case"]);
    assert_eq!(
        q.keywords,
        vec!["crash", "9lives", "two", "words", "snake", "case"]
    );
}

/// An unterminated quote does not swallow the words after it.
#[test]
fn tokenize_task_should_keep_words_after_an_unterminated_quote() {
    let q = tokenize_task("rename `cache_entry everywhere").unwrap();
    assert_eq!(q.keywords, vec!["rename", "cache", "entry", "everywhere"]);
    assert_eq!(q.exact_tokens, vec!["cache_entry"]);
}

/// Past the keyword cap, a new distinct word marks the query truncated; a
/// repeat of a word already kept does not.
#[test]
fn tokenize_task_should_flag_truncation_only_for_new_words_past_the_cap() {
    let twelve = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima";
    let q = tokenize_task(&format!("{twelve} alpha lima")).unwrap();
    assert_eq!(q.keywords.len(), MAX_KEYWORDS);
    assert!(!q.keywords_truncated, "repeats past the cap lose nothing");

    let q = tokenize_task(&format!("{twelve} mike")).unwrap();
    assert_eq!(q.keywords.len(), MAX_KEYWORDS);
    assert!(q.keywords_truncated, "a 13th distinct word was dropped");
    assert!(!q.keywords.contains(&"mike".to_string()));
}

// --- compute_targets: tier filter -------------------------------------------

#[test]
fn compute_targets_should_keep_only_p0_and_name_the_cap_when_max_tier_is_p0() {
    let r = compute_targets(
        "t",
        &query(&["login"]),
        three_tiers(),
        &opts(20, Some("P0"), false),
    );
    assert_eq!(listed(&r), vec![("src/login.rs".into(), "P0".into())]);
    assert_eq!(
        caps(&r),
        vec!["max-tier filter: 2 file(s) above tier P0 dropped".to_string()]
    );
    assert_eq!(r.envelope["lower_bound"], true);
    assert!(r.closed_world.contains("max-tier filter: 2 file(s)"));
}

#[test]
fn compute_targets_should_drop_only_p2_when_max_tier_is_p1() {
    let r = compute_targets(
        "t",
        &query(&["login"]),
        three_tiers(),
        &opts(20, Some("P1"), false),
    );
    assert_eq!(
        listed(&r),
        vec![
            ("src/login.rs".into(), "P0".into()),
            ("src/b.rs".into(), "P1".into())
        ]
    );
    assert_eq!(
        caps(&r),
        vec!["max-tier filter: 1 file(s) above tier P1 dropped".to_string()]
    );
}

/// `P2` (like no filter) keeps every tier and fires no cap.
#[test]
fn compute_targets_should_keep_every_tier_when_max_tier_is_p2() {
    let r = compute_targets(
        "t",
        &query(&["login"]),
        three_tiers(),
        &opts(20, Some("P2"), false),
    );
    assert_eq!(r.targets.len(), 3);
    assert!(caps(&r).is_empty());
}

// --- compute_targets: precision mode ------------------------------------------

/// With a P0 present, files scoring under half of the last P0 are noise:
/// they are dropped and the drop is named.
#[test]
fn compute_targets_should_drop_low_scores_below_the_p0_gap_in_precision_mode() {
    let r = compute_targets(
        "t",
        &query(&["login"]),
        three_tiers(),
        &opts(20, None, true),
    );
    assert_eq!(listed(&r), vec![("src/login.rs".into(), "P0".into())]);
    assert_eq!(
        caps(&r),
        vec!["precision mode: 2 low-score P1/P2 file(s) dropped by score-gap cutoff".to_string()]
    );
}

/// Without a P0, a clear winner (second below 70 % of it) keeps only the
/// files near the top.
#[test]
fn compute_targets_should_keep_the_clear_winner_when_no_p0_in_precision_mode() {
    let inputs = SignalInputs {
        all_paths: paths(&["src/login.rs", "src/other.rs"]),
        content_hits: content(&[("session", "src/other.rs", 3)]),
        graph_available: true,
        ..Default::default()
    };
    let r = compute_targets(
        "t",
        &query(&["login", "session"]),
        inputs,
        &opts(20, None, true),
    );
    assert_eq!(listed(&r), vec![("src/login.rs".into(), "P1".into())]);
    assert_eq!(
        r.envelope["caps"][0],
        "precision mode: 1 low-score P1/P2 file(s) dropped by score-gap cutoff"
    );
}

/// Without a P0 and without a clear winner, precision mode drops nothing.
#[test]
fn compute_targets_should_keep_close_scores_when_no_p0_in_precision_mode() {
    let inputs = SignalInputs {
        all_paths: paths(&["src/a.rs", "src/b.rs"]),
        content_hits: content(&[("token", "src/a.rs", 3), ("token", "src/b.rs", 3)]),
        graph_available: true,
        ..Default::default()
    };
    let r = compute_targets("t", &query(&["token"]), inputs, &opts(20, None, true));
    assert_eq!(r.targets.len(), 2);
    assert!(caps(&r).is_empty());
    assert_eq!(r.envelope["lower_bound"], false);
}

// --- compute_targets: caps and envelope ---------------------------------------

/// Candidates past the limit are never silently absorbed: the cap names
/// the limit and how many were cut.
#[test]
fn compute_targets_should_name_the_limit_cap_when_candidates_overflow() {
    let r = compute_targets(
        "t",
        &query(&["login"]),
        three_tiers(),
        &opts(1, None, false),
    );
    assert_eq!(r.targets.len(), 1);
    assert_eq!(
        caps(&r),
        vec!["target list truncated at limit 1: 2 scored candidate file(s) beyond it".to_string()]
    );
    assert_eq!(r.stats["limit"], 1);
}

/// A limit of zero is raised to one file, and one above the maximum is
/// lowered to it: the caller never gets an empty or unbounded list.
#[test]
fn compute_targets_should_clamp_the_limit_into_its_range() {
    let r = compute_targets(
        "t",
        &query(&["login"]),
        three_tiers(),
        &opts(0, None, false),
    );
    assert_eq!(r.stats["limit"], 1);
    let r = compute_targets(
        "t",
        &query(&["login"]),
        three_tiers(),
        &opts(10_000, None, false),
    );
    assert_eq!(r.stats["limit"], MAX_LIMIT);
}

/// A query whose keywords were truncated cannot back an exhaustive claim.
#[test]
fn compute_targets_should_name_keyword_truncation_as_a_cap() {
    let mut q = query(&["login"]);
    q.keywords_truncated = true;
    let r = compute_targets("t", &q, three_tiers(), &TargetsOptions::default());
    assert_eq!(
        caps(&r),
        vec![format!(
            "task keywords truncated at {MAX_KEYWORDS}; later task words contributed no signal"
        )]
    );
    assert_eq!(r.envelope["lower_bound"], true);
}

/// Unresolved calls sharing a matched name make the answer a lower bound,
/// and the note says how many.
#[test]
fn compute_targets_should_explain_unresolved_callers_in_the_note() {
    let mut inputs = three_tiers();
    inputs.envelope = Some(pixel_graph::Envelope {
        lower_bound: true,
        unresolved_same_name: 3,
    });
    let r = compute_targets("t", &query(&["login"]), inputs, &TargetsOptions::default());
    let note =
        "3 unresolved call site(s) share a matched symbol name; callers beyond this list may exist";
    assert_eq!(r.envelope["note"], note);
    assert_eq!(r.envelope["graph"], "fresh");
    assert_eq!(r.envelope["unresolved_same_name"], 3);
    assert!(r.closed_world.contains(note));
}

/// Only a fresh graph with no cap fired lets the report claim exhaustiveness.
#[test]
fn compute_targets_should_claim_exhaustive_only_when_nothing_was_capped() {
    let r = compute_targets(
        "t",
        &query(&["login"]),
        three_tiers(),
        &TargetsOptions::default(),
    );
    assert_eq!(r.envelope["lower_bound"], false);
    assert_eq!(r.envelope["note"], "");
    assert!(
        r.closed_world
            .ends_with("This list is exhaustive for the indexed tree.")
    );
}

// --- compute_targets: symbol evidence -----------------------------------------

fn symbol(path: &str, name: &str) -> (SymbolRow, String) {
    (
        SymbolRow {
            id: 1,
            uid: format!("{path}#{name}#function"),
            file_id: 1,
            name: name.to_string(),
            qualified: name.to_string(),
            kind: SymbolKind::Function,
            start_line: 1,
            end_line: 2,
            sig: String::new(),
        },
        name.to_string(),
    )
}

/// A file defining many matching symbols names three and counts the rest;
/// only a P0 file keeps the structured symbol list.
#[test]
fn compute_targets_should_summarise_extra_symbols_and_keep_structured_ones_for_p0_only() {
    let names = ["login_a", "login_b", "login_c", "login_d", "login_e"];
    let hits = vec![
        SymbolHit {
            path: "src/core.rs".into(),
            symbols: names.iter().map(|n| symbol("src/core.rs", n)).collect(),
            distinct_keywords: 1,
            exact_name_hit: true,
        },
        SymbolHit {
            path: "src/util.rs".into(),
            symbols: vec![symbol("src/util.rs", "login_helper")],
            distinct_keywords: 1,
            exact_name_hit: false,
        },
    ];
    let inputs = SignalInputs {
        all_paths: paths(&["src/core.rs", "src/util.rs"]),
        symbol_hits: hits,
        graph_available: true,
        ..Default::default()
    };
    let r = compute_targets("t", &query(&["login"]), inputs, &TargetsOptions::default());
    assert_eq!(
        listed(&r),
        vec![
            ("src/core.rs".into(), "P0".into()),
            ("src/util.rs".into(), "P1".into())
        ]
    );
    let core = &r.targets[0];
    assert_eq!(
        core.reasons,
        vec![
            "defines symbol `login_a`".to_string(),
            "defines symbol `login_b`".to_string(),
            "defines symbol `login_c`".to_string(),
            "+2 more matching symbols".to_string(),
        ]
    );
    assert_eq!(core.symbols.len(), 3, "P0 keeps the first three structured");
    assert!(r.targets[1].symbols.is_empty(), "P1 carries reasons only");
}
