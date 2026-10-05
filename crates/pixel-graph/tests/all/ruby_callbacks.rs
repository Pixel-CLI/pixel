// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Ruby callback symbols must remain attached to their owner through graph refreshes.

use std::fs;

use pixel_graph::build::{build_graph, update_file};
use pixel_graph::impact::{Direction, impact};
use pixel_graph::{EdgeKind, GraphStore, Tier};

const SOURCE: &str = "class Record\n  validate :check, if: :ready?\n  before_action :check, only: :show\n  after_commit :foreign\n  after_save :class_only\n  def check; end\n  def ready?; true; end\n  def show; end\n  def self.class_only; end\nend\n";

fn assert_callback_owner(store: &GraphStore, name: &str) {
    let uid = format!("record.rb#Record#{name}#method");
    let report = impact(store, &uid, Direction::Upstream, 3, 50).unwrap();
    assert_eq!(
        report
            .referenced_by
            .iter()
            .map(|s| (s.uid.as_str(), s.tier.as_str()))
            .collect::<Vec<_>>(),
        [("record.rb#Record#class", "probable")],
        "the declaring class references its own callback even with same-name methods elsewhere"
    );
    assert_eq!(report.referenced_by_total, 1);
    assert_eq!(
        report.d1_will_break.len(),
        0,
        "callbacks are references, never definite Calls"
    );
}

#[test]
fn ruby_callbacks_should_keep_their_owner_through_full_and_incremental_builds() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("record.rb"), SOURCE).unwrap();
    fs::write(
        root.path().join("other.rb"),
        "class Other\n  def check; end\n  def foreign; end\nend\n",
    )
    .unwrap();
    let db = root.path().join("graph.db");
    build_graph(root.path(), &db).unwrap();
    let store = GraphStore::open(&db).unwrap();
    assert_callback_owner(&store, "check");
    assert_callback_owner(&store, "ready?");

    // Rewriting the declaring file must use the same rule as a full build.
    fs::write(
        root.path().join("record.rb"),
        SOURCE.replace("true", "false"),
    )
    .unwrap();
    update_file(root.path(), &db, "record.rb").unwrap();
    let store = GraphStore::open(&db).unwrap();
    assert_callback_owner(&store, "check");

    // A new competing definition demotes an existing edge, then resolves it again.
    fs::write(
        root.path().join("third.rb"),
        "class Third\n  def ready?; end\nend\n",
    )
    .unwrap();
    update_file(root.path(), &db, "third.rb").unwrap();
    let store = GraphStore::open(&db).unwrap();
    assert_callback_owner(&store, "ready?");

    for uid in [
        "other.rb#Other#foreign#method",
        "record.rb#Record.class_only#method",
        "record.rb#Record#show#method",
    ] {
        let target = store.symbol_by_uid(uid).unwrap().unwrap();
        assert_eq!(
            store.edges_to(target.id, None).unwrap().len(),
            0,
            "no false edge to {uid}"
        );
    }

    // A reopened class can redefine the callback. Load order is unknown.
    fs::write(
        root.path().join("reopened.rb"),
        "class Record\n  def check; end\nend\n",
    )
    .unwrap();
    update_file(root.path(), &db, "reopened.rb").unwrap();
    let store = GraphStore::open(&db).unwrap();
    for target in store.symbols_by_name("check", None, 20).unwrap() {
        assert_eq!(
            store.edges_to(target.id, None).unwrap().len(),
            0,
            "no first-candidate guess after reopening"
        );
    }
    fs::write(root.path().join("reopened.rb"), "class Record; end\n").unwrap();
    update_file(root.path(), &db, "reopened.rb").unwrap();
    let store = GraphStore::open(&db).unwrap();
    assert_callback_owner(&store, "check");
}

#[test]
fn ruby_send_should_follow_self_kind_without_linking_data_symbols() {
    let root = tempfile::tempdir().unwrap();
    let source = "class Record\n  send(:check, :payload)\n  def self.check; end\n  def check; end\n  def payload; end\n  def run\n    send(:check, :payload)\n    self.public_send(:check)\n    other.send(:payload)\n  end\n  def self.run\n    public_send(:check)\n  end\nend\n";
    fs::write(root.path().join("record.rb"), source).unwrap();
    fs::write(
        root.path().join("other.py"),
        "class Record:\n    def check(self): pass\n",
    )
    .unwrap();
    let db = root.path().join("graph.db");
    build_graph(root.path(), &db).unwrap();
    for updated in [false, true] {
        if updated {
            fs::write(
                root.path().join("other.rb"),
                "class Other\n  def check; end\nend\n",
            )
            .unwrap();
            update_file(root.path(), &db, "other.rb").unwrap();
        }
        let store = GraphStore::open(&db).unwrap();
        for (qualified, expected) in [
            ("Record#check", vec!["Record#run", "Record#run"]),
            ("Record.check", vec!["Record", "Record.run"]),
            ("Record#payload", vec![]),
        ] {
            let target = store
                .symbol_by_uid(&format!("record.rb#{qualified}#method"))
                .unwrap()
                .unwrap();
            let mut sources = store
                .edges_to(target.id, Some(EdgeKind::References))
                .unwrap()
                .into_iter()
                .map(|edge| {
                    assert_eq!(edge.tier, Tier::Probable);
                    assert_eq!(edge.callee.as_deref(), Some("check"));
                    store
                        .symbols_in_file(target.file_id)
                        .unwrap()
                        .into_iter()
                        .find(|s| s.id == edge.src_id)
                        .unwrap()
                        .qualified
                })
                .collect::<Vec<_>>();
            sources.sort();
            assert_eq!(sources, expected, "{qualified}, updated={updated}");
        }
    }
}

#[test]
fn ruby_callbacks_should_relink_a_later_definition_and_a_rewritten_target_file() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("record.rb"),
        "module Rules\n  validate :check\nend\n",
    )
    .unwrap();
    let db = root.path().join("graph.db");
    build_graph(root.path(), &db).unwrap();
    let store = GraphStore::open(&db).unwrap();
    let unresolved = store.unresolved_named("check").unwrap();
    assert_eq!(
        unresolved.len(),
        1,
        "retain a literal method reference even before any definition exists"
    );
    assert_eq!(
        (unresolved[0].kind.as_str(), unresolved[0].site_line),
        ("references", 2)
    );
    drop(store);

    for body in ["true", "false"] {
        fs::write(
            root.path().join("rules.rb"),
            format!("module Rules\n  def check; {body}; end\nend\n"),
        )
        .unwrap();
        update_file(root.path(), &db, "rules.rb").unwrap();
        let store = GraphStore::open(&db).unwrap();
        let report = impact(
            &store,
            "rules.rb#Rules#check#method",
            Direction::Upstream,
            3,
            50,
        )
        .unwrap();
        assert_eq!(
            report
                .referenced_by
                .iter()
                .map(|s| (s.uid.as_str(), s.tier.as_str()))
                .collect::<Vec<_>>(),
            [("record.rb#Rules#module", "probable")]
        );
        assert_eq!(store.unresolved_named("check").unwrap().len(), 0);
        // Ruby symbols are not identifier nodes: rename must report the site
        // as skipped instead of silently omitting a known reference.
        let target = store
            .symbol_by_uid("rules.rb#Rules#check#method")
            .unwrap()
            .unwrap();
        let plan = pixel_graph::rename::plan(&store, root.path(), &target, "verify").unwrap();
        assert_eq!(
            plan.skipped
                .iter()
                .map(|s| (s.path.as_str(), s.line))
                .collect::<Vec<_>>(),
            [("record.rb", 2)]
        );
    }
}
