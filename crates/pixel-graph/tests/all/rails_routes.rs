// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Rails routes are concepts a request names (`POST /admin/orders`) that
//! carry their handler, and references from the routes file to that
//! controller's action; associations reference the model class Active
//! Record would load. Both are relationships, never calls, and neither falls
//! back to a same-name definition of another class.

use std::fs;
use std::path::Path;

use pixel_graph::build::{build_graph, update_file};
use pixel_graph::concept::ConceptKind;
use pixel_graph::concept_resolve::{Confidence, ResolveOptions, resolve};
use pixel_graph::impact::{Direction, impact};
use pixel_graph::{EdgeKind, GraphStore};

const ROUTES: &str = "Rails.application.routes.draw do
  namespace :admin do
    resources :orders, only: [:index, :create]
    resource :settings, only: :show
  end
  resources :orders, only: [:create]
  get \"dynamic/#{part}\", to: \"dynamic#show\"
  mount Sidekiq::Web => \"/sidekiq\"
  draw :api
end
";

const ADMIN_ORDERS: &str = "module Admin
  class OrdersController < BaseController
    def create
    end
  end
end
";

const TREE: &[(&str, &str)] = &[
    ("config/routes.rb", ROUTES),
    (
        "config/routes/api.rb",
        "get \"status\", to: \"health#show\"\n",
    ),
    (
        "app/controllers/application_controller.rb",
        "class ApplicationController < ActionController::Base\nend\n",
    ),
    (
        "app/controllers/admin/base_controller.rb",
        "module Admin\n  class BaseController < ApplicationController\n    def index\n    end\n  end\nend\n",
    ),
    ("app/controllers/admin/orders_controller.rb", ADMIN_ORDERS),
    (
        "app/controllers/orders_controller.rb",
        "class OrdersController < ApplicationController\n  def create\n  end\n  def show\n  end\n  def index\n  end\nend\n",
    ),
    (
        "app/controllers/health_controller.rb",
        "class HealthController < ApplicationController\n  def show\n  end\nend\n",
    ),
    (
        "app/models/admin/order.rb",
        "module Admin
  class Order < ApplicationRecord
    belongs_to :customer
    has_many :line_items
    has_many :tags, through: :taggings
    belongs_to :commentable, polymorphic: true
    has_one :invoice, class_name: \"Billing::Invoice\"
    has_many :notes, class_name: \"::Note\"
    belongs_to :owner, class_name: owner_class
  end
end
",
    ),
    (
        "app/models/customer.rb",
        "class Customer < ApplicationRecord\nend\n",
    ),
    (
        "app/models/admin/customer.rb",
        "module Admin\n  class Customer < ApplicationRecord\n  end\nend\n",
    ),
    (
        "app/models/line_item.rb",
        "class LineItem < ApplicationRecord\nend\n",
    ),
    ("app/models/tag.rb", "class Tag < ApplicationRecord\nend\n"),
    (
        "app/models/invoice.rb",
        "class Invoice < ApplicationRecord\nend\n",
    ),
    (
        "app/models/billing/invoice.rb",
        "module Billing\n  class Invoice < ApplicationRecord\n  end\nend\n",
    ),
    (
        "app/models/note.rb",
        "class Note < ApplicationRecord\nend\n",
    ),
    (
        "app/models/admin/note.rb",
        "module Admin\n  class Note < ApplicationRecord\n  end\nend\n",
    ),
    (
        "app/models/owner.rb",
        "class Owner < ApplicationRecord\nend\n",
    ),
];

const ROUTES_UID: &str = "config/routes.rb#config/routes.rb#script";

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn build() -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    for (rel, body) in TREE {
        write(root.path(), rel, body);
    }
    let db = root.path().join("graph.db");
    build_graph(root.path(), &db).unwrap();
    (root, db)
}

