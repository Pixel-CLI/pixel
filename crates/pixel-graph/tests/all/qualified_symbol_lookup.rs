// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `GraphStore::symbols_named`: the lookup every user-supplied name reaches,
//! with its qualified fallback (`ActionLog::finish`, `Builder.finish`).
//!
//! The daemon tests prove `impact`, `uses` and `find-symbol` answer a
//! qualified name; this file pins the matching rule itself — which stored
//! qualified names a given input selects and which it must not.

use pixel_graph::store::{GraphStore, SymbolKind, qualified_leaf};

/// `(path, name, qualified)` rows, one method symbol each.
fn store_with(rows: &[(&str, &str, &str)]) -> GraphStore {
    let mut store = GraphStore::open_in_memory().unwrap();
    for (index, (path, name, qualified)) in rows.iter().enumerate() {
        let file_id = store
            .replace_file(path, &format!("oid{index}"), "ts")
            .unwrap();
        store
            .insert_symbol(
                file_id,
                &format!("{path}#{qualified}#method"),
                name,
                qualified,
                SymbolKind::Method,
                1,
                1,
                "",
            )
            .unwrap();
    }
    store
}

fn uids(store: &GraphStore, name: &str, scope: Option<&str>) -> Vec<String> {
    let mut found: Vec<String> = store
        .symbols_named(name, scope, 50)
        .unwrap()
        .into_iter()
        .map(|row| row.uid)
        .collect();
    found.sort();
    found
}

fn fixture() -> GraphStore {
    store_with(&[
        ("src/log.rs", "finish", "ActionLog::finish"),
        ("src/hash.rs", "finish", "Hasher::finish"),
        ("src/notice.rs", "finish", "finish"),
        ("tools/extract.py", "finish", "Builder.finish"),
        ("app/models/plan.rb", "run", "Billing::Plan.run"),
    ])
}

#[test]
fn a_rust_qualified_name_selects_its_owner_and_no_other_homonym() {
    let store = fixture();
    assert_eq!(
        uids(&store, "ActionLog::finish", None),
        vec!["src/log.rs#ActionLog::finish#method"]
    );
}

/// `::` and `.` are one separator: the agent's spelling need not match the
/// language's.
#[test]
fn either_separator_matches_either_stored_spelling() {
    let store = fixture();
    let python = vec!["tools/extract.py#Builder.finish#method".to_string()];
    assert_eq!(uids(&store, "Builder.finish", None), python);
    assert_eq!(uids(&store, "Builder::finish", None), python);
    assert_eq!(
        uids(&store, "ActionLog.finish", None),
        vec!["src/log.rs#ActionLog::finish#method"]
    );
    let ruby = vec!["app/models/plan.rb#Billing::Plan.run#method".to_string()];
    assert_eq!(uids(&store, "Plan.run", None), ruby);
    assert_eq!(uids(&store, "Billing::Plan::run", None), ruby);
}

/// A module path the graph does not store still reaches the method, as long
/// as the stored name itself has an owner.
#[test]
fn a_longer_input_matches_a_stored_qualified_suffix_but_never_a_bare_one() {
    let store = fixture();
    assert_eq!(
        uids(&store, "pixel_actionlog::ActionLog::finish", None),
        vec!["src/log.rs#ActionLog::finish#method"]
    );
    // `notice::finish` would otherwise pick the free function `finish` by
    // its leaf alone, a match on one segment that names nothing.
    assert_eq!(uids(&store, "notice::finish", None), Vec::<String>::new());
}

/// Segments match whole: `Log` is not the tail of `ActionLog`.
#[test]
fn a_partial_owner_segment_matches_nothing() {
    let store = fixture();
    assert_eq!(uids(&store, "Log::finish", None), Vec::<String>::new());
    assert_eq!(uids(&store, "ctionLog::finish", None), Vec::<String>::new());
    assert_eq!(uids(&store, "Other::finish", None), Vec::<String>::new());
}

#[test]
fn an_ambiguous_qualified_name_returns_every_match() {
    let store = store_with(&[
        ("a/receipt.rs", "finish", "Snapshot::finish"),
        ("b/receipt.rs", "finish", "Snapshot::finish"),
        ("c/other.rs", "finish", "Other::finish"),
    ]);
    assert_eq!(
        uids(&store, "Snapshot::finish", None),
        vec![
            "a/receipt.rs#Snapshot::finish#method",
            "b/receipt.rs#Snapshot::finish#method",
        ]
    );
    // The scope narrows a qualified lookup in the query, as it does a bare one.
    assert_eq!(
        uids(&store, "Snapshot::finish", Some("b")),
        vec!["b/receipt.rs#Snapshot::finish#method"]
    );
}

/// The fallback runs only when the name itself matches nothing: a stored
/// name that contains a dot keeps answering as itself.
#[test]
fn a_bare_hit_wins_over_the_qualified_reading() {
    let store = store_with(&[
        ("a.js", "config.load", "config.load"),
        ("b.js", "load", "config.load"),
    ]);
    assert_eq!(
        uids(&store, "config.load", None),
        vec!["a.js#config.load#method"]
    );
    assert_eq!(uids(&store, "finish", None), Vec::<String>::new());
}

#[test]
fn qualified_leaf_needs_two_non_empty_segments() {
    assert_eq!(qualified_leaf("ActionLog::finish"), Some("finish"));
    assert_eq!(qualified_leaf("a::b.c"), Some("c"));
    assert_eq!(qualified_leaf("Builder.finish"), Some("finish"));
    for name in [
        "finish", "", "Foo.", "Foo::", "::finish", ".finish", "a..b", "a::::b",
    ] {
        assert_eq!(qualified_leaf(name), None, "{name:?}");
    }
}
