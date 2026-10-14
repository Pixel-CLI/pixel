// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The routes walker, on v1's fixtures (`crates/pixel-graph/src/extract/
//! ruby_routes.rs` tests, `tests/all/rails_routes.rs`) and on the shapes of
//! once-campfire's routes file.

use super::*;
use crate::ruby::extract::testing::{routes as route_sites, rows};
use crate::ruby::extract::{PARSE_BUDGET, parse};

/// Every route of `source` as `VERB /path controller#action (Constant#action)`,
/// or `VERB /path mount Constant`, in route order (v1's `table`).
fn table(source: &str) -> Vec<String> {
    let tree = parse(source.as_bytes(), PARSE_BUDGET).expect("a tree");
    routes(tree.root_node(), source.as_bytes(), false)
        .iter()
        .map(|r| {
            let handler = match (&r.mounted, &r.controller, &r.action) {
                (Some(mounted), _, _) => format!("mount {mounted}"),
                (None, Some(c), Some(a)) => {
                    format!(
                        "{c}#{a} ({}#{a})",
                        r.controller_constant().unwrap_or_default()
                    )
                }
                _ => "handler not static".to_owned(),
            };
            format!("{} {} {handler}", r.verb, r.path)
        })
        .collect()
}

/// Namespaces, resources with `only:`/`except:`, `member`/`collection` and
/// `on:`, singular resources, `module:` on a resource, `scope` with a path
/// and a module, `controller` blocks, inferred targets, `match ... via:`
/// and both `mount` forms. v1
/// `routes_should_compose_namespaces_resources_and_verbs`.
#[test]
fn routes_compose_namespaces_resources_and_verbs() {
    let source = r#"Rails.application.routes.draw do
  root "home#index"
  namespace :admin do
    root to: "dashboard#show"
    resources :orders, only: [:index, :create] do
      member do
        post :refund
      end
      get :export, on: :collection
      get "preview", action: :show_preview, on: :member
      resources :line_items, only: :destroy
    end
    resource :profile, except: [:new, :create, :destroy]
  end
  resources :orders, only: [:show]
  resources :posts, only: [], module: :blog do
    resources :comments, only: :index
    get "feed", to: "feeds#show"
  end
  scope "/shop", module: "store" do
    get "cart" => "carts#show"
    get "checkout", to: "checkouts#new"
  end
  scope module: :api do
    patch "items/:id", controller: "items", action: :update
  end
  controller :pages do
    get "about" => :about
  end
  get "help/faq"
  match "search", to: "search#query", via: [:get, :post]
  mount Sidekiq::Web => "/sidekiq"
  mount Blog::Engine, at: "/blog"
end
"#;
    assert_eq!(
        table(source),
        [
            "GET / home#index (HomeController#index)",
            "GET /admin admin/dashboard#show (Admin::DashboardController#show)",
            "GET /admin/orders admin/orders#index (Admin::OrdersController#index)",
            "POST /admin/orders admin/orders#create (Admin::OrdersController#create)",
            "POST /admin/orders/:id/refund admin/orders#refund (Admin::OrdersController#refund)",
            "GET /admin/orders/export admin/orders#export (Admin::OrdersController#export)",
            "GET /admin/orders/:id/preview admin/orders#show_preview (Admin::OrdersController#show_preview)",
            "DELETE /admin/orders/:order_id/line_items/:id admin/line_items#destroy (Admin::LineItemsController#destroy)",
            "GET /admin/profile/edit admin/profiles#edit (Admin::ProfilesController#edit)",
            "GET /admin/profile admin/profiles#show (Admin::ProfilesController#show)",
            "PATCH /admin/profile admin/profiles#update (Admin::ProfilesController#update)",
            "PUT /admin/profile admin/profiles#update (Admin::ProfilesController#update)",
            "GET /orders/:id orders#show (OrdersController#show)",
            "GET /posts/:post_id/comments blog/comments#index (Blog::CommentsController#index)",
            "GET /posts/:post_id/feed blog/feeds#show (Blog::FeedsController#show)",
            "GET /shop/cart store/carts#show (Store::CartsController#show)",
            "GET /shop/checkout store/checkouts#new (Store::CheckoutsController#new)",
            "PATCH /items/:id api/items#update (Api::ItemsController#update)",
            "GET /about pages#about (PagesController#about)",
            "GET /help/faq help#faq (HelpController#faq)",
            "GET /search search#query (SearchController#query)",
            "POST /search search#query (SearchController#query)",
            "MOUNT /sidekiq mount Sidekiq::Web",
            "MOUNT /blog mount Blog::Engine",
        ]
    );
}

