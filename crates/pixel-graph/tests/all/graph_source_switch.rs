// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The Rust source switch (`PIXEL_GRAPH_SOURCE_RUST=mvp`): on the adapter's
//! fixture (`mvp_adapter.rs`), what the graph holds with it on and off,
//! through the full build and through each incremental route that writes
//! rows (`update_file`, `apply_tree_delta`).

use std::fs;
use std::path::{Path, PathBuf};

use pixel_graph::build::{
    apply_tree_delta, build_graph, build_graph_from, tree_delta, update_file,
};
use pixel_graph::mvp_adapter::{GRAPH_SOURCE_KEY, MVP_UNAVAILABLE, RustSource, mvp_source};
use pixel_graph::{EdgeKind, GraphStore, Tier};

/// The fixture, built with `rust` as the Rust source.
fn built(rust: RustSource) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    crate::mvp_adapter::fixture(&root);
    let db = root.join(".pixel/graph.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    build_graph_from(&root, &db, rust).unwrap();
    (dir, root, db)
}

fn id(store: &GraphStore, path: &str, qualified: &str) -> i64 {
    store
        .conn()
        .query_row(
            "SELECT s.id FROM symbols s JOIN files f ON f.id = s.file_id
              WHERE f.path = ?1 AND s.qualified = ?2",
            [path, qualified],
            |r| r.get(0),
        )
        .unwrap()
}

