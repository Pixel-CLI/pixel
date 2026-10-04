// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Integration tests for the receiver rules: a qualified call from a file
//! whose own same-name function is the callee must not resolve to itself,
//! and a receiver path that names exactly one candidate's type
//! (`x::Store::open`) may resolve to that candidate instead of the shadow.

use std::fs;
use std::path::Path;

use pixel_git::GitRunner;
use pixel_graph::build::build_graph;
use pixel_graph::plan::{PlanQuery, run_plan_queries};
use pixel_graph::{EdgeKind, GraphStore, SymbolKind, Tier};

/// Three Rust files: `crates/bridge/src/lib.rs` has the wrapper `f` (which
/// calls `graph::build::f()`) and `g` (which calls the wrapper unqualified);
/// `crates/graph/src/build.rs` has the real, otherwise uncalled `f`; and
/// `crates/standalone/src/lib.rs` has `h`, uncalled with no same-name shadow
/// (the control for the dead-code assertion).
fn fixture(root: &Path) {
    let files = [
        (
            "crates/bridge/src/lib.rs",
            "pub fn f() -> u32 {\n    graph::build::f()\n}\n\npub fn g() -> u32 {\n    f()\n}\n",
        ),
        (
            "crates/graph/src/build.rs",
            "pub fn f() -> u32 {\n    1\n}\n",
        ),
        (
            "crates/standalone/src/lib.rs",
            "pub fn h() -> u32 {\n    2\n}\n",
        ),
    ];
    for (path, body) in files {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
}

#[test]
fn qualified_call_no_longer_links_the_callers_own_same_name_symbol() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let db = root.path().join(".pixel/graph.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    build_graph(root.path(), &db).unwrap();
    let store = GraphStore::open(&db).unwrap();

    let bridge_file = store
        .file_by_path("crates/bridge/src/lib.rs")
        .unwrap()
        .unwrap();
    let bridge_f = store
        .symbols_by_name("f", None, 10)
        .unwrap()
        .into_iter()
        .find(|s| s.file_id == bridge_file.id)
        .expect("bridge's own `f` is extracted");
    let bridge_g = store
        .symbols_by_name("g", None, 10)
        .unwrap()
        .into_iter()
        .find(|s| s.file_id == bridge_file.id)
        .expect("bridge's own `g` is extracted");

    let graph_file = store
        .file_by_path("crates/graph/src/build.rs")
        .unwrap()
        .unwrap();
    let graph_f = store
        .symbols_by_name("f", None, 10)
        .unwrap()
        .into_iter()
        .find(|s| s.file_id == graph_file.id)
        .expect("graph's `f` is extracted");

    // Pre-fix, `graph::build::f()` inside `f` resolved (Probable) to `f`
    // itself: the wrapper was its own caller. The receiver spells the module
    // `graph::build`, so the call now links to that file's `f`, at Probable:
    // the path is read from the file layout, not proven by an import.
    let calls = store
        .edges_from(bridge_f.id, Some(EdgeKind::Calls))
        .unwrap();
    assert_eq!(calls.len(), 1, "one call site, one edge: {calls:?}");
    assert_eq!(
        calls[0].dst_id, graph_f.id,
        "the qualified call targets graph's `f`"
    );
    assert_eq!(calls[0].tier, Tier::Probable);
    // The unqualified `f()` inside `g` still resolves Exact to the local `f`.
    let callers = store.edges_to(bridge_f.id, Some(EdgeKind::Calls)).unwrap();
    assert_eq!(callers.len(), 1, "callers: {callers:?}");
    assert_eq!(callers[0].src_id, bridge_g.id);
    assert_eq!(callers[0].tier, Tier::Exact);
    // No `f` site is left unresolved.
    let envelope = store.envelope_for_name("f").unwrap();
    assert!(!envelope.lower_bound, "envelope: {envelope:?}");
    assert_eq!(envelope.unresolved_same_name, 0);
    // `pixel plan --query dead-code`: the real `f` now has its caller, so it
    // is not listed; the genuinely uncalled `h` still is, so the assertion is
    // not vacuous.
    let runner = GitRunner::new(root.path());
    let findings = run_plan_queries(&store, root.path(), &runner, &[PlanQuery::DeadCode]).unwrap();
    assert!(
        findings
            .iter()
            .any(|f| f.file == "crates/standalone/src/lib.rs"),
        "the uncalled control `h` is still listed: {findings:?}"
    );
    assert!(
        findings
            .iter()
            .all(|f| f.file != "crates/graph/src/build.rs"),
        "the called `f` must not be listed as dead: {findings:?}"
    );
}

/// A module path shared by two files names neither: `util::f()` with a
/// `util.rs` in two crates stays unresolved (the envelope says so), while
/// `a::util::f()`, which only one of them ends with, links to it.
#[test]
fn a_module_path_two_files_end_with_stays_unresolved() {
    let root = tempfile::tempdir().unwrap();
    for (path, body) in [
        ("crates/a/src/util.rs", "pub fn f() -> u32 {\n    1\n}\n"),
        ("crates/b/src/util.rs", "pub fn f() -> u32 {\n    2\n}\n"),
        (
            "crates/app/src/lib.rs",
            "pub fn vague() -> u32 {\n    util::f()\n}\n\npub fn precise() -> u32 {\n    a::util::f()\n}\n",
        ),
    ] {
        let path = root.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    let db = root.path().join(".pixel/graph.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    build_graph(root.path(), &db).unwrap();
    let store = GraphStore::open(&db).unwrap();

    let symbol = |name: &str, path: &str| {
        let file = store.file_by_path(path).unwrap().unwrap();
        store
            .symbols_by_name(name, None, 10)
            .unwrap()
            .into_iter()
            .find(|s| s.file_id == file.id)
            .unwrap()
    };
    let a_f = symbol("f", "crates/a/src/util.rs");
    let vague = symbol("vague", "crates/app/src/lib.rs");
    let precise = symbol("precise", "crates/app/src/lib.rs");

    let vague_calls = store.edges_from(vague.id, Some(EdgeKind::Calls)).unwrap();
    assert!(
        vague_calls.is_empty(),
        "`util::f()` must not pick one of the two `util.rs`: {vague_calls:?}"
    );
    let calls = store.edges_from(precise.id, Some(EdgeKind::Calls)).unwrap();
    assert_eq!(calls.len(), 1, "calls: {calls:?}");
    assert_eq!(calls[0].dst_id, a_f.id);
    assert_eq!(calls[0].tier, Tier::Probable);
    let envelope = store.envelope_for_name("f").unwrap();
    assert!(envelope.lower_bound);
    assert_eq!(envelope.unresolved_same_name, 1, "only `util::f()` is left");
}

#[test]
fn ruby_root_call_resolves_from_a_script_scope() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("lib.rb"), "def foo\nend\n").unwrap();
    fs::write(root.path().join("script.rb"), "foo()\n").unwrap();
    let db = root.path().join(".pixel/graph.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    build_graph(root.path(), &db).unwrap();
    let store = GraphStore::open(&db).unwrap();

    let foo = store.symbols_by_name("foo", None, 10).unwrap().remove(0);
    let callers = store.edges_to(foo.id, Some(EdgeKind::Calls)).unwrap();
    assert_eq!(callers.len(), 1, "callers: {callers:?}");
    let script_file = store.file_by_path("script.rb").unwrap().unwrap();
    let script = store
        .symbols_in_file(script_file.id)
        .unwrap()
        .into_iter()
        .find(|symbol| symbol.id == callers[0].src_id)
        .unwrap();
    assert_eq!(script.kind, SymbolKind::Script);
    assert_eq!(script.qualified, "script.rb");
    assert_eq!(callers[0].tier, Tier::Probable);
    assert_eq!(
        store.envelope_for_name("foo").unwrap().unresolved_same_name,
        0
    );
}

/// The receiver relaxation: when a file holds the graph's only definition of
/// a name and it is an inherent method, a value receiver links to it at
/// `Probable` (never `Exact`). Before this, `w.push_call()` in
/// `crates/pixel-graph/src/extract.rs` was invisible to `pixel impact`.
#[test]
fn sole_inherent_method_with_a_value_receiver_becomes_a_probable_edge() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("walk.rs"),
        "struct Walker { calls: Vec<String> }\n\
         impl Walker {\n\
             fn push_call(&mut self, name: &str) {\n\
                 self.calls.push(name.to_string());\n\
             }\n\
         }\n\
         fn walk(w: &mut Walker, name: &str) {\n\
             w.push_call(name);\n\
         }\n",
    )
    .unwrap();
    let db = root.path().join(".pixel/graph.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    build_graph(root.path(), &db).unwrap();
    let store = GraphStore::open(&db).unwrap();

    let push_call = store
        .symbols_by_name("push_call", None, 10)
        .unwrap()
        .into_iter()
        .find(|s| s.kind == SymbolKind::Method)
        .expect("Walker::push_call is extracted");
    let callers = store.edges_to(push_call.id, Some(EdgeKind::Calls)).unwrap();
    assert_eq!(callers.len(), 1, "`w.push_call(name)` links: {callers:?}");
    assert_eq!(
        callers[0].tier,
        Tier::Probable,
        "the receiver's type is unknown: never Exact"
    );
    let walk = store
        .symbols_by_name("walk", None, 10)
        .unwrap()
        .into_iter()
        .find(|s| s.file_id == push_call.file_id)
        .expect("free `walk` is extracted");
    assert_eq!(callers[0].src_id, walk.id);
    // The row moved out of `unresolved_calls`, so the name's envelope is no
    // longer marked as a lower bound.
    assert!(
        !store.envelope_for_name("push_call").unwrap().lower_bound,
        "no unresolved `push_call` site remains"
    );
}

/// Precision guard: a trait-impl method with the same name does not relax.
/// `path.clone()` is `Clone::clone` for a std type the graph never sees, so
/// the file's own `impl Clone` must not attract the edge.
#[test]
fn trait_impl_method_with_a_value_receiver_stays_unresolved() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("clone.rs"),
        "struct Boxed;\n\
         impl Clone for Boxed {\n\
             fn clone(&self) -> Self { Boxed }\n\
         }\n\
         fn dup(path: &std::path::Path) -> std::path::PathBuf {\n\
             path.clone()\n\
         }\n",
    )
    .unwrap();
    let db = root.path().join(".pixel/graph.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    build_graph(root.path(), &db).unwrap();
    let store = GraphStore::open(&db).unwrap();

    let clone = store
        .symbols_by_name("clone", None, 10)
        .unwrap()
        .into_iter()
        .find(|s| s.kind == SymbolKind::Method)
        .expect("Boxed::clone is extracted");
    assert!(
        store
            .edges_to(clone.id, Some(EdgeKind::Calls))
            .unwrap()
            .is_empty(),
        "`path.clone()` must not link to the trait impl"
    );
    let envelope = store.envelope_for_name("clone").unwrap();
    assert!(envelope.lower_bound);
    assert_eq!(envelope.unresolved_same_name, 1);
}