/// `:"orders"` reads as `:orders`; an interpolated `:"x#{y}"` names
/// nothing. v1 `quoted_symbols_should_read_like_plain_ones`.
#[test]
fn quoted_symbols_read_like_plain_ones() {
    let source = "Rails.application.routes.draw do\n  resources :\"orders\", only: [:\"show\"]\n  get :\"ping\", to: \"health#ping\"\n  get :\"x#{y}\", to: \"a#b\"\nend\n";
    assert_eq!(
        table(source),
        [
            "GET /orders/:id orders#show (OrdersController#show)",
            "GET /ping health#ping (HealthController#ping)",
        ]
    );
}

/// Each level names two resources, so the innermost block is reached 2^20
/// times without a bound: the walk stops at the route cap, in time. v1
/// `nested_multi_name_resources_should_stop_at_the_route_cap`.
#[test]
fn nested_multi_name_resources_stop_at_the_route_cap() {
    let mut source = String::from("Rails.application.routes.draw do\n");
    for depth in 0..20 {
        source.push_str(&format!("resources :a{depth}, :b{depth} do\n"));
    }
    source.push_str(&"end\n".repeat(21));
    let started = std::time::Instant::now();
    assert_eq!(table(&source).len(), MAX_ROUTES);
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

/// A variable or interpolated path, a lambda, a Rack app or a redirect
/// target, a variable resource name or `only:`, `match` without `via:`,
/// `concern`, `draw`, a variable `mount`, and a dynamic segment with no
/// target: no route at all, never a guess (C-10). v1
/// `dynamic_routes_should_yield_nothing_rather_than_a_guess`.
#[test]
fn dynamic_routes_yield_nothing_rather_than_a_guess() {
    let source = r#"Rails.application.routes.draw do
  get path_var, to: "a#b"
  get "x/#{y}", to: "a#b"
  get "lambda", to: ->(env) { [200, {}, []] }
  get "rack", to: RackApp
  resources resource_name
  resources :orders, only: allowed
  namespace ns do
    get "inside", to: "a#b"
  end
  match "any", to: "a#b"
  concern :commentable do
    resources :comments
  end
  draw :admin
  mount engine, at: "/x"
  get "orders/:id"
  get "files/*path"
  get "feed(.:format)"
  get "old/path", to: redirect("/new")
  match "a/b", to: redirect("/c"), via: :get
  get "c/d", action: some_action
  get "e/f", controller: some_controller
  controller :pages do
    get "orders/:id"
    get "a/b"
    get "health", to: HealthApp
  end
  resources :photos, only: [] do
    get "nested/path"
    get "legacy", to: redirect("/x")
  end
end
"#;
    assert_eq!(table(source), Vec::<String>::new());
}

/// `%i[...]` and `%w[...]` lists filter like `[...]` wherever a literal
/// list does (`only:`, `except:`, `via:`), where v1 dropped the whole
/// declaration (MVP decision 7); an interpolated `%I[...]` is no literal.
#[test]
fn a_percent_list_filters_like_an_array() {
    let source = r#"Rails.application.routes.draw do
  resources :searches, only: %i[ index create ] do
    delete :clear, on: :collection
  end
  resources :photos, only: %w[show]
  resource :logo, except: %i[ new create edit ]
  match "ping", to: "health#ping", via: %i[get head]
  resources :dynamic, only: %I[ show#{x} ]
end
"#;
    assert_eq!(
        table(source),
        [
            "GET /searches searches#index (SearchesController#index)",
            "POST /searches searches#create (SearchesController#create)",
            "DELETE /searches/clear searches#clear (SearchesController#clear)",
            "GET /photos/:id photos#show (PhotosController#show)",
            "GET /logo logos#show (LogosController#show)",
            "PATCH /logo logos#update (LogosController#update)",
            "PUT /logo logos#update (LogosController#update)",
            "DELETE /logo logos#destroy (LogosController#destroy)",
            "GET /ping health#ping (HealthController#ping)",
            "HEAD /ping health#ping (HealthController#ping)",
        ]
    );
}

/// The shapes v1 was run on for once-campfire: `shallow:`, `as:`,
/// `constraints` and `with_options` are ignored, a stray `member do` and an
/// unknown block keep the outer scope, `path:`, `param:` and `controller:`
/// shape a resource, `module: nil` stops a namespace, and an escape in a
/// path is dropped as v1 does. Only the `%i[]`/`%w[]` lines differ from v1.
#[test]
fn campfire_shaped_declarations_compose_as_v1_does() {
    let source = r#"Rails.application.routes.draw do
  resources :searches, only: %i[ index create ] do
    delete :clear, on: :collection
  end
  resources :photos, only: %w[show]
  get :sym => "a#b"
  nested do
    get "inner", to: "c#d"
  end
  member do
    get "stray", to: "e#f"
  end
  resources :posts, shallow: true do
    resources :comments, only: :show
  end
  resources :orders, param: :slug, path: "commandes", controller: "purchases", only: [:show] do
    get :recent
  end
  scope "(:locale)", locale: /en|fr/ do
    get "about", to: "pages#about"
  end
  get "esc\tape", to: "x#y"
  get "multi",
    to: "multi#line"
  root :index
  with_options only: :show do
    resources :things
  end
  namespace :api, module: nil do
    get "v", to: "v#show"
  end
  get "", to: "empty#show"
  resource :status
  resources :people
  get "x", to: "Admin::Foo#bar"
end
"#;
    assert_eq!(
        table(source),
        [
            "GET /searches searches#index (SearchesController#index)",
            "POST /searches searches#create (SearchesController#create)",
            "DELETE /searches/clear searches#clear (SearchesController#clear)",
            "GET /photos/:id photos#show (PhotosController#show)",
            "GET /inner c#d (CController#d)",
            "GET /stray e#f (EController#f)",
            "GET /posts posts#index (PostsController#index)",
            "POST /posts posts#create (PostsController#create)",
            "GET /posts/new posts#new (PostsController#new)",
            "GET /posts/:id/edit posts#edit (PostsController#edit)",
            "GET /posts/:id posts#show (PostsController#show)",
            "PATCH /posts/:id posts#update (PostsController#update)",
            "PUT /posts/:id posts#update (PostsController#update)",
            "DELETE /posts/:id posts#destroy (PostsController#destroy)",
            "GET /posts/:post_id/comments/:id comments#show (CommentsController#show)",
            "GET /commandes/:slug purchases#show (PurchasesController#show)",
            "GET /commandes/:order_slug/recent purchases#recent (PurchasesController#recent)",
            "GET /(:locale)/about pages#about (PagesController#about)",
            "GET /escape x#y (XController#y)",
            "GET /multi multi#line (MultiController#line)",
            "GET /things things#index (ThingsController#index)",
            "POST /things things#create (ThingsController#create)",
            "GET /things/new things#new (ThingsController#new)",
            "GET /things/:id/edit things#edit (ThingsController#edit)",
            "GET /things/:id things#show (ThingsController#show)",
            "PATCH /things/:id things#update (ThingsController#update)",
            "PUT /things/:id things#update (ThingsController#update)",
            "DELETE /things/:id things#destroy (ThingsController#destroy)",
            "GET / empty#show (EmptyController#show)",
            "POST /status statuses#create (StatusesController#create)",
            "GET /status/new statuses#new (StatusesController#new)",
            "GET /status/edit statuses#edit (StatusesController#edit)",
            "GET /status statuses#show (StatusesController#show)",
            "PATCH /status statuses#update (StatusesController#update)",
            "PUT /status statuses#update (StatusesController#update)",
            "DELETE /status statuses#destroy (StatusesController#destroy)",
            "GET /people people#index (PeopleController#index)",
            "POST /people people#create (PeopleController#create)",
            "GET /people/new people#new (PeopleController#new)",
            "GET /people/:id/edit people#edit (PeopleController#edit)",
            "GET /people/:id people#show (PeopleController#show)",
            "PATCH /people/:id people#update (PeopleController#update)",
            "PUT /people/:id people#update (PeopleController#update)",
            "DELETE /people/:id people#destroy (PeopleController#destroy)",
            "GET /x Admin::Foo#bar (Admin::FooController#bar)",
        ]
    );
}

/// A routes file's sites: one per declaration line, controller and action,
/// at the declaring line, owned by the file, with the verb and path of the
/// first route; a `mount` and a dynamic path give none. The rows of v1's
/// `a_route_query_should_return_its_declaration_and_controller_action`.
#[test]
fn c10_a_route_site_is_at_its_declaration_line() {
    let rows = rows(
        "config/routes.rb",
        "Rails.application.routes.draw do
  namespace :admin do
    resources :orders, only: [:index, :create]
    resource :settings, only: :show
  end
  resources :orders, only: [:create, :update]
  get \"dynamic/#{part}\", to: \"dynamic#show\"
  mount Sidekiq::Web => \"/sidekiq\"
  draw :api
end
",
    );
    assert_eq!(
        route_sites(&rows),
        [
            "3 config/routes.rb: GET /admin/orders Admin::OrdersController#index []",
            "3 config/routes.rb: POST /admin/orders Admin::OrdersController#create []",
            "4 config/routes.rb: GET /admin/settings Admin::SettingsController#show []",
            "6 config/routes.rb: POST /orders OrdersController#create []",
            "6 config/routes.rb: PATCH /orders/:id OrdersController#update []",
        ]
    );
}

/// A file Rails draws is read in the module of the `draw` that reads it,
/// which another file gives: its controller is recorded relative to it
/// (D04); an absolute one is not. The same declarations outside a routes
/// file are no routes.
#[test]
fn a_drawn_routes_file_is_read_in_the_module_of_its_draw() {
    let source =
        "get \"status\", to: \"health#show\"\nget \"up\", controller: \"/health\", action: :up\n";
    let drawn = rows("config/routes/api.rb", source);
    assert_eq!(
        route_sites(&drawn),
        [
            "1 config/routes/api.rb: GET /status <draw>/health#show []",
            "2 config/routes/api.rb: GET /up HealthController#up []",
        ]
    );
    assert!(route_sites(&rows("app/routes.rb", source)).is_empty());
}

/// The main routes file and the files it draws, at the root or in an
/// engine. v1 `routes_files_should_be_the_main_one_and_drawn_ones`.
#[test]
fn routes_files_are_the_main_one_and_drawn_ones() {
    for path in [
        "config/routes.rb",
        "engines/billing/config/routes.rb",
        "config/routes/admin.rb",
        "config/routes/api/v1.rb",
    ] {
        assert!(is_routes_file(path), "{path}");
    }
    for path in [
        "app/config/routes.rbx",
        "config/router.rb",
        "myconfig/routes.rb",
        "config/routes/readme.md",
        "lib/routes.rb",
    ] {
        assert!(!is_routes_file(path), "{path}");
    }
}

/// The regular English rules Rails' default inflections apply; irregular
/// words are not special-cased. v1
/// `inflections_should_follow_the_regular_english_rules`.
#[test]
fn inflections_follow_the_regular_english_rules() {
    for (plural, singular) in [
        ("orders", "order"),
        ("categories", "category"),
        ("boxes", "box"),
        ("addresses", "address"),
        ("glass", "glass"),
        ("sizes", "size"),
        ("statuses", "status"),
        ("order_statuses", "order_status"),
        ("aliases", "alias"),
        ("buses", "bus"),
        ("status", "status"),
        ("alias", "alias"),
        ("bus", "bus"),
    ] {
        assert_eq!(singularize(plural), singular, "{plural}");
    }
    for (singular, plural) in [
        ("profile", "profiles"),
        ("category", "categories"),
        ("key", "keys"),
        ("box", "boxes"),
        ("address", "addresses"),
        ("settings", "settings"),
        ("status", "statuses"),
        ("order_status", "order_statuses"),
        ("alias", "aliases"),
        ("bus", "buses"),
    ] {
        assert_eq!(pluralize(singular), plural, "{singular}");
    }
    assert_eq!(camelize_path("admin/order_items"), "Admin::OrderItems");
}