/// `(caller, callee, kind, tier, line)` of every edge, by symbol uid.
fn edges(db: &Path) -> Vec<String> {
    let store = GraphStore::open(db).unwrap();
    let mut stmt = store
        .conn()
        .prepare(
            "SELECT a.uid, b.uid, e.kind, e.tier, e.site_line, e.receiver, e.callee FROM edges e
               JOIN symbols a ON a.id = e.src_id JOIN symbols b ON b.id = e.dst_id",
        )
        .unwrap();
    let mut out: Vec<String> = stmt
        .query_map([], |r| {
            Ok(format!(
                "{} -> {} {} {} {} {:?} {:?}",
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, u32>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    out.sort();
    out
}

/// Every row a resolution decision writes: edges, unresolved rows, import
/// targets, and the source the graph records.
fn decisions(db: &Path) -> Vec<String> {
    let store = GraphStore::open(db).unwrap();
    let mut out = edges(db);
    for sql in [
        "SELECT f.path || ' ' || u.name || ' ' || COALESCE(s.uid, '-') || ' ' || u.site_line
                || ' ' || COALESCE(u.receiver, '-') || ' ' || u.kind
           FROM unresolved_calls u JOIN files f ON f.id = u.file_id
           LEFT JOIN symbols s ON s.id = u.enclosing_symbol_id",
        "SELECT f.path || ' ' || i.path || ' -> ' || COALESCE(t.path, '-') FROM imports i
           JOIN files f ON f.id = i.file_id LEFT JOIN files t ON t.id = i.resolved_file_id",
        "SELECT key || '=' || value FROM meta WHERE key = 'graph_source'",
    ] {
        let mut stmt = store.conn().prepare(sql).unwrap();
        let rows: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        out.extend(rows);
    }
    out.sort();
    out
}

#[test]
fn with_the_switch_on_path_join_is_no_longer_linked_to_a_repo_join() {
    let (_dir, _root, db) = built(RustSource::Mvp);
    let store = GraphStore::open(&db).unwrap();
    let run = id(&store, "src/lib.rs", "run");
    let join = id(&store, "src/paths.rs", "join");
    let open = id(&store, "src/store.rs", "open");
    let from_run = store.edges_from(run, None).unwrap();
    assert!(from_run.iter().all(|e| e.dst_id != join), "{from_run:?}");
    // A proven call is exact, with the name the site wrote.
    assert!(
        from_run.iter().any(|e| e.dst_id == open
            && e.kind == EdgeKind::Calls
            && e.tier == Tier::Exact
            && e.site_line == 10),
        "{from_run:?}"
    );
    // The site is counted unresolved, not lost.
    let unresolved: u32 = store
        .conn()
        .query_row(
            "SELECT count(*) FROM unresolved_calls WHERE name = 'join' AND site_line = 9
               AND enclosing_symbol_id = ?1 AND receiver = '.p'",
            [run],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(unresolved, 1);
    assert_eq!(
        store.meta_get(GRAPH_SOURCE_KEY).unwrap(),
        Some(mvp_source())
    );
}

#[test]
fn with_the_switch_on_no_call_crosses_between_rust_and_another_language() {
    let (_dir, _root, db) = built(RustSource::Mvp);
    let store = GraphStore::open(&db).unwrap();
    let crossing: Vec<(String, String)> = {
        let mut stmt = store
            .conn()
            .prepare(
                "SELECT a.uid, b.uid FROM edges e
                   JOIN symbols a ON a.id = e.src_id JOIN files fa ON fa.id = a.file_id
                   JOIN symbols b ON b.id = e.dst_id JOIN files fb ON fb.id = b.file_id
                  WHERE (fa.lang = 'rust') != (fb.lang = 'rust')",
            )
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    // Rust `.unwrap()` and `.count()` reached env.py's functions, and
    // env.py's `count` reached Rust's `helper`, by name alone.
    assert_eq!(crossing, Vec::<(String, String)>::new());

    // The control: this graph's own resolver links all three.
    let (_dir, _root, own) = built(RustSource::V1);
    let own_edges = edges(&own);
    for expected in [
        "src/lib.rs#first#function -> scripts/env.py#unwrap#function",
        "src/lib.rs#run#function -> scripts/env.py#count#function",
        "scripts/env.py#count#function -> src/util.rs#helper#function",
    ] {
        assert!(
            own_edges.iter().any(|e| e.starts_with(expected)),
            "precondition {expected}: {own_edges:#?}"
        );
    }
}

#[test]
fn with_the_switch_off_the_graph_is_this_resolvers_alone() {
    // `build_graph` with the switch unset is `RustSource::V1`: no source
    // recorded, no pixel-mvp state, and every decision equal to an explicit
    // V1 build's, the name-only edges included.
    let (_dir, root, db) = built(RustSource::V1);
    let store = GraphStore::open(&db).unwrap();
    assert_eq!(store.meta_get(GRAPH_SOURCE_KEY).unwrap(), None);
    assert!(!root.join(".pixel/.pixel-mvp").exists());
    if std::env::var_os("PIXEL_GRAPH_SOURCE_RUST").is_none() {
        let again = root.join(".pixel/again.db");
        build_graph(&root, &again).unwrap();
        assert_eq!(decisions(&again), decisions(&db));
    }
    let run = id(&store, "src/lib.rs", "run");
    let join = id(&store, "src/paths.rs", "join");
    assert!(
        store
            .edges_from(run, Some(EdgeKind::Calls))
            .unwrap()
            .iter()
            .any(|e| e.dst_id == join && e.tier == Tier::Probable)
    );
}

#[test]
fn an_incremental_update_with_the_switch_on_stores_what_a_full_build_stores() {
    let (_dir, root, db) = built(RustSource::Mvp);
    // `first` gains a call to `helper`; env.py gains a second call to it;
    // `paths.rs` gains a second `join` (a changed definition reconsiders
    // the edges to that name).
    let lib = fs::read_to_string(root.join("src/lib.rs")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        lib.replace("    v.unwrap()", "    v.unwrap() + helper() as u8"),
    )
    .unwrap();
    fs::write(
        root.join("scripts/env.py"),
        "def unwrap(value):\n    return value\n\n\ndef count():\n    return helper() + helper()\n",
    )
    .unwrap();
    fs::write(
        root.join("src/paths.rs"),
        "pub fn join(a: &str) -> String {\n    a.to_string()\n}\npub fn join_all() -> String {\n    join(\"x\")\n}\n",
    )
    .unwrap();
    for rel in ["src/lib.rs", "scripts/env.py"] {
        update_file(&root, &db, rel).unwrap();
    }
    // The third through the daemon's route: a tree delta.
    let delta = tree_delta(&root, &db).unwrap();
    if std::env::var_os("PIXEL_GRAPH_SOURCE_RUST").is_none() {
        // With the switch unset in this process, a graph built from
        // pixel-mvp is not current: the next read rebuilds it whole.
        assert!(delta.is_none());
    }
    update_file(&root, &db, "src/paths.rs").unwrap();

    let reference = root.join(".pixel/reference.db");
    build_graph_from(&root, &reference, RustSource::Mvp).unwrap();
    assert_eq!(decisions(&db), decisions(&reference));
    let store = GraphStore::open(&db).unwrap();
    let first = id(&store, "src/lib.rs", "first");
    let helper = id(&store, "src/util.rs", "helper");
    assert!(
        store
            .edges_from(first, Some(EdgeKind::Calls))
            .unwrap()
            .iter()
            .any(|e| e.dst_id == helper && e.tier == Tier::Exact && e.site_line == 15)
    );
    // Python still reaches no Rust symbol after the update retried it.
    let count = id(&store, "scripts/env.py", "count");
    let from_count = store.edges_from(count, None).unwrap();
    assert!(from_count.is_empty(), "{from_count:?}");
}

#[test]
fn a_tree_delta_with_the_switch_on_keeps_pixel_mvps_rows() {
    if std::env::var_os("PIXEL_GRAPH_SOURCE_RUST").is_some() {
        return;
    }
    // `apply_tree_delta` is the daemon's route; with the switch unset here
    // the delta is withheld (above), so it is driven by hand.
    let (_dir, root, db) = built(RustSource::Mvp);
    let store = GraphStore::open(&db).unwrap();
    let before = decisions(&db);
    drop(store);
    let lib = fs::read_to_string(root.join("src/lib.rs")).unwrap();
    fs::write(root.join("src/lib.rs"), format!("{lib}\n")).unwrap();
    let hash = xxhash(&fs::read(root.join("src/lib.rs")).unwrap());
    let delta = pixel_graph::build::TreeDelta {
        fresh: false,
        changed: vec![("src/lib.rs".to_string(), hash)],
        removed: Vec::new(),
        indexed_files: 0,
        signature: "test".to_string(),
    };
    apply_tree_delta(&root, &db, &delta).unwrap();
    assert_eq!(decisions(&db), before);
}

fn xxhash(bytes: &[u8]) -> u64 {
    u64::from_str_radix(&pixel_graph::build::content_oid(bytes), 16).unwrap()
}

#[test]
fn outside_a_git_work_tree_the_switch_keeps_this_resolver_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fx\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn a() -> u8 {\n    b()\n}\npub fn b() -> u8 {\n    1\n}\n",
    )
    .unwrap();
    let db = root.join(".pixel/graph.db");
    build_graph_from(&root, &db, RustSource::Mvp).unwrap();
    let own = root.join(".pixel/own.db");
    build_graph_from(&root, &own, RustSource::V1).unwrap();
    let store = GraphStore::open(&db).unwrap();
    assert_eq!(
        store.meta_get(GRAPH_SOURCE_KEY).unwrap().as_deref(),
        Some(MVP_UNAVAILABLE)
    );
    assert_eq!(edges(&db), edges(&own));
    assert_eq!(edges(&db).len(), 1, "a -> b");
}

#[test]
fn a_subdirectory_of_a_work_tree_is_not_indexed_from_its_top_level() {
    let dir = tempfile::tempdir().unwrap();
    let top = dir.path().canonicalize().unwrap();
    crate::mvp_adapter::git(&top, &["init", "-q"]);
    let root = top.join("sub");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fx\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    let db = root.join(".pixel/graph.db");
    build_graph_from(&root, &db, RustSource::Mvp).unwrap();
    let store = GraphStore::open(&db).unwrap();
    assert_eq!(
        store.meta_get(GRAPH_SOURCE_KEY).unwrap().as_deref(),
        Some(MVP_UNAVAILABLE)
    );
}

#[test]
fn a_tracked_pixel_directory_is_refused_before_pixel_mvp_reads_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    crate::mvp_adapter::fixture(&root);
    // A clone that ships `.pixel/.pixel-mvp/graph.db`.
    fs::create_dir_all(root.join(".pixel/.pixel-mvp")).unwrap();
    fs::write(root.join(".pixel/.pixel-mvp/graph.db"), "planted").unwrap();
    crate::mvp_adapter::git(&root, &["add", "-f", ".pixel"]);
    crate::mvp_adapter::git(&root, &["commit", "-qm", "ship state"]);
    let err = pixel_graph::mvp_adapter::refresh(&root, &root.join(".pixel"))
        .err()
        .expect("a tracked .pixel is refused");
    assert!(err.to_string().contains(".pixel"), "{err}");
    assert_eq!(
        fs::read_to_string(root.join(".pixel/.pixel-mvp/graph.db")).unwrap(),
        "planted",
        "nothing under it was touched"
    );
}

#[test]
fn a_tree_without_a_cargo_manifest_keeps_this_resolver_and_no_pixel_mvp_state() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    crate::mvp_adapter::fixture(&root);
    fs::remove_file(root.join("Cargo.toml")).unwrap();
    crate::mvp_adapter::git(&root, &["commit", "-qam", "no manifest"]);
    let db = root.join(".pixel/graph.db");
    build_graph_from(&root, &db, RustSource::Mvp).unwrap();
    let own = root.join(".pixel/own.db");
    build_graph_from(&root, &own, RustSource::V1).unwrap();
    let store = GraphStore::open(&db).unwrap();
    assert_eq!(
        store.meta_get(GRAPH_SOURCE_KEY).unwrap().as_deref(),
        Some(MVP_UNAVAILABLE)
    );
    assert_eq!(edges(&db), edges(&own));
    assert!(!root.join(".pixel/.pixel-mvp").exists());
}
