// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `GraphStore::symbols_by_name` with a scope: the store-level contract that
//! `pixel evaluate --in` rests on.
//!
//! These live beside the daemon's end-to-end resolution tests rather than
//! instead of them. The daemon proves the command answers correctly; this
//! file proves the query does, which is the only level where the SQL —
//! its column list, its cap and its path boundary — is the thing under
//! test.

use pixel_graph::store::{GraphStore, SymbolKind};

/// A store holding one symbol called `name` in each of `paths`.
fn store_with(name: &str, paths: &[&str]) -> GraphStore {
    let mut store = GraphStore::open_in_memory().unwrap();
    for (index, path) in paths.iter().enumerate() {
        let file_id = store
            .replace_file(path, &format!("oid{index}"), "ts")
            .unwrap();
        store
            .insert_symbol(
                file_id,
                &format!("{path}#{name}#function"),
                name,
                name,
                SymbolKind::Function,
                1,
                1,
                "",
            )
            .unwrap();
    }
    store
}

fn paths_of(store: &GraphStore, name: &str, scope: &str, limit: u32) -> Vec<String> {
    let mut found: Vec<String> = store
        .symbols_by_name(name, Some(scope), limit)
        .unwrap()
        .into_iter()
        .map(|row| {
            store
                .file_by_id(row.file_id)
                .unwrap()
                .expect("every returned symbol belongs to a file")
                .path
        })
        .collect();
    found.sort();
    found
}

/// The scope selects the rows inside it, and only those.
#[test]
fn a_scoped_lookup_returns_the_rows_under_the_scope_and_no_others() {
    let store = store_with("helper", &["src/a.ts", "vendor/a.ts"]);

    assert_eq!(paths_of(&store, "helper", "src", 50), vec!["src/a.ts"]);
    assert_eq!(
        paths_of(&store, "helper", "vendor", 50),
        vec!["vendor/a.ts"]
    );
    assert!(
        paths_of(&store, "helper", "nowhere", 50).is_empty(),
        "a scope that matches no file selects nothing"
    );
}

/// Why the method exists: the cap must not decide what the scope contains.
///
/// Filtering a capped page in the caller reads the first `limit` rows for
/// the name and keeps the scoped ones. When the homonyms outside the scope
/// fill that page, the scoped symbol never comes back: the caller sees an
/// absence for a symbol that plainly exists. Worse, had a second scoped
/// symbol existed beyond the page, the survivor would have looked unique
/// and the answer would have been attributed to the wrong symbol while
/// claiming to be exact.
#[test]
fn a_scoped_lookup_is_not_decided_by_the_cap_it_shares_with_the_name() {
    let limit = 50;
    // `lib/` sorts before `src/`, so these fill the page a caller-side
    // filter would have had to work from.
    let mut paths: Vec<String> = (0..limit + 5).map(|i| format!("lib/h{i:03}.ts")).collect();
    paths.push("src/a.ts".to_string());
    let borrowed: Vec<&str> = paths.iter().map(String::as_str).collect();
    let store = store_with("helper", &borrowed);

    assert_eq!(
        paths_of(&store, "helper", "src", limit as u32),
        vec!["src/a.ts"],
        "the scoped symbol must come back however many homonyms sit outside the scope"
    );
}

/// A scope is a directory, not a leading substring.
#[test]
fn a_scoped_lookup_excludes_a_sibling_that_merely_shares_a_prefix() {
    let store = store_with("helper", &["src/foo/x.ts", "src/foobar.ts", "src/foo.ts"]);

    assert_eq!(
        paths_of(&store, "helper", "src/foo", 50),
        vec!["src/foo/x.ts"],
        "the directory and its descendants; `src/foobar.ts` and `src/foo.ts` \
         are siblings that merely share the letters"
    );
}

/// The cap still applies inside the scope, so a caller can bound the work.
#[test]
fn a_scoped_lookup_still_honours_its_cap() {
    let paths: Vec<String> = (0..6).map(|i| format!("src/h{i}.ts")).collect();
    let borrowed: Vec<&str> = paths.iter().map(String::as_str).collect();
    let store = store_with("helper", &borrowed);

    assert_eq!(paths_of(&store, "helper", "src", 4).len(), 4);
}