fn qualified(store: &GraphStore, id: i64) -> String {
    store
        .conn()
        .query_row("SELECT qualified FROM symbols WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .unwrap()
}

/// `(source qualified, site line)` of every reference edge into `uid`.
fn referenced_by(store: &GraphStore, uid: &str) -> Vec<(String, u32)> {
    let target = store
        .symbol_by_uid(uid)
        .unwrap()
        .unwrap_or_else(|| panic!("{uid} missing"));
    let mut out: Vec<(String, u32)> = store
        .edges_to(target.id, None)
        .unwrap()
        .into_iter()
        .map(|e| {
            assert_eq!(e.kind, EdgeKind::References, "{uid}: never a call");
            (qualified(store, e.src_id), e.site_line)
        })
        .collect();
    out.sort();
    out
}

fn action(path: &str, qualified: &str) -> String {
    format!("{path}#{qualified}#method")
}

fn assert_routes(db: &Path) {
    let store = GraphStore::open(db).unwrap();
    let routes = "config/routes.rb".to_string();
    // Each namespace reaches its own controller, an inherited action its
    // ancestor's definition, a drawn file its own handler.
    for (uid, want) in [
        (
            action(
                "app/controllers/admin/orders_controller.rb",
                "Admin::OrdersController#create",
            ),
            vec![(routes.clone(), 3)],
        ),
        (
            action(
                "app/controllers/admin/base_controller.rb",
                "Admin::BaseController#index",
            ),
            vec![(routes.clone(), 3)],
        ),
        (
            action(
                "app/controllers/orders_controller.rb",
                "OrdersController#create",
            ),
            vec![(routes.clone(), 6)],
        ),
        (
            action(
                "app/controllers/health_controller.rb",
                "HealthController#show",
            ),
            vec![("config/routes/api.rb".to_string(), 1)],
        ),
        // The missing `Admin::SettingsController#show` is not replaced by
        // another `show`, nor `admin/orders#index` by `OrdersController`'s.
        (
            action(
                "app/controllers/orders_controller.rb",
                "OrdersController#show",
            ),
            vec![],
        ),
        (
            action(
                "app/controllers/orders_controller.rb",
                "OrdersController#index",
            ),
            vec![],
        ),
    ] {
        assert_eq!(referenced_by(&store, &uid), want, "{uid}");
    }
    let unresolved: Vec<(String, Option<String>)> = store
        .conn()
        .prepare(
            "SELECT u.name, u.receiver FROM unresolved_calls u JOIN files f ON f.id = u.file_id
              WHERE f.path = 'config/routes.rb' AND u.kind = 'references' ORDER BY u.site_line",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        unresolved,
        [(
            "show".to_string(),
            Some(":route Admin::SettingsController".to_string())
        )]
    );
}

#[test]
fn a_route_query_should_return_its_declaration_and_controller_action() {
    let (_root, db) = build();
    let store = GraphStore::open(&db).unwrap();
    let outcome = resolve(&store, "POST /admin/orders", &ResolveOptions::default()).unwrap();
    assert_eq!(outcome.confidence, Confidence::Resolved);
    let first = &outcome.matches[0];
    assert_eq!(
        (
            first.kind,
            first.raw.as_str(),
            first.path.as_str(),
            first.start_line,
            first.detail.as_str()
        ),
        (
            ConceptKind::Route,
            "POST /admin/orders",
            "config/routes.rb",
            3,
            "admin/orders#create (Admin::OrdersController#create)"
        )
    );
    let concepts: Vec<(String, String)> = store
        .conn()
        .prepare("SELECT raw, detail FROM concepts WHERE kind = 'route' ORDER BY start_line, raw")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let pair = |raw: &str, detail: &str| (raw.to_string(), detail.to_string());
    assert_eq!(
        concepts,
        [
            pair("GET /status", "health#show (HealthController#show)"),
            pair(
                "GET /admin/orders",
                "admin/orders#index (Admin::OrdersController#index)"
            ),
            pair(
                "POST /admin/orders",
                "admin/orders#create (Admin::OrdersController#create)"
            ),
            pair(
                "GET /admin/settings",
                "admin/settings#show (Admin::SettingsController#show)"
            ),
            pair("POST /orders", "orders#create (OrdersController#create)"),
            pair("MOUNT /sidekiq", "mount Sidekiq::Web"),
        ],
        "a dynamic path yields no route"
    );
    assert_routes(&db);
    // Changing the action shows the route through impact.
    let report = impact(
        &store,
        &action(
            "app/controllers/admin/orders_controller.rb",
            "Admin::OrdersController#create",
        ),
        Direction::Upstream,
        1,
        50,
    )
    .unwrap();
    assert_eq!(
        report
            .referenced_by
            .iter()
            .map(|s| (s.uid.as_str(), s.tier.as_str()))
            .collect::<Vec<_>>(),
        [(ROUTES_UID, "probable")]
    );
}

#[test]
fn route_edges_should_follow_controller_edits_incrementally() {
    let (root, db) = build();
    let path = "app/controllers/admin/orders_controller.rb";
    write(
        root.path(),
        path,
        &ADMIN_ORDERS.replace("    def create\n    end\n", ""),
    );
    update_file(root.path(), &db, path).unwrap();
    {
        let store = GraphStore::open(&db).unwrap();
        assert_eq!(
            referenced_by(
                &store,
                &action(
                    "app/controllers/orders_controller.rb",
                    "OrdersController#create"
                )
            ),
            [("config/routes.rb".to_string(), 6)],
            "the namespaced route never falls back to the root controller"
        );
    }
    write(root.path(), path, ADMIN_ORDERS);
    update_file(root.path(), &db, path).unwrap();
    assert_routes(&db);

    // The inherited `index` follows the controller's ancestors: another
    // superclass takes it away, an own definition takes its place.
    let base_index = action(
        "app/controllers/admin/base_controller.rb",
        "Admin::BaseController#index",
    );
    write(
        root.path(),
        path,
        &ADMIN_ORDERS.replace("< BaseController", "< ApplicationController"),
    );
    update_file(root.path(), &db, path).unwrap();
    {
        let store = GraphStore::open(&db).unwrap();
        assert_eq!(referenced_by(&store, &base_index), []);
    }
    write(
        root.path(),
        path,
        &ADMIN_ORDERS.replace(
            "    def create\n",
            "    def index\n    end\n    def create\n",
        ),
    );
    update_file(root.path(), &db, path).unwrap();
    {
        let store = GraphStore::open(&db).unwrap();
        assert_eq!(referenced_by(&store, &base_index), []);
        assert_eq!(
            referenced_by(&store, &action(path, "Admin::OrdersController#index")),
            [("config/routes.rb".to_string(), 3)]
        );
    }
    write(root.path(), path, ADMIN_ORDERS);
    update_file(root.path(), &db, path).unwrap();
    assert_routes(&db);
    write(root.path(), "config/routes.rb", &format!("{ROUTES}\n"));
    update_file(root.path(), &db, "config/routes.rb").unwrap();
    assert_routes(&db);
}

/// `(callee, target qualified)` of every association reference of
/// `Admin::Order`, sorted.
fn associations(db: &Path) -> Vec<(String, String)> {
    let store = GraphStore::open(db).unwrap();
    let order = store
        .symbol_by_uid("app/models/admin/order.rb#Admin::Order#class")
        .unwrap()
        .unwrap();
    let mut out: Vec<(String, String)> = store
        .edges_from(order.id, None)
        .unwrap()
        .into_iter()
        .map(|e| {
            assert_eq!(e.kind, EdgeKind::References);
            (e.callee.unwrap_or_default(), qualified(&store, e.dst_id))
        })
        .collect();
    out.sort();
    out
}

fn pair(a: &str, b: &str) -> (String, String) {
    (a.to_string(), b.to_string())
}

#[test]
fn associations_should_reference_the_model_active_record_loads() {
    let (root, db) = build();
    // The owner's namespace first (`Admin::Customer`, not `Customer`), the
    // literal `class_name:` (`Billing::Invoice`, absolute `::Note`); no
    // target for `through:` or `polymorphic:`, or a computed class name.
    let want = vec![
        pair("Customer", "Admin::Customer"),
        pair("Invoice", "Billing::Invoice"),
        pair("LineItem", "LineItem"),
        pair("Note", "Note"),
    ];
    assert_eq!(associations(&db), want);
    {
        let store = GraphStore::open(&db).unwrap();
        for uid in [
            "app/models/tag.rb#Tag#class",
            "app/models/customer.rb#Customer#class",
            "app/models/invoice.rb#Invoice#class",
            "app/models/admin/note.rb#Admin::Note#class",
            "app/models/owner.rb#Owner#class",
        ] {
            let target = store.symbol_by_uid(uid).unwrap().unwrap();
            assert_eq!(store.edges_to(target.id, None).unwrap().len(), 0, "{uid}");
        }
    }
    // Without the namespaced model, the top-level one is the target; back
    // with it, the namespaced one again.
    fs::remove_file(root.path().join("app/models/admin/customer.rb")).unwrap();
    update_file(root.path(), &db, "app/models/admin/customer.rb").unwrap();
    assert_eq!(
        associations(&db),
        [
            pair("Customer", "Customer"),
            pair("Invoice", "Billing::Invoice"),
            pair("LineItem", "LineItem"),
            pair("Note", "Note"),
        ]
    );
    write(
        root.path(),
        "app/models/admin/customer.rb",
        "module Admin\n  class Customer < ApplicationRecord\n  end\nend\n",
    );
    update_file(root.path(), &db, "app/models/admin/customer.rb").unwrap();
    assert_eq!(associations(&db), want);
}
