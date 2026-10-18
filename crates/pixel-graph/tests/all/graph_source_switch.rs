// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The Rust source switch (`PIXEL_GRAPH_SOURCE_RUST`, pixel-mvp unless
//! `v1`): on the adapter's
//! fixture (`mvp_adapter.rs`), what the graph holds with it on and off,
//! through the full build and through each incremental route that writes
//! rows (`update_file`, `apply_tree_delta`).

use std::fs;
use std::path::{Path, PathBuf};

use pixel_graph::build::{
    apply_tree_delta, build_graph, build_graph_from, tree_delta, update_file,
};
use pixel_graph::mvp_adapter::{
    GRAPH_SOURCE_KEY, MVP_UNAVAILABLE, RUST_GENERATION_KEY, RustSource, mvp_source, refresh,
};
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
        "SELECT f.path || ' ' || i.path || ' -> '
                || COALESCE(t.path, 'file ' || i.resolved_file_id, '-') FROM imports i
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

/// Counts, from now on, every edge and `unresolved_calls` row written or
/// deleted in `db`, by the path of the file holding its site: triggers the
/// graph's own writes fire, whatever route writes them.
fn count_writes(db: &Path) {
    GraphStore::open(db)
        .unwrap()
        .conn()
        .execute_batch(
            "CREATE TABLE writes (path TEXT);
             CREATE TRIGGER e_in AFTER INSERT ON edges BEGIN INSERT INTO writes
               SELECT f.path FROM symbols s JOIN files f ON f.id = s.file_id WHERE s.id = NEW.src_id;
             END;
             CREATE TRIGGER e_out BEFORE DELETE ON edges BEGIN INSERT INTO writes
               SELECT f.path FROM symbols s JOIN files f ON f.id = s.file_id WHERE s.id = OLD.src_id;
             END;
             CREATE TRIGGER u_in AFTER INSERT ON unresolved_calls BEGIN INSERT INTO writes
               SELECT path FROM files WHERE id = NEW.file_id;
             END;
             CREATE TRIGGER u_out BEFORE DELETE ON unresolved_calls BEGIN INSERT INTO writes
               SELECT path FROM files WHERE id = OLD.file_id;
             END;",
        )
        .unwrap();
}

