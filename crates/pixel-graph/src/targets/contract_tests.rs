// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Contract tests for the graph signals `pixel targets` fuses: symbol hit
//! ranking and caps, and the deduplicated, seed-free neighbour lists.

use super::*;
use crate::store::{EdgeKind, EdgeRow, GraphStore, SymbolKind, Tier};

fn sym(store: &GraphStore, file_id: i64, file: &str, name: &str) -> i64 {
    store
        .insert_symbol(
            file_id,
            &format!("{file}#{name}#function"),
            name,
            name,
            SymbolKind::Function,
            1,
            5,
            "",
        )
        .unwrap()
}

fn call(store: &GraphStore, src: i64, dst: i64) {
    store
        .insert_edge(&EdgeRow {
            src_id: src,
            dst_id: dst,
            kind: EdgeKind::Calls,
            tier: Tier::Exact,
            site_line: 1,
            receiver: None,
            callee: None,
        })
        .unwrap();
}

fn kws(words: &[&str]) -> Vec<String> {
    words.iter().map(ToString::to_string).collect()
}

/// No keyword and no exact token means no symbol hit, whatever is stored.
#[test]
fn symbol_hits_should_be_empty_without_keywords_or_exact_tokens() {
    let mut store = GraphStore::open_in_memory().unwrap();
    let f = store.replace_file("src/a.rs", "oid", "rs").unwrap();
    sym(&store, f, "src/a.rs", "login");
    assert!(symbol_hits(&store, &[], &[]).unwrap().is_empty());
}

/// Exact hits rank first, then more distinct keywords, then more matched
/// symbols, then path.
#[test]
fn symbol_hits_should_rank_exact_then_keywords_then_symbol_count_then_path() {
    let mut store = GraphStore::open_in_memory().unwrap();
    let files = [
        "src/z_exact.rs",
        "src/two_kw.rs",
        "src/many.rs",
        "src/b_one.rs",
        "src/a_one.rs",
    ];
    let ids: Vec<i64> = files
        .iter()
        .map(|p| store.replace_file(p, "oid", "rs").unwrap())
        .collect();
    sym(&store, ids[0], files[0], "parse_token");
    sym(&store, ids[1], files[1], "login_session");
    sym(&store, ids[2], files[2], "login_a");
    sym(&store, ids[2], files[2], "login_b");
    sym(&store, ids[3], files[3], "login_x");
    sym(&store, ids[4], files[4], "login_y");

    let hits = symbol_hits(&store, &kws(&["login", "session"]), &kws(&["parse_token"])).unwrap();
    let order: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
    assert_eq!(
        order,
        vec![
            "src/z_exact.rs",
            "src/two_kw.rs",
            "src/many.rs",
            "src/a_one.rs",
            "src/b_one.rs"
        ]
    );
    assert_eq!(
        hits[0].symbols[0].1, "parse_token",
        "an exact hit names itself"
    );
    assert_eq!(hits[1].distinct_keywords, 2);
}

/// A file keeps at most five matched symbols, while its distinct keywords
/// still count every match.
#[test]
fn symbol_hits_should_cap_symbols_per_file_at_five() {
    let mut store = GraphStore::open_in_memory().unwrap();
    let f = store.replace_file("src/a.rs", "oid", "rs").unwrap();
    for i in 0..7 {
        sym(&store, f, "src/a.rs", &format!("login_{i}"));
    }
    sym(&store, f, "src/a.rs", "session_end");
    let hits = symbol_hits(&store, &kws(&["login", "session"]), &[]).unwrap();
    assert_eq!(hits[0].symbols.len(), MAX_SYMBOLS_PER_FILE);
    assert_eq!(hits[0].distinct_keywords, 2);
    assert_eq!(
        hits[0].symbols[0].1, "login",
        "the matched keyword is recorded"
    );
}

/// A neighbour reached from two seeds or two directions is listed once,
/// with its first reason; neighbours in a seed file and unknown seeds are
/// dropped.
#[test]
fn neighbor_files_should_dedupe_and_skip_seed_files_and_unknown_seeds() {
    let mut store = GraphStore::open_in_memory().unwrap();
    let fa = store.replace_file("src/a.ts", "oid", "ts").unwrap();
    let fb = store.replace_file("src/b.ts", "oid", "ts").unwrap();
    let a1 = sym(&store, fa, "src/a.ts", "alpha");
    let a2 = sym(&store, fa, "src/a.ts", "alpha_two");
    let b = sym(&store, fb, "src/b.ts", "beta");
    call(&store, b, a1);
    call(&store, a1, b);
    call(&store, a2, b);
    call(&store, a1, a2);
    let nbrs = neighbor_files(&store, &[a1, a2, 9_999]).unwrap();
    assert_eq!(
        nbrs,
        vec![("src/b.ts".to_string(), "caller of `alpha`".to_string())]
    );
}

/// A seed path the graph does not know adds nothing; a file both imported
/// by and importing the seed is listed once.
#[test]
fn import_adjacent_files_should_skip_unknown_seeds_and_dedupe() {
    let mut store = GraphStore::open_in_memory().unwrap();
    let fa = store.replace_file("src/a.ts", "oid", "ts").unwrap();
    let fb = store.replace_file("src/b.ts", "oid", "ts").unwrap();
    store.insert_import(fa, "./b", Some(fb), &[]).unwrap();
    store.insert_import(fb, "./a", Some(fa), &[]).unwrap();
    store.insert_import(fa, "./a", Some(fa), &[]).unwrap();
    let adj = import_adjacent_files(&store, &["src/a.ts".into(), "src/missing.ts".into()]).unwrap();
    assert_eq!(
        adj,
        vec![("src/b.ts".to_string(), "imported by src/a.ts".to_string())]
    );
}

/// Without a keyword overlap the reason names only the cluster; a file in
/// two of the seed's clusters is listed once.
#[test]
fn cluster_co_files_should_name_the_cluster_without_overlap_and_dedupe() {
    let mut store = GraphStore::open_in_memory().unwrap();
    let fa = store.replace_file("src/a.ts", "oid", "ts").unwrap();
    let fb = store.replace_file("src/b.ts", "oid", "ts").unwrap();
    let a = sym(&store, fa, "src/a.ts", "alpha");
    let b = sym(&store, fb, "src/b.ts", "beta");
    let conn = store.conn();
    conn.execute(
        "INSERT INTO clusters (id, label, cohesion, keywords) VALUES (1, 'net', 0.9, 'socket, , http')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO clusters (id, label, cohesion, keywords) VALUES (2, 'io', 0.8, 'socket')",
        [],
    )
    .unwrap();
    for (cid, sid) in [(1, a), (1, b), (2, a), (2, b)] {
        conn.execute(
            "INSERT INTO cluster_members (cluster_id, symbol_id) VALUES (?1, ?2)",
            params![cid, sid],
        )
        .unwrap();
    }
    let co = cluster_co_files(&store, &[a], &kws(&["login", ""])).unwrap();
    assert_eq!(
        co,
        vec![("src/b.ts".to_string(), "same cluster 'net'".to_string())]
    );
    let co = cluster_co_files(&store, &[a], &kws(&["http"])).unwrap();
    assert_eq!(
        co,
        vec![(
            "src/b.ts".to_string(),
            "same cluster 'net' (cluster keywords match task)".to_string()
        )]
    );
}
