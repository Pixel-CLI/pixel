// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Integration test for Ruby calls written without receiver or parentheses:
//! `target` and `target.to_set` call the method `target` exactly as
//! `target()` does, so `impact target` must list their callers, while a local
//! variable of the same name calls nothing.

use std::collections::BTreeSet;
use std::fs;

use pixel_graph::build::{build_graph, update_file};
use pixel_graph::{EdgeKind, GraphStore};

#[test]
fn ruby_callers_should_include_bare_and_receiver_position_calls() {
    let root = tempfile::tempdir().unwrap();
    let source = [
        "class Svc",
        "  def chained",
        "    @ids ||= target.to_set",
        "  end",
        "  def bare",
        "    target",
        "  end",
        "  def parens",
        "    target()",
        "  end",
        "  def with_self",
        "    self.target",
        "  end",
        "  def shadowed",
        "    target = 1",
        "    target",
        "  end",
        "  def target",
        "    [1]",
        "  end",
        "end",
        "",
    ]
    .join("\n");
    fs::write(root.path().join("a.rb"), source).unwrap();
    let db = root.path().join(".pixel/graph.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    build_graph(root.path(), &db).unwrap();
    let store = GraphStore::open(&db).unwrap();

    let target = store
        .symbols_by_name("target", None, 10)
        .unwrap()
        .into_iter()
        .find(|symbol| symbol.qualified == "Svc#target")
        .expect("`def target` is extracted");
    let in_file = store.symbols_in_file(target.file_id).unwrap();
    let callers: BTreeSet<String> = store
        .edges_to(target.id, Some(EdgeKind::Calls))
        .unwrap()
        .into_iter()
        .map(|edge| {
            in_file
                .iter()
                .find(|symbol| symbol.id == edge.src_id)
                .expect("every caller is a method of the one fixture file")
                .qualified
                .clone()
        })
        .collect();
    assert_eq!(
        callers,
        ["Svc#bare", "Svc#chained", "Svc#parens", "Svc#with_self"]
            .into_iter()
            .map(String::from)
            .collect::<BTreeSet<_>>(),
        "the four call forms are callers; the method reading a local is not"
    );
    let envelope = store.envelope_for_name("target").unwrap();
    assert_eq!(
        envelope.unresolved_same_name, 0,
        "every call to `target` resolved, so the caller set is complete"
    );
    assert!(!envelope.lower_bound);
}

/// `Svc#run` calls its own `access_logs` without receiver; `Svc.build` (whose
/// `self` is the class) and `Admin#run` (another class) call a name their
/// own class does not define.
const SELF_CALLS: [&str; 19] = [
    "class Svc",
    "  def run",
    "    access_logs(user: 1)",
    "  end",
    "  def self.build",
    "    access_logs(user: 2)",
    "  end",
    "  def access_logs(user:)",
    "    [user]",
    "  end",
    "end",
    "",
    "class Admin",
    "  def run",
    "    access_logs(user: 3)",
    "  end",
    "end",
    "",
    "",
];

/// A second class in another file defining the same name, as a Rails
/// controller action next to a service's private helper does.
const COMPETING: &str = "class Other\n  def access_logs\n    []\n  end\nend\n";

/// A file reopening `Svc` to define `access_logs` again makes the override
/// depend on load order: the edge the full build gave `Svc#run` goes back
/// to unresolved once the update sees the reopened class.
#[test]
fn ruby_bare_call_should_lose_its_exact_edge_when_a_reopened_class_redefines_it() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a.rb"), SELF_CALLS.join("\n")).unwrap();
    fs::write(root.path().join("other.rb"), COMPETING).unwrap();
    let db = root.path().join(".pixel/graph.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    build_graph(root.path(), &db).unwrap();
    assert_eq!(svc_access_logs_callers(&db), (only_svc_run(), 2));

    fs::write(
        root.path().join("patch.rb"),
        "class Svc\n  def access_logs(user:)\n    []\n  end\nend\n",
    )
    .unwrap();
    update_file(root.path(), &db, "patch.rb").unwrap();
    let store = GraphStore::open(&db).unwrap();
    let svc_definitions: Vec<_> = store
        .symbols_by_name("access_logs", None, 10)
        .unwrap()
        .into_iter()
        .filter(|symbol| symbol.qualified == "Svc#access_logs")
        .collect();
    assert_eq!(svc_definitions.len(), 2, "a.rb and patch.rb both define it");
    for definition in &svc_definitions {
        assert_eq!(
            store
                .edges_to(definition.id, Some(EdgeKind::Calls))
                .unwrap()
                .into_iter()
                .map(|edge| edge.src_id)
                .collect::<Vec<_>>(),
            Vec::<i64>::new(),
            "no Exact edge picks one of the two `Svc#access_logs`"
        );
    }
    assert_eq!(
        store
            .envelope_for_name("access_logs")
            .unwrap()
            .unresolved_same_name,
        3,
        "`Svc#run` joins `Svc.build` and `Admin#run` among the unresolved sites"
    );
}

/// The qualified names of the `Calls` edges into `Svc#access_logs`, and the
/// `access_logs` call sites left unresolved.
fn svc_access_logs_callers(db: &std::path::Path) -> (BTreeSet<String>, u64) {
    let store = GraphStore::open(db).unwrap();
    let target = store
        .symbols_by_name("access_logs", None, 10)
        .unwrap()
        .into_iter()
        .find(|symbol| symbol.qualified == "Svc#access_logs")
        .expect("`def access_logs` is extracted");
    let in_file = store.symbols_in_file(target.file_id).unwrap();
    let callers = store
        .edges_to(target.id, Some(EdgeKind::Calls))
        .unwrap()
        .into_iter()
        .map(|edge| {
            in_file
                .iter()
                .find(|symbol| symbol.id == edge.src_id)
                .expect("every caller is a method of the fixture file")
                .qualified
                .clone()
        })
        .collect();
    let unresolved = store
        .envelope_for_name("access_logs")
        .unwrap()
        .unresolved_same_name;
    (callers, unresolved)
}

fn only_svc_run() -> BTreeSet<String> {
    BTreeSet::from(["Svc#run".to_string()])
}

#[test]
fn ruby_bare_call_should_reach_its_own_class_method_despite_a_same_name_method_elsewhere() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a.rb"), SELF_CALLS.join("\n")).unwrap();
    fs::write(root.path().join("other.rb"), COMPETING).unwrap();
    let db = root.path().join(".pixel/graph.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    build_graph(root.path(), &db).unwrap();

    assert_eq!(
        svc_access_logs_callers(&db),
        (only_svc_run(), 2),
        "the full build links `Svc#run` and leaves the class method and `Admin#run` unresolved"
    );
}

#[test]
fn ruby_bare_call_should_keep_its_own_class_method_across_incremental_updates() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a.rb"), SELF_CALLS.join("\n")).unwrap();
    let db = root.path().join(".pixel/graph.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    build_graph(root.path(), &db).unwrap();

    // A competing definition appears: `reconsider_resolved_calls` and
    // `resolve_all` re-decide the edges built while the name was unique.
    fs::write(root.path().join("other.rb"), COMPETING).unwrap();
    update_file(root.path(), &db, "other.rb").unwrap();
    assert_eq!(
        svc_access_logs_callers(&db),
        (only_svc_run(), 2),
        "adding `Other#access_logs` keeps the caller of the own method"
    );

    // The caller's and target's file is rewritten: its rows are re-extracted
    // and resolved again next to the competing definition.
    let mut rewritten = SELF_CALLS.join("\n");
    rewritten.push_str("# touched\n");
    fs::write(root.path().join("a.rb"), rewritten).unwrap();
    update_file(root.path(), &db, "a.rb").unwrap();
    assert_eq!(
        svc_access_logs_callers(&db),
        (only_svc_run(), 2),
        "re-indexing the caller's file keeps the edge"
    );
}
