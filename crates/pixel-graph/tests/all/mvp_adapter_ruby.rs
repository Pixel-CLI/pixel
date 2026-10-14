// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The Ruby merge (`pixel_graph::mvp_adapter::ruby`) on a small Rails-shaped
//! tree holding one case of each thing the 2026-10-15 comparison found:
//! a reader pixel-mvp proves where this graph guesses another class's
//! method, a call on a local only this graph links, a Ruby call this graph
//! links to a JavaScript function, a call split over lines, a route and an
//! association.

use std::fs;
use std::path::Path;

use pixel_graph::GraphStore;
use pixel_graph::build::build_graph_from;
use pixel_graph::mvp_adapter::ruby::{RubyPlan, ruby_plan};
use pixel_graph::mvp_adapter::{RustSource, refresh};

/// The tree. Line numbers are the assertions'.
pub(crate) fn fixture(root: &Path) {
    let files: [(&str, &str); 7] = [
        (
            "app/models/card.rb",
            "class Card\n\
             \x20 FIELDS = %i[ title ]\n\
             \x20 attr_accessor *FIELDS\n\
             \n\
             \x20 has_many :tags\n\
             \n\
             \x20 def label\n\
             \x20   title\n\
             \x20 end\n\
             \n\
             \x20 def total\n\
             \x20   compute\n\
             \x20 end\n\
             \n\
             \x20 def compute\n\
             \x20   1\n\
             \x20 end\n\
             \n\
             \x20 def store(item)\n\
             \x20   item.save\n\
             \x20 end\n\
             \n\
             \x20 def boxed\n\
             \x20   Box.new(\n\
             \x20     size: 1\n\
             \x20   ).pack\n\
             \x20 end\n\
             end\n",
        ),
        (
            "app/models/book.rb",
            "class Book\n  def title\n    \"b\"\n  end\n\n  def save\n    true\n  end\nend\n",
        ),
        ("app/models/tag.rb", "class Tag\nend\n"),
        (
            "app/models/box.rb",
            "class Box\n  def initialize(size:)\n    @size = size\n  end\n\n  def pack\n    @size\n  end\nend\n",
        ),
        (
            "app/controllers/cards_controller.rb",
            "class CardsController\n  def index\n    sort\n  end\n\n  def show\n  end\nend\n",
        ),
        (
            "config/routes.rb",
            "Rails.application.routes.draw do\n  resources :cards, only: [:index, :show]\nend\n",
        ),
        (
            "app/javascript/sorter.js",
            "export function sort(list) {\n  return list.slice().sort()\n}\nexport function run() {\n  return compute()\n}\n",
        ),
    ];
    for (path, body) in files {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    fs::write(root.join(".gitignore"), ".pixel/\n").unwrap();
    crate::mvp_adapter::git(root, &["init", "-q"]);
    crate::mvp_adapter::git(root, &["add", "-A"]);
    crate::mvp_adapter::git(root, &["commit", "-qm", "fixture"]);
}

/// This graph of the fixture, by its own resolver, and the merge.
fn planned() -> (tempfile::TempDir, GraphStore, RubyPlan) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fixture(&root);
    let db = root.join(".pixel/graph.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    build_graph_from(&root, &db, RustSource::V1).unwrap();
    let store = GraphStore::open(&db).unwrap();
    let mvp = refresh(&root, &root.join(".pixel")).unwrap();
    let plan = ruby_plan(&store, &mvp).unwrap();
    (dir, store, plan)
}

/// `caller -> callee @line tier` of the edges of `ids`, sorted.
fn edges(store: &GraphStore, ids: &[i64]) -> Vec<String> {
    let mut out: Vec<String> = ids
        .iter()
        .map(|id| {
            store
                .conn()
                .query_row(
                    "SELECT s.qualified, d.qualified, e.site_line, e.tier FROM edges e
                       JOIN symbols s ON s.id = e.src_id JOIN symbols d ON d.id = e.dst_id
                      WHERE e.id = ?1",
                    [id],
                    |r| {
                        Ok(format!(
                            "{} -> {} @{} {}",
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, u32>(2)?,
                            r.get::<_, String>(3)?
                        ))
                    },
                )
                .unwrap()
        })
        .collect();
    out.sort();
    out
}

#[test]
fn the_merge_drops_only_the_guess_pixel_mvp_disproves_and_keeps_the_rest() {
    let (_dir, store, plan) = planned();
    // `title` in Card#label is the `attr_accessor *FIELDS` reader, which
    // only pixel-mvp reads: this graph's name guess, Book#title, goes, and
    // the site stays an unresolved row (no symbol here to link it to).
    assert_eq!(
        edges(&store, &plan.remove_edges),
        vec!["Card#label -> Book#title @8 probable"]
    );
    assert!(plan.add_edges.is_empty(), "{:?}", plan.add_edges);
    assert_eq!(plan.add_unresolved.len(), 1);
    assert_eq!(
        (
            plan.add_unresolved[0].name.as_str(),
            plan.add_unresolved[0].site_line
        ),
        ("title", 8)
    );
    // Kept: `item.save` (pixel-mvp cannot type a parameter), `sort` into
    // JavaScript (wrong, but pixel-mvp abstains: ADR-0037's stated limit),
    // and the agreements, `compute`, `Box.new(…).pack` (this graph at
    // line 24, pixel-mvp at 26: paired, not doubled), the two routes and
    // the association.
    assert_eq!((plan.calls.v1, plan.calls.hybrid), (6, 5));
    assert_eq!(plan.calls.v1_replaced, 1);
    assert_eq!(plan.calls.v1_kept, 2);
    assert_eq!(plan.calls.both, 3);
    assert_eq!(plan.calls.mvp_only_site, 0);
    assert_eq!((plan.routes.both, plan.routes.hybrid), (2, 2));
    assert_eq!((plan.associations.both, plan.associations.hybrid), (1, 1));
    assert_eq!(plan.no_target, 1);
}
