// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Rails: a route references its controller's action, an association the
//! model class Active Record loads. Neither is a call, and neither falls
//! back to a same-name definition of another class. The fixtures are v1's
//! `tests/all/rails_routes.rs` tree.

use pixel_model::EdgeKind::{Association, Reference, Route};

use super::{assert_unresolved, index, labelled, resolved};

/// Each namespace reaches its own controller, an inherited action its
/// ancestor's definition, a drawn file its own handler; a controller the
/// graph lacks (`Admin::SettingsController`) leaves its route unresolved,
/// and a dynamic path or a `mount` gives none. v1
/// `a_route_query_should_return_its_declaration_and_controller_action`
/// (`assert_routes`).
#[test]
fn c10_ruby_routes_reach_the_controller_action_or_an_inherited_one() {
    let rows = index(&[
        (
            "config/routes.rb",
            "Rails.application.routes.draw do
  namespace :admin do
    resources :orders, only: [:index, :create]
    resource :settings, only: :show
  end
  resources :orders, only: [:create]
  get \"dynamic/#{part}\", to: \"dynamic#show\"
  mount Sidekiq::Web => \"/sidekiq\"
  draw :api
end
",
        ),
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
        (
            "app/controllers/admin/orders_controller.rb",
            "module Admin\n  class OrdersController < BaseController\n    def create\n    end\n  end\nend\n",
        ),
        (
            "app/controllers/orders_controller.rb",
            "class OrdersController < ApplicationController\n  def create\n  end\n  def show\n  end\n  def index\n  end\nend\n",
        ),
        (
            "app/controllers/health_controller.rb",
            "class HealthController < ApplicationController\n  def show\n  end\nend\n",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("config/routes.rb", Route, 3, "Admin::BaseController#index"),
            (
                "config/routes.rb",
                Route,
                3,
                "Admin::OrdersController#create"
            ),
            ("config/routes.rb", Route, 6, "OrdersController#create"),
            ("config/routes/api.rb", Route, 1, "HealthController#show"),
            (
                "Admin::BaseController",
                Reference,
                2,
                "ApplicationController"
            ),
            (
                "Admin::OrdersController",
                Reference,
                2,
                "Admin::BaseController"
            ),
            ("OrdersController", Reference, 1, "ApplicationController"),
            ("HealthController", Reference, 1, "ApplicationController"),
        ])
    );
    let routes: Vec<_> = super::unresolved(&rows)
        .into_iter()
        .filter(|(_, kind, _, _)| *kind == Route)
        .collect();
    assert_eq!(
        routes,
        [(
            "config/routes.rb".to_owned(),
            Route,
            4,
            "Admin::SettingsController#show".to_owned()
        )]
    );
}

/// Without its own `create`, the namespaced controller's route stays
/// unresolved: it never falls back to `OrdersController#create`. v1
/// `route_edges_should_follow_controller_edits_incrementally` (first step,
/// full-build form).
#[test]
fn c10_ruby_routes_never_fall_back_to_a_same_name_action_of_another_controller() {
    let rows = index(&[
        (
            "config/routes.rb",
            "Rails.application.routes.draw do
  namespace :admin do
    resources :orders, only: [:index, :create]
  end
  resources :orders, only: [:create]
end
",
        ),
        (
            "app/controllers/admin/base_controller.rb",
            "module Admin\n  class BaseController\n    def index\n    end\n  end\nend\n",
        ),
        (
            "app/controllers/admin/orders_controller.rb",
            "module Admin\n  class OrdersController < BaseController\n  end\nend\n",
        ),
        (
            "app/controllers/orders_controller.rb",
            "class OrdersController\n  def create\n  end\n  def index\n  end\nend\n",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("config/routes.rb", Route, 3, "Admin::BaseController#index"),
            ("config/routes.rb", Route, 5, "OrdersController#create"),
            (
                "Admin::OrdersController",
                Reference,
                2,
                "Admin::BaseController"
            ),
        ])
    );
    assert_unresolved(
        &rows,
        &[(
            "config/routes.rb",
            Route,
            3,
            "Admin::OrdersController#create",
        )],
    );
}

