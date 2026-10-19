// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Assignment evidence must survive indexing without becoming a name-based call target.

use pixel_graph::build::{build_graph, update_file};
use pixel_graph::extract::extract_file;
use pixel_graph::{EdgeKind, GraphStore};
use serde_json::{Value, json};
use std::fs;

const SOURCE: &str = "const client = {}; const cache = {};\n\
client.fetch = client.read = function pull() {};\n\
cache.fetch = function other() {};\n\
function known() {}\n\
function invoke() { known(); client.read(); cache.fetch(); }\n";

fn facts(store: &GraphStore) -> Value {
    let tables: i64 = store
        .conn()
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'js_assignment_facts'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        tables, 1,
        "assigned aliases need evidence separate from resolvable symbols"
    );
    let payload: String = store.conn().query_row(
        "SELECT facts FROM js_assignment_facts JOIN files ON files.id = file_id WHERE path = 'app.js'",
        [], |r| r.get(0),
    ).unwrap();
    serde_json::from_str(&payload).unwrap()
}

fn assert_no_promotion(store: &GraphStore) {
    for name in ["fetch", "read", "pull", "other"] {
        assert!(
            store.symbols_by_name(name, None, 20).unwrap().is_empty(),
            "{name} is evidence, not a callable symbol"
        );
    }
    let caller = store.symbols_by_name("invoke", None, 20).unwrap().remove(0);
    let known = store.symbols_by_name("known", None, 20).unwrap().remove(0);
    let edges = store.edges_from(caller.id, Some(EdgeKind::Calls)).unwrap();
    assert_eq!(
        edges.iter().map(|e| e.dst_id).collect::<Vec<_>>(),
        vec![known.id]
    );
}

#[test]
fn assignments_should_survive_full_and_incremental_storage_without_promoting_aliases() {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join(".pixel/graph.db");
    fs::write(root.path().join("app.js"), SOURCE).unwrap();
    build_graph(root.path(), &db).unwrap();
    let store = GraphStore::open(&db).unwrap();
    let first = facts(&store);
    assert_eq!(first["parsed"], true);
    assert_eq!(first["assignments"].as_array().unwrap().len(), 3);
    assert_eq!(
        first["assignments"][0]["target"]["member"],
        json!(["client", "fetch"])
    );
    assert_eq!(
        first["assignments"][0]["value"]["span"],
        first["assignments"][1]["span"]
    );
    assert_eq!(
        first["assignments"][1]["value"]["kind"],
        "function_expression"
    );
    assert_no_promotion(&store);
    drop(store);

    fs::write(
        root.path().join("app.js"),
        format!("{SOURCE}client.read = cache.fetch;\n"),
    )
    .unwrap();
    update_file(root.path(), &db, "app.js").unwrap();
    let store = GraphStore::open(&db).unwrap();
    let updated = facts(&store);
    assert_eq!(updated["assignments"].as_array().unwrap().len(), 4);
    assert_eq!(
        updated["assignments"][3]["value"]["member"],
        json!(["cache", "fetch"])
    );
    assert_no_promotion(&store);
    drop(store);
    let rebuilt = root.path().join(".pixel/rebuilt.db");
    build_graph(root.path(), &rebuilt).unwrap();
    assert_eq!(facts(&GraphStore::open(&rebuilt).unwrap()), updated);

    fs::write(root.path().join("app.js"), "const removed = {};\n").unwrap();
    update_file(root.path(), &db, "app.js").unwrap();
    assert_eq!(
        facts(&GraphStore::open(&db).unwrap())["assignments"],
        json!([])
    );
    fs::remove_file(root.path().join("app.js")).unwrap();
    update_file(root.path(), &db, "app.js").unwrap();
    let count: i64 = GraphStore::open(&db)
        .unwrap()
        .conn()
        .query_row("SELECT count(*) FROM js_assignment_facts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        count, 0,
        "deleted source must leave no stale alias evidence"
    );
}

