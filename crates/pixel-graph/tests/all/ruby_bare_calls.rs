// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Integration test for Ruby calls written without receiver or parentheses:
//! `target` and `target.to_set` call the method `target` exactly as
//! `target()` does, so `impact target` must list their callers, while a local
//! variable of the same name calls nothing.

use std::collections::BTreeSet;
use std::fs;

use pixel_graph::build::build_graph;
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