/// A routes file the grammar cannot parse whole gives no route: error
/// recovery can move a declaration into the block before it (here
/// `resources :users` into `namespace :admin`, after the anonymous block
/// argument tree-sitter-ruby 0.23 cannot read). v1 read the declarations of
/// such a region in the scope around them and routed `users#show` to
/// `Admin::UsersController`.
#[test]
fn c10_ruby_routes_a_region_the_grammar_cannot_parse_gives_no_route() {
    let rows = index(&[
        (
            "config/routes.rb",
            "Rails.application.routes.draw do
  namespace :admin do
    resources :orders, only: [:index]
    constraints(Subdomain) \\
      &
  end
  resources :users, only: [:show]
end
",
        ),
        (
            "app/controllers/admin/users_controller.rb",
            "module Admin\n  class UsersController\n    def show\n    end\n  end\nend\n",
        ),
        (
            "app/controllers/users_controller.rb",
            "class UsersController\n  def show\n  end\nend\n",
        ),
    ]);
    assert_eq!(resolved(&rows), labelled(&[]));
    let routes = rows
        .iter()
        .flat_map(|r| &r.edges)
        .filter(|e| e.kind == Route);
    assert_eq!(routes.count(), 0);
}

/// An inherited action follows the controller's superclass: another
/// superclass takes it away, an own definition takes its place. v1
/// `route_edges_should_follow_controller_edits_incrementally` (superclass
/// and own `index` steps, full-build form).
#[test]
fn c10_ruby_routes_an_inherited_action_follows_the_controllers_superclass() {
    let routes = (
        "config/routes.rb",
        "Rails.application.routes.draw do\n  namespace :admin do\n    resources :orders, only: [:index]\n  end\nend\n",
    );
    let base = (
        "app/controllers/admin/base_controller.rb",
        "module Admin\n  class BaseController\n    def index\n    end\n  end\nend\n",
    );
    let other_parent = (
        "app/controllers/admin/orders_controller.rb",
        "module Admin\n  class OrdersController < ApplicationController\n  end\nend\n",
    );
    let rows = index(&[routes, base, other_parent]);
    assert_eq!(resolved(&rows), labelled(&[]));
    let own_index = (
        "app/controllers/admin/orders_controller.rb",
        "module Admin\n  class OrdersController < BaseController\n    def index\n    end\n  end\nend\n",
    );
    let rows = index(&[routes, base, own_index]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            (
                "config/routes.rb",
                Route,
                3,
                "Admin::OrdersController#index"
            ),
            (
                "Admin::OrdersController",
                Reference,
                2,
                "Admin::BaseController"
            ),
        ])
    );
}

/// An association names the class Active Record loads: the owner's
/// namespace first (`Admin::Customer`), a literal `class_name:`
/// (`Billing::Invoice`, the absolute `::Note`); `through:`, `polymorphic:`
/// and a computed class name have no target. v1
/// `associations_should_reference_the_model_active_record_loads`.
#[test]
fn c10_ruby_associations_name_the_model_active_record_loads() {
    let rows = index(&[
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
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Admin::Order", Association, 3, "Admin::Customer"),
            ("Admin::Order", Association, 4, "LineItem"),
            ("Admin::Order", Association, 7, "Billing::Invoice"),
            ("Admin::Order", Association, 8, "Note"),
        ])
    );
}