#[test]
fn lexical_contexts_should_distinguish_shadowing_without_claiming_receiver_identity() {
    let src = "let/*comment*/a={}; {let a={}; a.read=b.read;} function f(a){this.read=a.read; function g(){this.read=c.read;}} a.read=0;";
    let fx = extract_file("app.js", src.as_bytes())
        .unwrap()
        .js_assignments;
    assert!(fx.parsed);
    assert!(!fx.truncated);
    assert_eq!(
        fx.bindings
            .iter()
            .map(|b| b.kind.as_str())
            .collect::<Vec<_>>(),
        ["let", "let", "parameter"]
    );
    assert_ne!(fx.bindings[0].context, fx.bindings[1].context);
    assert_ne!(fx.bindings[1].pattern.span, fx.bindings[2].pattern.span);
    assert_eq!(fx.assignments.len(), 4);
    assert_ne!(fx.assignments[1].context, fx.assignments[2].context);
    assert_eq!(
        fx.assignments[1].target.member,
        Some(("this".into(), "read".into()))
    );
    assert_eq!(fx.assignments[3].value.as_ref().unwrap().kind, "number");
    for assignment in &fx.assignments {
        let container = &fx.contexts[assignment.context];
        assert_eq!(
            container.kind,
            if assignment.context == 0 {
                "program"
            } else {
                "statement_block"
            }
        );
        assert!(container.span[0] <= assignment.span[0] && assignment.span[1] <= container.span[1]);
    }
    let same_span = extract_file("app.js", b"function f(a){a.read=b.read;}")
        .unwrap()
        .js_assignments;
    assert_eq!(same_span.contexts[0].span, same_span.contexts[1].span);
    assert_eq!(same_span.contexts[1].parent, Some(0));
    assert_eq!(same_span.bindings[0].context, 1);
}

#[test]
fn syntax_should_keep_computed_writes_patterns_and_utf8_coordinates_without_inventing_names() {
    let src = "const {a}=external; a.run = a => {a[key]=b.read; a.read=b[key]; a.read+=b.read; a.read++; delete a.read;}; const café=0; café.read = () => {};";
    let fx = extract_file("app.js", src.as_bytes())
        .unwrap()
        .js_assignments;
    assert_eq!(fx.bindings[0].pattern.kind, "object_pattern");
    assert_eq!(fx.bindings[1].kind, "parameter");
    assert_eq!(fx.assignments.len(), 7);
    assert_eq!(fx.assignments[1].target.member, None);
    assert_eq!(fx.assignments[2].value.as_ref().unwrap().member, None);
    assert_eq!(fx.assignments[3].kind, "augmented_assignment_expression");
    assert_eq!(fx.assignments[4].kind, "update_expression");
    assert_eq!(fx.assignments[5].kind, "unary_expression");
    assert_eq!(fx.assignments[4].value, None);
    assert_eq!(fx.assignments[5].value, None);
    let last = &fx.assignments[6];
    assert_eq!(&src[last.span[0]..last.span[1]], "café.read = () => {}");
    assert_eq!(last.target.member, Some(("café".into(), "read".into())));
    assert_eq!(last.value.as_ref().unwrap().kind, "arrow_function");
}

#[test]
fn unsupported_syntax_should_not_look_like_a_parsed_empty_inventory() {
    for (path, src) in [("app.js", "a.read=function("), ("app.ts", "a.read=()=>1;")] {
        let facts = extract_file(path, src.as_bytes()).unwrap().js_assignments;
        assert!(!facts.parsed);
        assert!(facts.assignments.is_empty());
    }
}

#[test]
fn stores_should_migrate_and_round_trip_facts_without_hiding_corrupt_json() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("graph.db");
    let store = GraphStore::open(&db).unwrap();
    store
        .conn()
        .execute_batch("DROP TABLE js_assignment_facts")
        .unwrap();
    drop(store);
    let mut store = GraphStore::open(&db).unwrap();
    let id = store.replace_file("app.js", "blob", "js").unwrap();
    assert_eq!(store.js_assignment_facts(id).unwrap(), None);
    let facts = extract_file("app.js", SOURCE.as_bytes())
        .unwrap()
        .js_assignments;
    store.set_js_assignment_facts(id, &facts).unwrap();
    assert_eq!(store.js_assignment_facts(id).unwrap(), Some(facts));
    store
        .conn()
        .execute("UPDATE js_assignment_facts SET facts = 'broken'", [])
        .unwrap();
    assert!(store.js_assignment_facts(id).is_err());
    store.replace_file("app.js", "other", "js").unwrap();
    assert_eq!(store.js_assignment_facts(id).unwrap(), None);
}
