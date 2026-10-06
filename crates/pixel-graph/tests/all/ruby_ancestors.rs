// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Ruby calls on `self` follow the caller's declared ancestors in Ruby's
//! lookup order: prepended modules, the owner, included modules and concerns,
//! the superclass; class methods through `extend` and a concern's
//! `class_methods`. Every chain the graph cannot prove abstains.

use std::fs;
use std::path::Path;

use pixel_graph::build::{build_graph, update_file};
use pixel_graph::impact::{Direction, impact};
use pixel_graph::{EdgeKind, GraphStore, Tier};

const TREE: &[(&str, &str)] = &[
    (
        "app/models/concerns/admin/trackable.rb",
        "module Admin
  module Trackable
    extend ActiveSupport::Concern

    included do
      include Admin::Auditable
      before_save :stamp
    end

    class_methods do
      def tracked_since
      end
    end

    def track
    end

    def stamp
    end
  end
end
",
    ),
    (
        "app/models/concerns/admin/auditable.rb",
        "module Admin
  module Auditable
    def audit
    end
  end
end
",
    ),
    (
        "app/models/concerns/billing/trackable.rb",
        "module Billing
  module Trackable
    def track
    end

    def audit
    end

    def tracked_since
    end
  end
end
",
    ),
    (
        "app/models/concerns/loud.rb",
        "module Loud
  def save
  end
end
",
    ),
    (
        "app/models/concerns/sortable.rb",
        "module Sortable
  def sort_key
  end
end
",
    ),
    (
        "app/models/application_record.rb",
        "class ApplicationRecord < ActiveRecord::Base
  def persisted_label
  end
end
",
    ),
    (
        "app/models/admin/order.rb",
        "module Admin
  class Order < ApplicationRecord
    include Trackable
    prepend Loud
    extend Sortable

    def save
    end

    def checkout
      track
      audit
      save
      persisted_label
      tracked_since
      self.track
    end

    def self.report
      tracked_since
      sort_key
      track
    end
  end
end
",
    ),
    (
        "app/models/admin/invoice.rb",
        "module Admin
  class Invoice < ApplicationRecord
    include Trackable
    alias_method :label, :persisted_label

    def track
    end

    def close
      track
    end
  end
end
",
    ),
    (
        "app/models/cycle.rb",
        "module Ping
  include Pong
  def ping
  end
end
module Pong
  include Ping
  def pong
  end
end
class Cycler
  include Ping
  def run
    pong
    ping
    never_defined
  end
end
",
    ),
    (
        "app/models/superclass_cycle.rb",
        "class Alpha < Beta
  def run
    never_defined
  end
end
class Beta < Alpha
end
",
    ),
    (
        "app/reports/sales.rb",
        "class SalesReport
  def build
    Admin::Order.sort_key
  end
end
",
    ),
    (
        "app/models/dynamic.rb",
        "class Dynamic
  include Sortable
  include(PLUGINS.first)

  def run
    sort_key
  end
end
",
    ),
];

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn qualified(store: &GraphStore, id: i64) -> String {
    store
        .conn()
        .query_row("SELECT qualified FROM symbols WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .unwrap()
}

/// `(callee name, target qualified name, tier)` of every call edge from the
/// method `caller` (a uid), sorted.
fn calls_from(store: &GraphStore, caller: &str) -> Vec<(String, String, Tier)> {
    let src = store.symbol_by_uid(caller).unwrap().unwrap();
    let mut edges: Vec<(String, String, Tier)> = store
        .edges_from(src.id, Some(EdgeKind::Calls))
        .unwrap()
        .into_iter()
        .map(|e| {
            (
                e.callee.clone().unwrap_or_default(),
                qualified(store, e.dst_id),
                e.tier,
            )
        })
        .collect();
    edges.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    edges
}

fn unresolved_from(store: &GraphStore, caller: &str) -> Vec<String> {
    let src = store.symbol_by_uid(caller).unwrap().unwrap();
    store
        .conn()
        .prepare("SELECT name FROM unresolved_calls WHERE enclosing_symbol_id = ?1 ORDER BY name, site_line")
        .unwrap()
        .query_map([src.id], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn edge(name: &str, target: &str, tier: Tier) -> (String, String, Tier) {
    (name.to_string(), target.to_string(), tier)
}

const ORDER: &str = "app/models/admin/order.rb";

fn uid(path: &str, qualified: &str) -> String {
    format!("{path}#{qualified}#method")
}

fn assert_chains(db: &Path) {
    let store = GraphStore::open(db).unwrap();
    // The lexical `Trackable` in `Admin::Order` is `Admin::Trackable`, never
    // the same-tail `Billing::Trackable`; `audit` comes from what the
    // concern's `included` block includes; the prepended `Loud#save` wins
    // over the owner's own `save`; `persisted_label` from the superclass.
    // The class method `tracked_since` is not an instance method: no edge.
    assert_eq!(
        calls_from(&store, &uid(ORDER, "Admin::Order#checkout")),
        [
            edge("audit", "Admin::Auditable#audit", Tier::Probable),
            edge(
                "persisted_label",
                "ApplicationRecord#persisted_label",
                Tier::Probable
            ),
            edge("save", "Loud#save", Tier::Probable),
            edge("track", "Admin::Trackable#track", Tier::Probable),
            edge("track", "Admin::Trackable#track", Tier::Probable),
        ]
    );
    assert_eq!(
        unresolved_from(&store, &uid(ORDER, "Admin::Order#checkout")),
        ["tracked_since"]
    );
    // Class side: the concern's `class_methods`, then what the class
    // extends; an instance method is not a class method.
    assert_eq!(
        calls_from(&store, &uid(ORDER, "Admin::Order.report")),
        [
            edge("sort_key", "Sortable#sort_key", Tier::Probable),
            edge(
                "tracked_since",
                "Admin::Trackable::ClassMethods#tracked_since",
                Tier::Probable
            ),
        ]
    );
    assert_eq!(
        unresolved_from(&store, &uid(ORDER, "Admin::Order.report")),
        ["track"]
    );
    // An owner's own definition overrides the included one, and stays exact.
    assert_eq!(
        calls_from(
            &store,
            &uid("app/models/admin/invoice.rb", "Admin::Invoice#close")
        ),
        [edge("track", "Admin::Invoice#track", Tier::Exact)]
    );
    // An alias of an inherited method references the ancestor's.
    let inherited = store
        .symbol_by_uid(&uid(
            "app/models/application_record.rb",
            "ApplicationRecord#persisted_label",
        ))
        .unwrap()
        .unwrap();
    assert_eq!(
        store
            .edges_to(inherited.id, Some(EdgeKind::References))
            .unwrap()
            .iter()
            .map(|e| qualified(&store, e.src_id))
            .collect::<Vec<_>>(),
        ["Admin::Invoice#label"]
    );
    // Mutual includes terminate; what neither defines stays unresolved.
    let cycler = uid("app/models/cycle.rb", "Cycler#run");
    assert_eq!(
        calls_from(&store, &cycler),
        [
            edge("ping", "Ping#ping", Tier::Probable),
            edge("pong", "Pong#pong", Tier::Probable),
        ]
    );
    assert_eq!(unresolved_from(&store, &cycler), ["never_defined"]);
    // A superclass cycle terminates and proves nothing.
    let cycle = uid("app/models/superclass_cycle.rb", "Alpha#run");
    assert_eq!(calls_from(&store, &cycle), []);
    assert_eq!(unresolved_from(&store, &cycle), ["never_defined"]);
    // A constant receiver reaches what its class extends.
    assert_eq!(
        calls_from(&store, &uid("app/reports/sales.rb", "SalesReport#build")),
        [edge("sort_key", "Sortable#sort_key", Tier::Probable)]
    );
    // A module included after `Sortable` that the graph cannot name may
    // define `sort_key` first: no target, although only one definition
    // exists.
    let dynamic = uid("app/models/dynamic.rb", "Dynamic#run");
    assert_eq!(calls_from(&store, &dynamic), []);
    assert_eq!(unresolved_from(&store, &dynamic), ["sort_key"]);
    // A concern's own `before_save` callback stays on the concern.
    let stamp = store
        .symbol_by_uid(&uid(
            "app/models/concerns/admin/trackable.rb",
            "Admin::Trackable#stamp",
        ))
        .unwrap()
        .unwrap();
    assert_eq!(
        store
            .edges_to(stamp.id, Some(EdgeKind::References))
            .unwrap()
            .iter()
            .map(|e| qualified(&store, e.src_id))
            .collect::<Vec<_>>(),
        ["Admin::Trackable"]
    );
    // Nothing reaches the same-tail Billing methods.
    for method in [
        "Billing::Trackable#track",
        "Billing::Trackable#audit",
        "Billing::Trackable#tracked_since",
    ] {
        let target = store
            .symbol_by_uid(&uid("app/models/concerns/billing/trackable.rb", method))
            .unwrap()
            .unwrap();
        assert_eq!(
            store.edges_to(target.id, None).unwrap().len(),
            0,
            "{method}"
        );
    }
}

#[test]
fn ruby_self_calls_should_follow_the_declared_ancestor_chain() {
    let root = tempfile::tempdir().unwrap();
    for (rel, body) in TREE {
        write(root.path(), rel, body);
    }
    let db = root.path().join("graph.db");
    build_graph(root.path(), &db).unwrap();
    assert_chains(&db);

    // Impact on the concern's method names its consumers' call sites.
    let store = GraphStore::open(&db).unwrap();
    let report = impact(
        &store,
        &uid(
            "app/models/concerns/admin/trackable.rb",
            "Admin::Trackable#track",
        ),
        Direction::Upstream,
        1,
        50,
    )
    .unwrap();
    assert_eq!(
        report
            .d1_will_break
            .iter()
            .map(|item| (item.uid.as_str(), item.tier.as_str()))
            .collect::<Vec<_>>(),
        [(uid(ORDER, "Admin::Order#checkout").as_str(), "probable")]
    );
}

#[test]
fn ruby_ancestor_edges_should_follow_incremental_changes() {
    let root = tempfile::tempdir().unwrap();
    for (rel, body) in TREE {
        write(root.path(), rel, body);
    }
    let db = root.path().join("graph.db");
    build_graph(root.path(), &db).unwrap();
    let checkout = uid(ORDER, "Admin::Order#checkout");
    let track = || {
        let store = GraphStore::open(&db).unwrap();
        calls_from(&store, &checkout)
            .into_iter()
            .filter(|(name, _, _)| name == "track")
            .map(|(_, target, _)| target)
            .collect::<Vec<_>>()
    };
    let concern = TREE[0].1;

    // Removing the concern's method never falls back to the same-tail one.
    write(
        root.path(),
        TREE[0].0,
        &concern.replace("    def track\n    end\n", ""),
    );
    update_file(root.path(), &db, TREE[0].0).unwrap();
    assert_eq!(track(), Vec::<String>::new());
    write(root.path(), TREE[0].0, concern);
    update_file(root.path(), &db, TREE[0].0).unwrap();
    assert_eq!(
        track(),
        ["Admin::Trackable#track", "Admin::Trackable#track"]
    );

    // A reopening in another file includes a second definition whose order
    // against the first the graph cannot prove: no target.
    let reopened = "app/models/admin/order_billing.rb";
    write(
        root.path(),
        reopened,
        "module Admin\n  class Order\n    include Billing::Trackable\n  end\nend\n",
    );
    update_file(root.path(), &db, reopened).unwrap();
    assert_eq!(track(), Vec::<String>::new());
    fs::remove_file(root.path().join(reopened)).unwrap();
    update_file(root.path(), &db, reopened).unwrap();
    assert_eq!(
        track(),
        ["Admin::Trackable#track", "Admin::Trackable#track"]
    );

    // Dropping the `include` from the consumer drops the edge; restoring it
    // in a later update brings it back. Every other chain is unchanged.
    let order = TREE.iter().find(|(rel, _)| *rel == ORDER).unwrap().1;
    write(
        root.path(),
        ORDER,
        &order.replace("    include Trackable\n", "\n"),
    );
    update_file(root.path(), &db, ORDER).unwrap();
    assert_eq!(track(), Vec::<String>::new());
    write(root.path(), ORDER, order);
    update_file(root.path(), &db, ORDER).unwrap();
    assert_chains(&db);

    // Dropping the `extend` from the class moves the call another file makes
    // on its constant, although no definition changed.
    let sales = uid("app/reports/sales.rb", "SalesReport#build");
    write(
        root.path(),
        ORDER,
        &order.replace("    extend Sortable\n", "\n"),
    );
    update_file(root.path(), &db, ORDER).unwrap();
    {
        let store = GraphStore::open(&db).unwrap();
        assert_eq!(calls_from(&store, &sales), []);
        assert_eq!(unresolved_from(&store, &sales), ["sort_key"]);
    }
    write(root.path(), ORDER, order);
    update_file(root.path(), &db, ORDER).unwrap();
    assert_chains(&db);

    // A new constant that shadows the lexical `Trackable` moves the chain.
    let shadow = "app/models/admin/order/trackable.rb";
    write(
        root.path(),
        shadow,
        "module Admin\n  class Order\n    module Trackable\n      def track\n      end\n    end\n  end\nend\n",
    );
    update_file(root.path(), &db, shadow).unwrap();
    assert_eq!(
        track(),
        [
            "Admin::Order::Trackable#track",
            "Admin::Order::Trackable#track"
        ]
    );
    fs::remove_file(root.path().join(shadow)).unwrap();
    update_file(root.path(), &db, shadow).unwrap();
    assert_chains(&db);
}
