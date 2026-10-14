// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! A TS/JS class property whose value is an arrow function is a method of its
//! class: the graph indexes it, resolves the calls made inside it, and
//! resolves calls to it, on a full build and after an incremental update of
//! either file.

use std::fs;

use pixel_graph::build::{build_graph, update_file};
use pixel_graph::{EdgeKind, GraphStore};

const APP: &str = "\
import { insertImage } from './image';
export class App {
  handleDrop = async (event: DragEvent) => {
    insertImage(event);
  };
  attach() {
    this.handleDrop(null);
  }
}
";
const IMAGE: &str = "export function insertImage(e: unknown) {\n  return e;\n}\n";

/// `(caller, callee)` names of every calls edge of the store.
fn call_edges(store: &GraphStore) -> Vec<(String, String)> {
    let mut edges = Vec::new();
    for file in store.files().unwrap() {
        for symbol in store.symbols_in_file(file.id).unwrap() {
            for edge in store.edges_from(symbol.id, Some(EdgeKind::Calls)).unwrap() {
                let callee = store
                    .conn()
                    .query_row(
                        "SELECT qualified FROM symbols WHERE id = ?1",
                        [edge.dst_id],
                        |row| row.get::<_, String>(0),
                    )
                    .unwrap();
                edges.push((symbol.qualified.clone(), callee));
            }
        }
    }
    edges.sort();
    edges
}

fn expected() -> Vec<(String, String)> {
    vec![
        ("App.attach".into(), "App.handleDrop".into()),
        ("App.handleDrop".into(), "insertImage".into()),
    ]
}

#[test]
fn a_class_property_arrow_is_a_resolved_callee_and_a_resolved_caller_after_every_kind_of_update() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("app.ts"), APP).unwrap();
    fs::write(root.path().join("image.ts"), IMAGE).unwrap();
    let db = root.path().join(".pixel/graph.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    build_graph(root.path(), &db).unwrap();
    assert_eq!(
        call_edges(&GraphStore::open(&db).unwrap()),
        expected(),
        "full build"
    );

    // The importer rewritten, then the target, then the importer again.
    fs::write(root.path().join("app.ts"), format!("{APP}// touched\n")).unwrap();
    update_file(root.path(), &db, "app.ts").unwrap();
    assert_eq!(
        call_edges(&GraphStore::open(&db).unwrap()),
        expected(),
        "after the class file"
    );
    fs::write(root.path().join("image.ts"), format!("{IMAGE}// touched\n")).unwrap();
    update_file(root.path(), &db, "image.ts").unwrap();
    assert_eq!(
        call_edges(&GraphStore::open(&db).unwrap()),
        expected(),
        "after the target"
    );
}
