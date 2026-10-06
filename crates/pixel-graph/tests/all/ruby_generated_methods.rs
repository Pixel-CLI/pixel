// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Methods Ruby and Rails generate from a declaration (`attr_*`,
//! `alias_method`, `alias`, `delegate`, `scope`) are symbols of their owner,
//! spanning the declaration, and calls reach them like any `def`.

use std::fs;
use std::path::Path;

use pixel_graph::build::{build_graph, update_file};
use pixel_graph::extract::extract_file;
use pixel_graph::{EdgeKind, GraphStore, Tier};

const ACCOUNT: &str = "\
module Admin
  class Account
    attr_reader :token
    attr_writer :secret
    attr_accessor :name, \"email\"
    attr_reader(*FIELDS)
    attr_reader :\"dyn_#{x}\"
    attr_reader :status
    alias_method :label, :name
    alias title name
    alias $old_global $new_global
    delegate :street, :city, to: :address
    delegate :email, to: :owner, prefix: true
    delegate :zip, to: :address, prefix: :home
    delegate :currency, to: :@config
    delegate :size, to: :class
    delegate :x, to: :address, prefix: some_prefix
    scope :active, -> { where(active: true) }

    class << self
      attr_accessor :registry
    end

    def status
      @status || :new
    end

    def address; end
    def owner; end

    def rename(value)
      self.name = value
      self.secret = value
      self.token = value
      name
      label
      self.class.registry
    end

    def bump
      self.name += \"!\"
    end
  end
end
";

/// `(qualified, start_line, end_line)` of every method the file defines.
fn methods(source: &str) -> Vec<(String, u32, u32)> {
    let fx = extract_file("app/models/admin/account.rb", source.as_bytes()).unwrap();
    fx.symbols
        .iter()
        .filter(|s| s.kind == pixel_graph::SymbolKind::Method)
        .map(|s| (s.qualified.clone(), s.start_line, s.end_line))
        .collect()
}

#[test]
fn ruby_declarations_should_generate_methods_of_their_owner_at_the_declaring_line() {
    let got = methods(ACCOUNT);
    let at = |q: &str| -> Vec<(u32, u32)> {
        got.iter()
            .filter(|(name, _, _)| name == q)
            .map(|(_, start, end)| (*start, *end))
            .collect()
    };
    for (qualified, line) in [
        ("Admin::Account#token", 3),
        ("Admin::Account#secret=", 4),
        ("Admin::Account#name", 5),
        ("Admin::Account#name=", 5),
        ("Admin::Account#email", 5),
        ("Admin::Account#email=", 5),
        ("Admin::Account#label", 9),
        ("Admin::Account#title", 10),
        ("Admin::Account#street", 12),
        ("Admin::Account#city", 12),
        ("Admin::Account#owner_email", 13),
        ("Admin::Account#home_zip", 14),
        ("Admin::Account#currency", 15),
        ("Admin::Account#size", 16),
        ("Admin::Account.active", 18),
        ("Admin::Account.registry", 21),
        ("Admin::Account.registry=", 21),
    ] {
        assert_eq!(at(qualified), [(line, line)], "{qualified}");
    }
    // Reader-only and writer-only accessors define one side.
    assert_eq!(at("Admin::Account#token="), []);
    assert_eq!(at("Admin::Account#secret"), []);
    // The explicit `def status` overrides the generated reader; one symbol.
    assert_eq!(at("Admin::Account#status"), [(24, 26)]);
    // Dynamic names, a global alias and a dynamic prefix define nothing the
    // extractor can name; nothing is guessed in their place.
    for absent in [
        "Admin::Account#FIELDS",
        "Admin::Account#dyn_",
        "Admin::Account#$old_global",
        "Admin::Account#x",
        "Admin::Account#some_prefix_x",
        "Admin::Account#registry",
        "Admin::Account#street=",
        "Admin::Account#active",
    ] {
        assert!(
            !got.iter().any(|(q, _, _)| q.starts_with(absent)),
            "{absent} in {got:#?}"
        );
    }
}

#[test]
fn ruby_declarations_outside_an_owner_body_should_generate_nothing() {
    let source = "attr_reader :top\ndef helper\n  attr_reader :inside_def\nend\nclass Box\n  def build\n    delegate :a, to: :b\n  end\nend\n";
    let got = methods(source);
    assert_eq!(
        got.iter().map(|(q, _, _)| q.as_str()).collect::<Vec<_>>(),
        ["Box#build"],
        "a declaration evaluated in a method or at the top level has no known owner"
    );
}

fn edges_into(store: &GraphStore, uid: &str, kind: EdgeKind) -> Vec<(String, Tier)> {
    let target = store
        .symbol_by_uid(uid)
        .unwrap()
        .unwrap_or_else(|| panic!("{uid} missing"));
    let mut sources: Vec<(String, Tier)> = store
        .edges_to(target.id, Some(kind))
        .unwrap()
        .into_iter()
        .map(|edge| {
            let src: String = store
                .conn()
                .query_row(
                    "SELECT qualified FROM symbols WHERE id = ?1",
                    [edge.src_id],
                    |r| r.get(0),
                )
                .unwrap();
            (src, edge.tier)
        })
        .collect();
    sources.sort_by(|a, b| a.0.cmp(&b.0));
    sources
}