/// Without a class of the owner's namespace, Active Record's lookup goes
/// outward to the top-level one. v1
/// `associations_should_reference_the_model_active_record_loads` (with
/// `admin/customer.rb` deleted).
#[test]
fn c10_ruby_associations_go_outward_when_the_owners_namespace_lacks_the_model() {
    let rows = index(&[
        (
            "app/models/admin/order.rb",
            "module Admin\n  class Order < ApplicationRecord\n    belongs_to :customer\n  end\nend\n",
        ),
        (
            "app/models/customer.rb",
            "class Customer < ApplicationRecord\nend\n",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[("Admin::Order", Association, 3, "Customer")])
    );
}

/// `draw :name` reads `config/routes/name.rb` in the scope of the draw:
/// inside `namespace :admin` its controllers are `Admin::`'s, a draw in a
/// drawn file composes with the one that drew it, and a file nothing draws
/// declares no route. A draw inside a resource is a scope the rules do not
/// carry. Action Pack 7.1.5 draws `admin/users#index`,
/// `admin/reports/summaries#daily`, `health#show`, `v2/pings#show`, and
/// nothing of `orphan.rb`.
#[test]
fn c10_ruby_routes_a_drawn_file_is_read_in_the_scope_of_its_draw() {
    // D04
    let rows = index(&[
        (
            "config/routes.rb",
            "Rails.application.routes.draw do
  namespace :admin do
    draw :admin
  end
  draw :api
  scope module: :v2 do
    draw :v2
  end
  resources :orders do
    draw :nested
  end
end
",
        ),
        (
            "config/routes/admin.rb",
            "resources :users, only: [:index]
namespace :reports do
  get \"daily\", to: \"summaries#daily\"
end
",
        ),
        (
            "config/routes/api.rb",
            "get \"status\", to: \"health#show\"\n",
        ),
        ("config/routes/v2.rb", "draw \"v2/extra\"\n"),
        (
            "config/routes/v2/extra.rb",
            "get \"ping\", to: \"pings#show\"\n",
        ),
        ("config/routes/nested.rb", "get \"x\", to: \"xs#show\"\n"),
        (
            "config/routes/orphan.rb",
            "resources :users, only: [:show]\n",
        ),
        (
            "app/controllers/users_controller.rb",
            "class UsersController\n  def index; end\n  def show; end\nend\n",
        ),
        (
            "app/controllers/admin/users_controller.rb",
            "module Admin\n  class UsersController\n    def index; end\n  end\nend\n",
        ),
        (
            "app/controllers/admin/reports/summaries_controller.rb",
            "module Admin\n  module Reports\n    class SummariesController\n      def daily; end\n    end\n  end\nend\n",
        ),
        (
            "app/controllers/health_controller.rb",
            "class HealthController\n  def show; end\nend\n",
        ),
        (
            "app/controllers/v2/pings_controller.rb",
            "module V2\n  class PingsController\n    def show; end\n  end\nend\n",
        ),
        (
            "app/controllers/xs_controller.rb",
            "class XsController\n  def show; end\nend\n",
        ),
    ]);
    let routes: Vec<_> = resolved(&rows)
        .into_iter()
        .filter(|(_, kind, _, _)| *kind == Route)
        .collect();
    assert_eq!(
        routes,
        labelled(&[
            (
                "config/routes/admin.rb",
                Route,
                1,
                "Admin::UsersController#index"
            ),
            (
                "config/routes/admin.rb",
                Route,
                3,
                "Admin::Reports::SummariesController#daily"
            ),
            ("config/routes/api.rb", Route, 1, "HealthController#show"),
            (
                "config/routes/v2/extra.rb",
                Route,
                1,
                "V2::PingsController#show"
            ),
        ])
        .into_iter()
        .collect::<Vec<_>>()
    );
    let unresolved: Vec<_> = super::unresolved(&rows)
        .into_iter()
        .filter(|(src, kind, _, _)| *kind == Route && src.starts_with("config/routes/"))
        .map(|(src, _, line, _)| (src, line))
        .collect();
    assert_eq!(
        unresolved,
        [
            ("config/routes/nested.rb".to_owned(), 1),
            ("config/routes/orphan.rb".to_owned(), 1),
        ]
    );
}
