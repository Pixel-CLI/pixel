// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Integration tests for Go calls through a package name (#1456):
//! `daemon.Run()` names the function `Run` of the package an import of the
//! calling file points at, whatever other package defines a `Run` too. A
//! function handed over the same way (`run(start.Run)`, #1458) is a
//! reference to it.

use std::fs;
use std::path::Path;

use pixel_graph::build::{build_graph, update_file};
use pixel_graph::{EdgeKind, GraphStore, Tier};

const MAIN: &str = "package main\n\nimport (\n\t\"example.com/app/internal/daemon\"\n\t\"example.com/app/internal/start\"\n)\n\nfunc main() {\n\tdaemon.Run()\n\tstart.Boot()\n}\n";
const DAEMON: &str = "package daemon\n\nfunc Run() {}\n";
const START: &str = "package start\n\nfunc Run() {}\n\nfunc Boot() {}\n";

fn write(root: &Path, path: &str, body: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

/// `main.go` calls `daemon.Run()`; `internal/daemon` and `internal/start`
/// each define a `Run`.
fn fixture(root: &Path) -> std::path::PathBuf {
    write(root, "main.go", MAIN);
    write(root, "internal/daemon/cli.go", DAEMON);
    write(root, "internal/start/start.go", START);
    let db = root.join(".pixel/graph.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    build_graph(root, &db).unwrap();
    db
}

/// Every edge from `main` to a symbol named `Run`, whatever its kind: the
/// target's uid, the edge kind and its tier.
fn runs_reached_from_main(db: &Path) -> Vec<(String, EdgeKind, Tier)> {
    let store = GraphStore::open(db).unwrap();
    let main = store
        .symbol_by_uid("main.go#main#function")
        .unwrap()
        .expect("`main` is extracted");
    let mut reached: Vec<(String, EdgeKind, Tier)> = store
        .edges_from(main.id, None)
        .unwrap()
        .into_iter()
        .filter_map(|edge| {
            let target = store.symbol_by_id(edge.dst_id).unwrap()?;
            (target.name == "Run").then_some((target.uid, edge.kind, edge.tier))
        })
        .collect();
    reached.sort_by(|a, b| a.0.cmp(&b.0));
    reached
}

fn daemon_run() -> Vec<(String, EdgeKind, Tier)> {
    vec![(
        "internal/daemon/cli.go#Run#function".to_string(),
        EdgeKind::Calls,
        Tier::Probable,
    )]
}

#[test]
fn a_go_call_through_a_package_name_should_link_the_function_of_the_imported_directory() {
    let root = tempfile::tempdir().unwrap();
    let db = fixture(root.path());

    // Two packages define `Run`, so the name alone picks neither: before
    // #1456 `main` had no edge to any `Run`. Probable, not Exact: a local
    // variable named `daemon` would shadow the package, which no row records.
    assert_eq!(runs_reached_from_main(&db), daemon_run());
}

#[test]
fn the_edge_should_hold_after_an_update_of_the_caller_the_target_and_a_competitor() {
    let root = tempfile::tempdir().unwrap();
    let db = fixture(root.path());

    write(root.path(), "main.go", &format!("// entry point\n{MAIN}"));
    update_file(root.path(), &db, "main.go").unwrap();
    assert_eq!(
        runs_reached_from_main(&db),
        daemon_run(),
        "caller rewritten"
    );

    write(
        root.path(),
        "internal/daemon/cli.go",
        "package daemon\n\n// Run serves until told to stop.\nfunc Run() {}\n",
    );
    update_file(root.path(), &db, "internal/daemon/cli.go").unwrap();
    assert_eq!(
        runs_reached_from_main(&db),
        daemon_run(),
        "target rewritten"
    );

    write(
        root.path(),
        "internal/worker/worker.go",
        "package worker\n\nfunc Run() {}\n",
    );
    update_file(root.path(), &db, "internal/worker/worker.go").unwrap();
    assert_eq!(
        runs_reached_from_main(&db),
        daemon_run(),
        "a third `Run` added"
    );
}

#[test]
fn a_package_added_after_its_caller_should_take_the_call_from_the_sole_other_definition() {
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "main.go", MAIN);
    write(root.path(), "internal/start/start.go", START);
    let db = root.path().join(".pixel/graph.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    build_graph(root.path(), &db).unwrap();

    write(root.path(), "internal/daemon/cli.go", DAEMON);
    update_file(root.path(), &db, "internal/daemon/cli.go").unwrap();

    assert_eq!(runs_reached_from_main(&db), daemon_run());
}

#[test]
fn two_functions_of_the_name_in_the_package_should_leave_the_call_unresolved() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "internal/daemon/cli_other.go",
        "package daemon\n\nfunc Run() {}\n",
    );
    let db = fixture(root.path());

    // Build-constrained twins (`run_linux.go`, `run_darwin.go`) look like
    // this: the graph cannot tell which one a build keeps, and never picks
    // by symbol order.
    assert_eq!(runs_reached_from_main(&db), Vec::new());
}