fn assert_navigation(root: &Path) {
    let store = GraphStore::open(&root.join("graph.db")).unwrap();
    let uid = |q: &str| format!("app/models/admin/account.rb#{q}#method");
    // Writer and reader are distinct targets: `self.name = v` reaches the
    // writer, the bare `name` the reader, `self.name += v` both.
    assert_eq!(
        edges_into(&store, &uid("Admin::Account#name="), EdgeKind::Calls),
        [
            ("Admin::Account#bump".to_string(), Tier::Exact),
            ("Admin::Account#rename".to_string(), Tier::Exact)
        ]
    );
    assert_eq!(
        edges_into(&store, &uid("Admin::Account#name"), EdgeKind::Calls),
        [
            ("Admin::Account#bump".to_string(), Tier::Exact),
            ("Admin::Account#rename".to_string(), Tier::Exact)
        ]
    );
    assert_eq!(
        edges_into(&store, &uid("Admin::Account#secret="), EdgeKind::Calls),
        [("Admin::Account#rename".to_string(), Tier::Exact)]
    );
    // `self.token = v` has no writer to reach: the reader is not its target.
    assert_eq!(
        edges_into(&store, &uid("Admin::Account#token"), EdgeKind::Calls),
        []
    );
    // The alias is called, and references the method it copies.
    assert_eq!(
        edges_into(&store, &uid("Admin::Account#label"), EdgeKind::Calls),
        [("Admin::Account#rename".to_string(), Tier::Exact)]
    );
    assert_eq!(
        edges_into(&store, &uid("Admin::Account#name"), EdgeKind::References),
        [
            ("Admin::Account#label".to_string(), Tier::Probable),
            ("Admin::Account#title".to_string(), Tier::Probable)
        ]
    );
    // Delegators reference their own owner's `to:` method, never a method of
    // the same name elsewhere (`Billing::Account#owner`), and never the
    // delegate's own method, whose type is unknown.
    assert_eq!(
        edges_into(&store, &uid("Admin::Account#owner"), EdgeKind::References),
        [("Admin::Account#owner_email".to_string(), Tier::Probable)]
    );
    assert_eq!(
        edges_into(&store, &uid("Admin::Account#address"), EdgeKind::References),
        [
            ("Admin::Account#city".to_string(), Tier::Probable),
            ("Admin::Account#home_zip".to_string(), Tier::Probable),
            ("Admin::Account#street".to_string(), Tier::Probable)
        ]
    );
    assert_eq!(
        edges_into(
            &store,
            "app/models/billing/account.rb#Billing::Account#owner#method",
            EdgeKind::References
        ),
        []
    );
    assert_eq!(
        edges_into(
            &store,
            "app/models/billing/account.rb#Billing::Account#name#method",
            EdgeKind::Calls
        ),
        [],
        "a same-tail owner in another namespace is never the target"
    );
    // The scope's lambda body belongs to the generated class method.
    let scope = store
        .symbol_by_uid(&uid("Admin::Account.active"))
        .unwrap()
        .unwrap();
    let unresolved: Vec<String> = store
        .conn()
        .prepare("SELECT name FROM unresolved_calls WHERE enclosing_symbol_id = ?1 ORDER BY name")
        .unwrap()
        .query_map([scope.id], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(unresolved, ["where"]);
}

#[test]
fn ruby_generated_methods_should_be_navigable_after_full_and_incremental_builds() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("app/models/admin")).unwrap();
    fs::create_dir_all(root.path().join("app/models/billing")).unwrap();
    fs::write(root.path().join("app/models/admin/account.rb"), ACCOUNT).unwrap();
    fs::write(
        root.path().join("app/models/billing/account.rb"),
        "module Billing\n  class Account\n    attr_accessor :name\n    def owner; end\n  end\nend\n",
    )
    .unwrap();
    let db = root.path().join("graph.db");
    build_graph(root.path(), &db).unwrap();
    assert_navigation(root.path());

    // Re-extracting the declaring file through the incremental path stores
    // the same generated methods and edges.
    fs::write(
        root.path().join("app/models/admin/account.rb"),
        format!("{ACCOUNT}\n"),
    )
    .unwrap();
    update_file(root.path(), &db, "app/models/admin/account.rb").unwrap();
    assert_navigation(root.path());

    // A competing same-name definition elsewhere leaves the owner-relative
    // targets in place.
    fs::write(
        root.path().join("app/models/other.rb"),
        "class Other\n  attr_accessor :name\n  def owner; end\n  def address; end\nend\n",
    )
    .unwrap();
    update_file(root.path(), &db, "app/models/other.rb").unwrap();
    assert_navigation(root.path());
}

#[test]
fn ruby_quoted_symbols_should_name_methods_and_overridden_declarations_forward_nothing() {
    let source = "class Card
  attr_reader :\"display_name\"
  alias :\"label\" :\"display_name\"
  alias_method :title, :display_name
  alias_method :title, :label
  alias_method :heading, :display_name
  def heading
  end
end
";
    let fx = extract_file("app/models/card.rb", source.as_bytes()).unwrap();
    let mut qualified: Vec<&str> = fx
        .symbols
        .iter()
        .filter(|s| s.kind == pixel_graph::SymbolKind::Method)
        .map(|s| s.qualified.as_str())
        .collect();
    qualified.sort_unstable();
    assert_eq!(
        qualified,
        [
            "Card#display_name",
            "Card#heading",
            "Card#label",
            "Card#title"
        ]
    );
    // The later `alias_method :title` and the `def heading` win: the first
    // `title` alias and the `heading` alias leave no reference behind.
    let mut forwards: Vec<(String, Option<String>)> = fx
        .references
        .iter()
        .map(|r| {
            (
                r.name.clone(),
                r.enclosing_index.map(|i| fx.symbols[i].qualified.clone()),
            )
        })
        .collect();
    forwards.sort();
    assert_eq!(
        forwards,
        [
            ("display_name".to_string(), Some("Card#label".to_string())),
            ("label".to_string(), Some("Card#title".to_string())),
        ]
    );
}
