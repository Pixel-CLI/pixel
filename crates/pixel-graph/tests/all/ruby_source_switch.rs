// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The Ruby source switch (`PIXEL_GRAPH_SOURCE_RUBY=hybrid`) on the Ruby
//! fixture (`mvp_adapter_ruby.rs`): what the graph holds with it on and
//! off, through the full build and through an incremental update.

use std::fs;
use std::path::{Path, PathBuf};

use pixel_graph::GraphStore;
use pixel_graph::build::{build_graph_sources, update_file};
use pixel_graph::mvp_adapter::RustSource;
use pixel_graph::mvp_adapter::ruby_switch::{
    RUBY_MVP_UNAVAILABLE, RUBY_SOURCE_KEY, RubySource, hybrid_source,
};

/// The fixture, built with `ruby` as the Ruby source.
fn built(ruby: RubySource) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    crate::mvp_adapter_ruby::fixture(&root);
    let db = root.join(".pixel/graph.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    build_graph_sources(&root, &db, RustSource::Mvp, ruby).unwrap();
    (dir, root, db)
}

fn rows(db: &Path, sql: &str) -> Vec<String> {
    let store = GraphStore::open(db).unwrap();
    let mut stmt = store.conn().prepare(sql).unwrap();
    let mut out: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    out.sort();
    out
}

/// `caller -> callee @line tier` of every edge, by qualified name.
fn edges(db: &Path) -> Vec<String> {
    rows(
        db,
        "SELECT a.qualified || ' -> ' || b.qualified || ' @' || e.site_line || ' ' || e.kind
                || ' ' || e.tier || ' ' || COALESCE(e.receiver, '-') || ' ' || COALESCE(e.callee, '-')
           FROM edges e JOIN symbols a ON a.id = e.src_id JOIN symbols b ON b.id = e.dst_id",
    )
}

/// Every row a resolution decision writes, the tables a graph holds and
/// its sources: what "the same graph" means here.
fn decisions(db: &Path) -> Vec<String> {
    let mut out = edges(db);
    out.extend(rows(
        db,
        "SELECT f.path || ' ' || u.name || ' ' || COALESCE(s.qualified, '-') || ' @' || u.site_line
                || ' ' || COALESCE(u.receiver, '-') || ' ' || u.kind
           FROM unresolved_calls u JOIN files f ON f.id = u.file_id
           LEFT JOIN symbols s ON s.id = u.enclosing_symbol_id",
    ));
    out.extend(rows(
        db,
        "SELECT 'table ' || name FROM sqlite_master WHERE type = 'table'",
    ));
    out.extend(rows(
        db,
        "SELECT key || '=' || value FROM meta WHERE key LIKE 'graph_source%'",
    ));
    out
}

#[test]
fn the_hybrid_drops_the_guess_pixel_mvp_disproves_and_keeps_every_other_edge() {
    let (_dir, _root, own) = built(RubySource::V1);
    let (_dir2, _root2, merged) = built(RubySource::Hybrid);
    let before = edges(&own);
    let after = edges(&merged);
    let gone: Vec<&String> = before.iter().filter(|e| !after.contains(e)).collect();
    let new: Vec<&String> = after.iter().filter(|e| !before.contains(e)).collect();
    // Only `title`, which pixel-mvp proves is Card's own `attr_accessor`
    // reader, loses this resolver's guess; nothing is added.
    assert_eq!(
        gone,
        vec!["Card#label -> Book#title @8 calls probable - title"],
        "{after:#?}"
    );
    assert!(new.is_empty(), "{new:?}");
    // The edges pixel-mvp abstains on stay, in the tier this resolver gave
    // them: a parameter's `save`, a Ruby `sort` into JavaScript; and the
    // mixed-language edge the other way, JavaScript into Ruby.
    for kept in [
        "Card#store -> Book#save @20 calls probable item save",
        "CardsController#index -> sort @3 calls probable - sort",
        "run -> Card#compute @5 calls probable - compute",
    ] {
        assert!(after.contains(&kept.to_string()), "{kept} in {after:#?}");
    }
    // The site stays unresolved, so `impact`'s lower bound counts it.
    let open = rows(
        &merged,
        "SELECT u.name || ' @' || u.site_line FROM unresolved_calls u
           JOIN files f ON f.id = u.file_id WHERE f.path = 'app/models/card.rb'",
    );
    assert!(open.contains(&"title @8".to_string()), "{open:?}");
    let store = GraphStore::open(&merged).unwrap();
    assert_eq!(
        store.meta_get(RUBY_SOURCE_KEY).unwrap(),
        Some(hybrid_source())
    );
}

#[test]
fn with_the_switch_off_the_graph_is_the_one_this_resolver_writes_alone() {
    let (_dir, root, own) = built(RubySource::V1);
    let store = GraphStore::open(&own).unwrap();
    assert_eq!(store.meta_get(RUBY_SOURCE_KEY).unwrap(), None);
    drop(store);
    let tables = rows(&own, "SELECT name FROM sqlite_master WHERE type = 'table'");
    assert!(
        !tables.iter().any(|t| t.starts_with("ruby_hybrid")),
        "{tables:?}"
    );
    // A merged graph rebuilt with the switch off is that graph again, row
    // for row: no side table, no source key, no edge of the merge.
    let merged = root.join(".pixel/merged.db");
    build_graph_sources(&root, &merged, RustSource::Mvp, RubySource::Hybrid).unwrap();
    assert_ne!(decisions(&merged), decisions(&own));
    build_graph_sources(&root, &merged, RustSource::Mvp, RubySource::V1).unwrap();
    assert_eq!(decisions(&merged), decisions(&own));
}

