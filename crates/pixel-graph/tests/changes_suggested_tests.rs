//! Integration tests for `changes::detect` affected-test mapping
//! (`suggested_tests`): test files reached by walking UPSTREAM callers of
//! changed symbols, plus changed test files themselves.

use std::path::{Path, PathBuf};

use pixel_graph::changes::detect;
use pixel_graph::store::{EdgeKind, EdgeRow, GraphStore, SymbolKind, Tier};

fn tmpdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pixel-graph-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// Five-line body so symbols spanning lines 1-5 overlap an edit on line 2.
fn body(tag: &str, salt: u32) -> String {
    format!("// {tag} l1 s{salt}\n// l2\n// l3\n// l4\n// l5\n")
}

fn sym(store: &GraphStore, file_id: i64, path: &str, name: &str) -> i64 {
    store
        .insert_symbol(
            file_id,
            &format!("{path}#{name}#function"),
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
            site_line: 2,
            receiver: None,
        })
        .unwrap();
}

/// Fixture: a git repo whose baseline commit contains
///   src/alpha.ts        — `alpha` (will be changed)
///   src/mid.ts          — `mid` calls alpha
///   tests/alpha.test.ts — `testAlpha` calls alpha       (depth 1)
///   tests/mid.test.ts   — `testMid` calls mid           (depth 2)
///   tests/own.test.ts   — `ownTest` (the file will be changed itself)
/// then modifies src/alpha.ts and tests/own.test.ts in the working tree,
/// and a hand-built in-memory graph mirroring those files and edges.
fn fixture(name: &str) -> (PathBuf, GraphStore) {
    let root = tmpdir(name);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    let files = [
        "src/alpha.ts",
        "src/mid.ts",
        "tests/alpha.test.ts",
        "tests/mid.test.ts",
        "tests/own.test.ts",
    ];
    for f in files {
        std::fs::write(root.join(f), body(f, 1)).unwrap();
    }
    git(&root, &["init", "-q"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "baseline"]);
    // Working-tree changes: alpha's file and a test file itself.
    std::fs::write(root.join("src/alpha.ts"), body("src/alpha.ts", 2)).unwrap();
    std::fs::write(root.join("tests/own.test.ts"), body("tests/own.test.ts", 2)).unwrap();

    let mut store = GraphStore::open_in_memory().unwrap();
    let mut ids = std::collections::HashMap::new();
    for f in files {
        let fid = store.replace_file(f, "oid", "ts").unwrap();
        ids.insert(f, fid);
    }
    let alpha = sym(&store, ids["src/alpha.ts"], "src/alpha.ts", "alpha");
    let mid = sym(&store, ids["src/mid.ts"], "src/mid.ts", "mid");
    let test_alpha = sym(
        &store,
        ids["tests/alpha.test.ts"],
        "tests/alpha.test.ts",
        "testAlpha",
    );
    let test_mid = sym(&store, ids["tests/mid.test.ts"], "tests/mid.test.ts", "testMid");
    let _own = sym(&store, ids["tests/own.test.ts"], "tests/own.test.ts", "ownTest");
    call(&store, mid, alpha); // mid -> alpha
    call(&store, test_alpha, alpha); // testAlpha -> alpha (depth 1)
    call(&store, test_mid, mid); // testMid -> mid -> alpha (depth 2)
    (root, store)
}

#[test]
fn changes_suggests_direct_caller_transitive_and_changed_test() {
    let (root, store) = fixture("sugg-full");
    let report = detect(&store, &root, None, true).unwrap();

    let names: Vec<&str> = report.symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"alpha"), "changed symbols: {names:?}");
    assert!(names.contains(&"ownTest"), "changed symbols: {names:?}");

    assert_eq!(
        report.suggested_tests.len(),
        3,
        "suggested: {:?}",
        report.suggested_tests
    );

    // Sorted by (depth, file): depth 0 changed test first.
    let t0 = &report.suggested_tests[0];
    assert_eq!(t0.file, "tests/own.test.ts");
    assert_eq!(t0.depth, 0);
    assert_eq!(t0.via, "direct");
    assert_eq!(t0.matched_symbols, vec!["ownTest".to_string()]);

    // Depth 1: a test symbol directly calls the changed symbol.
    let t1 = &report.suggested_tests[1];
    assert_eq!(t1.file, "tests/alpha.test.ts");
    assert_eq!(t1.depth, 1);
    assert_eq!(t1.via, "direct-caller");
    assert_eq!(t1.matched_symbols, vec!["alpha".to_string()]);

    // Depth 2: testMid -> mid -> alpha.
    let t2 = &report.suggested_tests[2];
    assert_eq!(t2.file, "tests/mid.test.ts");
    assert_eq!(t2.depth, 2);
    assert_eq!(t2.via, "transitive");
    assert_eq!(t2.matched_symbols, vec!["alpha".to_string()]);

    assert!(!report.suggested_tests_lower_bound);
    // No .rs files changed, nothing truncated → no caveats.
    assert!(
        report.suggested_tests_note.is_empty(),
        "note: {}",
        report.suggested_tests_note
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn changes_include_tests_false_yields_empty_suggestions() {
    let (root, store) = fixture("sugg-off");
    let report = detect(&store, &root, None, false).unwrap();

    // Change detection itself still works…
    assert!(
        report.symbols.iter().any(|s| s.name == "alpha"),
        "symbols: {:?}",
        report.symbols
    );
    // …but the test mapping is gated off.
    assert!(report.suggested_tests.is_empty());
    assert!(!report.suggested_tests_lower_bound);
    assert!(report.suggested_tests_note.is_empty());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn changes_rust_file_surfaces_extraction_caveat() {
    // A changed .rs symbol must produce the honest note that Rust #[test] /
    // #[cfg(test)] symbols are absent from the graph (extraction skips test
    // containers), instead of silently suggesting nothing.
    let root = tmpdir("sugg-rs-note");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), body("src/lib.rs", 1)).unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "baseline"]);
    std::fs::write(root.join("src/lib.rs"), body("src/lib.rs", 2)).unwrap();

    let mut store = GraphStore::open_in_memory().unwrap();
    let fid = store.replace_file("src/lib.rs", "oid", "rust").unwrap();
    sym(&store, fid, "src/lib.rs", "alpha");

    let report = detect(&store, &root, None, true).unwrap();
    assert!(report.symbols.iter().any(|s| s.name == "alpha"));
    assert!(
        report.suggested_tests_note.contains("#[cfg(test)]"),
        "note: {}",
        report.suggested_tests_note
    );

    let _ = std::fs::remove_dir_all(&root);
}