/// The receiver-type tiebreak, end to end: `crate::store::Store::open()` in a
/// file that also defines `Other::open` links to `Store::open` as `Probable`,
/// not to the caller's own same-name method (the old shadow veto said
/// Unresolved) and not to the wrong one.
#[test]
fn receiver_path_type_selects_the_right_constructor() {
    let root = tempfile::tempdir().unwrap();
    for (path, body) in [
        (
            "crates/store/src/lib.rs",
            "pub struct Store;\n\
             impl Store {\n\
                 pub fn open() -> Store { Store }\n\
             }\n",
        ),
        (
            "crates/bridge/src/lib.rs",
            "pub struct Other;\n\
             impl Other {\n\
                 pub fn open() -> Other { Other }\n\
             }\n\
             pub fn connect() -> crate::store::Store {\n\
                 crate::store::Store::open()\n\
             }\n",
        ),
    ] {
        let path = root.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    let db = root.path().join(".pixel/graph.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    build_graph(root.path(), &db).unwrap();
    let store = GraphStore::open(&db).unwrap();

    let opens = store.symbols_by_name("open", None, 10).unwrap();
    assert_eq!(opens.len(), 2, "both constructors extracted: {opens:?}");
    let store_open = opens
        .iter()
        .find(|s| s.uid.contains("Store::open"))
        .unwrap();
    let other_open = opens
        .iter()
        .find(|s| s.uid.contains("Other::open"))
        .unwrap();
    let connect = store
        .symbols_by_name("connect", None, 10)
        .unwrap()
        .remove(0);

    let edges = store.edges_from(connect.id, Some(EdgeKind::Calls)).unwrap();
    assert_eq!(edges.len(), 1, "one call site, one edge: {edges:?}");
    assert_eq!(edges[0].dst_id, store_open.id, "targets Store::open");
    assert_eq!(edges[0].tier, Tier::Probable);
    assert!(
        edges.iter().all(|e| e.dst_id != other_open.id),
        "must not link the caller's own Other::open"
    );
    assert!(!store.envelope_for_name("open").unwrap().lower_bound);
}

/// Precision guard for the same tiebreak: two crates with a same-named type
/// make the path ambiguous, so the call stays unresolved instead of picking
/// one of them.
#[test]
fn two_same_named_types_keep_the_path_call_unresolved() {
    let root = tempfile::tempdir().unwrap();
    for path in ["crates/a/src/lib.rs", "crates/b/src/lib.rs"] {
        let path = root.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            "pub struct Store;\n\
             impl Store {\n\
                 pub fn open() -> Store { Store }\n\
             }\n",
        )
        .unwrap();
    }
    let path = root.path().join("crates/bridge/src/lib.rs");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        "pub fn connect() -> u32 {\n\
             a::Store::open();\n\
             0\n\
         }\n",
    )
    .unwrap();
    let db = root.path().join(".pixel/graph.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    build_graph(root.path(), &db).unwrap();
    let store = GraphStore::open(&db).unwrap();

    let connect = store
        .symbols_by_name("connect", None, 10)
        .unwrap()
        .remove(0);
    assert!(
        store
            .edges_from(connect.id, Some(EdgeKind::Calls))
            .unwrap()
            .is_empty(),
        "ambiguous `Store::open` must not fan out to a guess"
    );
    let envelope = store.envelope_for_name("open").unwrap();
    assert!(envelope.lower_bound);
    assert_eq!(envelope.unresolved_same_name, 1);
}