const MAIN_VALUE: &str = "package main\n\nimport (\n\t\"example.com/app/internal/daemon\"\n\t\"example.com/app/internal/start\"\n)\n\nfunc main() {\n\tdaemon.Only()\n\trun(start.Run)\n}\n\nfunc run(f func()) { f() }\n";

fn start_run_referenced() -> Vec<(String, EdgeKind, Tier)> {
    vec![(
        "internal/start/start.go#Run#function".to_string(),
        EdgeKind::References,
        Tier::Probable,
    )]
}

/// Rows of `unresolved_calls` naming `Run`, of any kind.
fn unresolved_runs(db: &Path) -> i64 {
    GraphStore::open(db)
        .unwrap()
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM unresolved_calls WHERE name = 'Run'",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn a_go_function_passed_through_its_package_name_should_be_referenced_by_the_caller() {
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "main.go", MAIN_VALUE);
    write(
        root.path(),
        "internal/daemon/cli.go",
        "package daemon\n\nfunc Run() {}\n\nfunc Only() {}\n",
    );
    write(root.path(), "internal/start/start.go", START);
    let db = root.path().join(".pixel/graph.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    build_graph(root.path(), &db).unwrap();

    // Before #1458 a `pkg.Func` argument was dropped with every other member
    // access: `main` reached no `Run` at all.
    assert_eq!(runs_reached_from_main(&db), start_run_referenced());

    write(
        root.path(),
        "main.go",
        &format!("// entry point\n{MAIN_VALUE}"),
    );
    update_file(root.path(), &db, "main.go").unwrap();
    assert_eq!(
        runs_reached_from_main(&db),
        start_run_referenced(),
        "caller rewritten"
    );

    write(
        root.path(),
        "internal/start/start.go",
        &format!("// Package start boots the app.\n{START}"),
    );
    update_file(root.path(), &db, "internal/start/start.go").unwrap();
    assert_eq!(
        runs_reached_from_main(&db),
        start_run_referenced(),
        "target rewritten"
    );
    assert_eq!(unresolved_runs(&db), 0);
}

#[test]
fn a_member_of_a_go_value_passed_as_an_argument_should_record_nothing() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "main.go",
        "package main\n\nimport \"example.com/app/internal/start\"\n\nfunc main() {\n\tuser := start.Load()\n\trun(user.Run)\n}\n\nfunc run(f func()) { f() }\n",
    );
    write(root.path(), "internal/daemon/cli.go", DAEMON);
    write(
        root.path(),
        "internal/start/start.go",
        "package start\n\ntype User struct{}\n\nfunc Load() User { return User{} }\n",
    );
    let db = root.path().join(".pixel/graph.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    build_graph(root.path(), &db).unwrap();

    // `user` is a value: `user.Run` is its method, not the sole function
    // `Run` the repository happens to define in another package.
    assert_eq!(runs_reached_from_main(&db), Vec::new());
    assert_eq!(unresolved_runs(&db), 0);
}