/// The files [`count_writes`] saw a row of written or deleted, sorted.
fn written(db: &Path) -> Vec<String> {
    let store = GraphStore::open(db).unwrap();
    let mut stmt = store
        .conn()
        .prepare("SELECT DISTINCT path FROM writes ORDER BY path")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
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
    // `PIXEL_GRAPH_SOURCE_RUST=v1`: no source recorded, no pixel-mvp
    // state, the name-only edges included.
    let (_dir, root, db) = built(RustSource::V1);
    let store = GraphStore::open(&db).unwrap();
    assert_eq!(store.meta_get(GRAPH_SOURCE_KEY).unwrap(), None);
    assert!(!root.join(".pixel/.pixel-mvp").exists());
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
fn with_the_switch_unset_a_rust_crate_takes_pixel_mvps_edges() {
    if std::env::var_os("PIXEL_GRAPH_SOURCE_RUST").is_some() {
        return;
    }
    let (_dir, root, db) = built(RustSource::Mvp);
    let unset = root.join(".pixel/unset.db");
    build_graph(&root, &unset).unwrap();
    assert_eq!(decisions(&unset), decisions(&db));
    assert_eq!(
        GraphStore::open(&unset)
            .unwrap()
            .meta_get(GRAPH_SOURCE_KEY)
            .unwrap(),
        Some(mvp_source())
    );
}

#[test]
fn the_switch_reads_v1_as_off_and_anything_else_as_on() {
    assert_eq!(RustSource::parse(Some("v1")), RustSource::V1);
    assert_eq!(RustSource::parse(Some(" v1\n")), RustSource::V1);
    assert_eq!(RustSource::parse(None), RustSource::Mvp);
    assert_eq!(RustSource::parse(Some("mvp")), RustSource::Mvp);
    assert_eq!(RustSource::parse(Some("V1")), RustSource::Mvp);
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
    // The third through the daemon's route, a tree delta, where this
    // process's switch is the default; `update_file` otherwise.
    match tree_delta(&root, &db).unwrap() {
        Some(delta) if std::env::var_os("PIXEL_GRAPH_SOURCE_RUST").is_none() => {
            assert_eq!(
                delta
                    .changed
                    .iter()
                    .map(|(rel, _)| rel.as_str())
                    .collect::<Vec<_>>(),
                vec!["src/paths.rs"]
            );
            apply_tree_delta(&root, &db, &delta).unwrap();
        }
        _ => {
            update_file(&root, &db, "src/paths.rs").unwrap();
        }
    }

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
fn an_incremental_update_with_the_switch_on_writes_only_the_rust_rows_it_may_have_moved() {
    // One save used to delete and write again every Rust edge and
    // unresolved row of the graph (#1114). `src/paths.rs` defines nothing
    // another file calls or refers to: its own rows move; the edges and
    // unresolved rows of `src/lib.rs` and `src/util.rs` stay as they are,
    // without a statement.
    let (_dir, root, db) = built(RustSource::Mvp);
    count_writes(&db);
    fs::write(
        root.join("src/paths.rs"),
        "// moved\npub fn join(a: &str) -> String {\n    a.to_string()\n}\n",
    )
    .unwrap();
    update_file(&root, &db, "src/paths.rs").unwrap();
    assert_eq!(written(&db), vec!["src/paths.rs"]);
    let reference = root.join(".pixel/reference.db");
    build_graph_from(&root, &reference, RustSource::Mvp).unwrap();
    assert_eq!(decisions(&db), decisions(&reference));
}

#[test]
fn successive_scoped_updates_with_the_switch_on_store_what_a_full_build_stores() {
    // Each save but the fourth moves rows outside its own file without
    // changing which names exist: a new call from `util.rs` into
    // `paths.rs`, then `store.rs` renumbered under both callers, then
    // `paths.rs` rewritten under its new caller; the fourth gives `paths.rs`
    // an `open` of its own, so the fifth can take `store.rs`'s away from
    // under `util.rs` and `lib.rs`. After each, the graph is the one a full
    // build writes.
    let (_dir, root, db) = built(RustSource::Mvp);
    let steps: [(&str, &str); 5] = [
        (
            "src/util.rs",
            "pub fn helper() -> usize {\n    crate::paths::join(\"x\");\n    crate::store::open().len()\n}\n",
        ),
        (
            "src/store.rs",
            "\n\npub struct Store {\n    n: usize,\n}\nimpl Store {\n    pub fn len(&self) -> usize {\n        self.n\n    }\n}\npub fn open() -> Store {\n    Store { n: 1 }\n}\n",
        ),
        (
            "src/paths.rs",
            "\npub fn join(a: &str) -> String {\n    a.to_string()\n}\n",
        ),
        (
            "src/paths.rs",
            "\npub fn join(a: &str) -> String {\n    a.to_string()\n}\npub fn open() {}\n",
        ),
        (
            "src/store.rs",
            "\n\npub struct Store {\n    n: usize,\n}\nimpl Store {\n    pub fn len(&self) -> usize {\n        self.n\n    }\n}\n",
        ),
    ];
    for (n, (rel, body)) in steps.into_iter().enumerate() {
        fs::write(root.join(rel), body).unwrap();
        update_file(&root, &db, rel).unwrap();
        let reference = root.join(format!(".pixel/reference-{n}.db"));
        build_graph_from(&root, &reference, RustSource::Mvp).unwrap();
        assert_eq!(decisions(&db), decisions(&reference), "after {rel}");
    }
    let store = GraphStore::open(&db).unwrap();
    let helper = id(&store, "src/util.rs", "helper");
    let join = id(&store, "src/paths.rs", "join");
    assert!(
        store
            .edges_from(helper, Some(EdgeKind::Calls))
            .unwrap()
            .iter()
            .any(|e| e.dst_id == join && e.tier == Tier::Exact && e.site_line == 2)
    );
}

/// Applies each `(path, body)` save in turn (`None` deletes the file),
/// updating the graph after each, and holds it equal to a full build's.
fn saves_store_what_a_full_build_stores(root: &Path, db: &Path, steps: &[(&str, Option<&str>)]) {
    for (n, (rel, body)) in steps.iter().enumerate() {
        match body {
            Some(body) => fs::write(root.join(rel), body).unwrap(),
            None => fs::remove_file(root.join(rel)).unwrap(),
        }
        update_file(root, db, rel).unwrap();
        let reference = root.join(format!(".pixel/reference-{n}.db"));
        build_graph_from(root, &reference, RustSource::Mvp).unwrap();
        assert_eq!(
            decisions(db),
            decisions(&reference),
            "after step {n}, {rel}"
        );
    }
}

#[test]
fn a_definition_that_resolves_a_site_of_an_unsaved_file_writes_that_file_again() {
    // `util.rs` calls `crate::paths::extra`, which nothing defines in Rust
    // (Python does, so the name is not new): an unresolved row. Defining it
    // in `paths.rs` resolves the site, in a file the save did not touch.
    let (_dir, root, db) = built(RustSource::Mvp);
    saves_store_what_a_full_build_stores(
        &root,
        &db,
        &[
            (
                "scripts/env.py",
                Some("def unwrap(value):\n    return value\n\n\ndef extra():\n    return 1\n"),
            ),
            (
                "src/util.rs",
                Some(
                    "pub fn helper() -> usize {\n    crate::paths::extra();\n    crate::store::open().len()\n}\n",
                ),
            ),
            (
                "src/paths.rs",
                Some("pub fn join(a: &str) -> String {\n    a.to_string()\n}\npub fn extra() {}\n"),
            ),
        ],
    );
}

#[test]
fn a_deleted_file_writes_its_importers_again() {
    // `user.rs` imports `Gone` from `extra.rs` and never uses it: no edge
    // reaches `extra.rs`, only the import row. `paths.rs` defines a `Gone`
    // too, so deleting `extra.rs` makes no name disappear; the import then
    // resolves to no file, which only pixel-mvp's answer for `user.rs` says.
    let (_dir, root, db) = built(RustSource::Mvp);
    let lib = fs::read_to_string(root.join("src/lib.rs")).unwrap();
    let lib = format!("pub mod extra;\npub mod user;\n{lib}");
    saves_store_what_a_full_build_stores(
        &root,
        &db,
        &[
            ("src/extra.rs", Some("pub struct Gone;\n")),
            ("src/user.rs", Some("use crate::extra::Gone;\n")),
            ("src/lib.rs", Some(&lib)),
            (
                "src/paths.rs",
                Some("pub fn join(a: &str) -> String {\n    a.to_string()\n}\npub struct Gone;\n"),
            ),
            ("src/extra.rs", None),
        ],
    );
}

#[test]
fn an_update_that_names_a_new_symbol_writes_every_rust_file_again() {
    // `join` gains a sibling `scale` no symbol carried: an unresolved
    // reference to `scale` anywhere is recorded from now on, so no file's
    // rows can be kept as they were.
    let (_dir, root, db) = built(RustSource::Mvp);
    count_writes(&db);
    fs::write(
        root.join("src/paths.rs"),
        "pub fn join(a: &str) -> String {\n    a.to_string()\n}\npub fn scale() {}\n",
    )
    .unwrap();
    update_file(&root, &db, "src/paths.rs").unwrap();
    assert_eq!(
        written(&db),
        vec!["src/lib.rs", "src/paths.rs", "src/util.rs"]
    );
}

#[test]
fn an_index_refreshed_without_the_graph_makes_the_next_update_write_every_rust_file() {
    // A run refreshed pixel-mvp's index, then its graph update did not
    // commit: the graph's Rust rows are from a generation no delta of the
    // next refresh starts from, so that update writes every file again.
    let (_dir, root, db) = built(RustSource::Mvp);
    let lib = fs::read_to_string(root.join("src/lib.rs")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        lib.replace("    v.unwrap()", "    v.unwrap() + helper() as u8"),
    )
    .unwrap();
    refresh(&root, &root.join(".pixel")).unwrap();
    count_writes(&db);
    fs::write(
        root.join("src/paths.rs"),
        "// moved\npub fn join(a: &str) -> String {\n    a.to_string()\n}\n",
    )
    .unwrap();
    update_file(&root, &db, "src/paths.rs").unwrap();
    assert_eq!(
        written(&db),
        vec!["src/lib.rs", "src/paths.rs", "src/util.rs"]
    );
    let reference = root.join(".pixel/reference.db");
    build_graph_from(&root, &reference, RustSource::Mvp).unwrap();
    assert_eq!(decisions(&db), decisions(&reference));
}

#[test]
fn the_rust_rows_name_the_generation_of_pixel_mvp_they_were_written_from() {
    let (_dir, root, db) = built(RustSource::Mvp);
    let generation = || {
        use mvp_model::Graph;
        refresh(&root, &root.join(".pixel"))
            .unwrap()
            .provenance()
            .unwrap()
            .generation
    };
    let recorded = |db: &Path| {
        GraphStore::open(db)
            .unwrap()
            .meta_get(RUST_GENERATION_KEY)
            .unwrap()
    };
    assert_eq!(recorded(&db), Some(generation()));
    fs::write(root.join("src/paths.rs"), "pub fn join() {}\n").unwrap();
    update_file(&root, &db, "src/paths.rs").unwrap();
    assert_eq!(recorded(&db), Some(generation()));
    // A graph built with the switch off holds none.
    build_graph_from(&root, &db, RustSource::V1).unwrap();
    assert_eq!(recorded(&db), None);
}

#[test]
fn a_graph_built_from_the_other_source_is_rebuilt_whole() {
    if std::env::var_os("PIXEL_GRAPH_SOURCE_RUST").is_some() {
        return;
    }
    // The switch unset is pixel-mvp: a graph of this resolver's is not
    // current, and no delta is offered for it.
    let (_dir, root, db) = built(RustSource::V1);
    assert!(tree_delta(&root, &db).unwrap().is_none());
    let (_dir, root, db) = built(RustSource::Mvp);
    assert!(tree_delta(&root, &db).unwrap().is_some_and(|d| d.fresh));
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