#[test]
fn an_incremental_update_of_a_merged_graph_stores_what_a_full_build_stores() {
    let (_dir, root, db) = built(RubySource::Hybrid);
    // Card loses its `attr_accessor *FIELDS`: pixel-mvp no longer proves a
    // `title` of Card, so this resolver's own edge must come back (the
    // merge is undone before the update, not stacked on it). Book gains a
    // `pack`, a competing definition for the edge both agree on.
    let card = fs::read_to_string(root.join("app/models/card.rb")).unwrap();
    fs::write(
        root.join("app/models/card.rb"),
        card.replace("  attr_accessor *FIELDS\n", "  attr_writer :title\n"),
    )
    .unwrap();
    let book = fs::read_to_string(root.join("app/models/book.rb")).unwrap();
    fs::write(
        root.join("app/models/book.rb"),
        book.replace("  def save\n", "  def pack\n    1\n  end\n\n  def save\n"),
    )
    .unwrap();
    for rel in ["app/models/card.rb", "app/models/book.rb"] {
        update_file(&root, &db, rel).unwrap();
    }
    let reference = root.join(".pixel/reference.db");
    build_graph_sources(&root, &reference, RustSource::Mvp, RubySource::Hybrid).unwrap();
    assert_eq!(decisions(&db), decisions(&reference));
    // And the undo held nothing back: a second update with no change to
    // the merge leaves the graph as it was.
    update_file(&root, &db, "app/models/tag.rb").unwrap();
    assert_eq!(decisions(&db), decisions(&reference));
}

#[test]
fn outside_a_git_work_tree_the_switch_keeps_this_resolver_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fs::create_dir_all(root.join("app")).unwrap();
    fs::write(
        root.join("app/a.rb"),
        "class A\n  def f\n    g\n  end\n\n  def g\n  end\nend\n",
    )
    .unwrap();
    let db = root.join(".pixel/graph.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    build_graph_sources(&root, &db, RustSource::Mvp, RubySource::Hybrid).unwrap();
    let own = root.join(".pixel/own.db");
    build_graph_sources(&root, &own, RustSource::Mvp, RubySource::V1).unwrap();
    let store = GraphStore::open(&db).unwrap();
    assert_eq!(
        store.meta_get(RUBY_SOURCE_KEY).unwrap().as_deref(),
        Some(RUBY_MVP_UNAVAILABLE)
    );
    assert_eq!(edges(&db), edges(&own));
}

#[test]
fn the_switch_reads_hybrid_as_on_and_anything_else_as_off() {
    assert_eq!(RubySource::parse(Some("hybrid")), RubySource::Hybrid);
    assert_eq!(RubySource::parse(Some(" hybrid\n")), RubySource::Hybrid);
    assert_eq!(RubySource::parse(None), RubySource::V1);
    assert_eq!(RubySource::parse(Some("v1")), RubySource::V1);
    assert_eq!(RubySource::parse(Some("Hybrid")), RubySource::V1);
}

#[test]
fn an_update_elsewhere_that_makes_pixel_mvp_abstain_gives_the_site_back_to_this_resolver() {
    // `params_for` is inherited from another file: this resolver links it
    // `probable`, pixel-mvp proves it, and the merge raises it to `exact`.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let write = |rel: &str, body: &str| {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    };
    write(
        "app/controllers/base_controller.rb",
        "class BaseController\n  def params_for\n    1\n  end\nend\n",
    );
    write(
        "app/controllers/decks_controller.rb",
        "class DecksController < BaseController\n  def index\n    params_for\n  end\nend\n",
    );
    write(".gitignore", ".pixel/\n");
    crate::mvp_adapter::git(&root, &["init", "-q"]);
    crate::mvp_adapter::git(&root, &["add", "-A"]);
    crate::mvp_adapter::git(&root, &["commit", "-qm", "fixture"]);
    let db = root.join(".pixel/graph.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    build_graph_sources(&root, &db, RustSource::Mvp, RubySource::Hybrid).unwrap();
    let call = |db: &Path| -> Vec<String> {
        edges(db)
            .into_iter()
            .filter(|e| e.contains(" calls "))
            .collect()
    };
    assert_eq!(
        call(&db),
        vec!["DecksController#index -> BaseController#params_for @3 calls exact - params_for"]
    );
    // A third file evaluates code it cannot read into BaseController:
    // pixel-mvp abstains on `params_for` now, while nothing this resolver
    // reads at that site changed. The update must hand the site back to
    // this resolver's own `probable` edge, not keep the proof it lost.
    write(
        "config/initializers/patch.rb",
        "code = ENV[\"X\"]\nBaseController.class_eval(code)\n",
    );
    update_file(&root, &db, "config/initializers/patch.rb").unwrap();
    assert_eq!(
        call(&db),
        vec!["DecksController#index -> BaseController#params_for @3 calls probable - params_for"]
    );
    let reference = root.join(".pixel/reference.db");
    build_graph_sources(&root, &reference, RustSource::Mvp, RubySource::Hybrid).unwrap();
    assert_eq!(decisions(&db), decisions(&reference));
}
