// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Hand-labelled sites of the resolution rules, on generations built
//! row by row (`fixture`).

use mvp_model::SymbolKind::{Class, Method, Module};

use super::fixture::Generation;
use crate::mvp_index::ruby::{ALIAS, DELEGATE};

#[test]
fn c10_ruby_bare_and_self_calls_reach_the_callers_own_method() {
    // a.rb                          other.rb
    //  1 class Svc                  1 class Other
    //  2   def run                  2   def access_logs; end
    //  3     access_logs(user: 1)   3 end
    //  4     self.access_logs
    //  5   end
    //  6   def access_logs(user:); end
    //  7 end
    let mut g = Generation::default();
    g.def("a.rb", Class, "Svc", 1, 7)
        .def("a.rb", Method, "Svc#run", 2, 5)
        .def("a.rb", Method, "Svc#access_logs", 6, 6)
        .def("other.rb", Class, "Other", 1, 3)
        .def("other.rb", Method, "Other#access_logs", 2, 2);
    let bare = g.call("a.rb", "Svc#run", 3, "", "access_logs", &["Svc"]);
    let on_self = g.call("a.rb", "Svc#run", 4, "self", "access_logs", &["Svc"]);
    let dst = g.resolve();
    assert_eq!(dst[bare].as_deref(), Some("a.rb#Svc#access_logs#method"));
    assert_eq!(dst[on_self].as_deref(), Some("a.rb#Svc#access_logs#method"));
}

#[test]
fn c10_ruby_a_self_call_never_crosses_to_the_other_side() {
    // 1 class Svc
    // 2   def self.build; access_logs; end
    // 3   def run; importable; end
    // 4   def access_logs; end
    // 5   def self.importable; end
    // 6 end
    // 7 require_relative "helpers"   # defines both names at the top level
    let mut g = Generation::default();
    g.def("a.rb", Class, "Svc", 1, 6)
        .def("a.rb", Method, "Svc.build", 2, 2)
        .def("a.rb", Method, "Svc#run", 3, 3)
        .def("a.rb", Method, "Svc#access_logs", 4, 4)
        .def("a.rb", Method, "Svc.importable", 5, 5)
        .function("helpers.rb", "access_logs", 1, 1)
        .function("helpers.rb", "importable", 2, 2);
    g.import("a.rb", 7, "require_relative", "helpers", "./helpers");
    let from_class = g.call("a.rb", "Svc.build", 2, "", "access_logs", &["Svc"]);
    let from_instance = g.call("a.rb", "Svc#run", 3, "", "importable", &["Svc"]);
    let dst = g.resolve();
    // `self` in `Svc.build` is the class: its instance method is not
    // reachable, nor is the top-level function of the same name.
    assert_eq!(dst[from_class], None);
    assert_eq!(dst[from_instance], None);
}

#[test]
fn c10_ruby_a_class_body_call_reaches_a_method_of_the_class_itself() {
    // catalog.rb
    //  1 class Catalog
    //  2   setup
    //  3   def self.load
    //  4     importable
    //  5   end
    //  6   class << self
    //  7     def importable; end
    //  8     def setup; end
    //  9   end
    // 10 end
    let mut g = Generation::default();
    g.def("catalog.rb", Class, "Catalog", 1, 10)
        .def("catalog.rb", Method, "Catalog.load", 3, 5)
        .def("catalog.rb", Method, "Catalog.importable", 7, 7)
        .def("catalog.rb", Method, "Catalog.setup", 8, 8)
        .def("other.rb", Method, "Other#importable", 2, 2);
    let in_body = g.call("catalog.rb", "Catalog", 2, "", "setup", &["Catalog"]);
    let in_method = g.call(
        "catalog.rb",
        "Catalog.load",
        4,
        "",
        "importable",
        &["Catalog"],
    );
    let dst = g.resolve();
    assert_eq!(
        dst[in_body].as_deref(),
        Some("catalog.rb#Catalog.setup#method")
    );
    assert_eq!(
        dst[in_method].as_deref(),
        Some("catalog.rb#Catalog.importable#method")
    );
}

#[test]
fn c10_ruby_a_call_never_takes_another_classs_method_of_the_same_file() {
    // a.rb (v1 Q4 gave `Admin#run` an exact edge to `Svc#access_logs`
    // through its same-file tier; decision 1 makes it unresolved)
    //  1 class Svc
    //  2   def access_logs; end
    //  3 end
    //  4 class Admin
    //  5   def run
    //  6     access_logs
    //  7   end
    //  8 end
    let mut g = Generation::default();
    g.def("a.rb", Class, "Svc", 1, 3)
        .def("a.rb", Method, "Svc#access_logs", 2, 2)
        .def("a.rb", Class, "Admin", 4, 8)
        .def("a.rb", Method, "Admin#run", 5, 7);
    let call = g.call("a.rb", "Admin#run", 6, "", "access_logs", &["Admin"]);
    assert_eq!(g.resolve()[call], None);
}

#[test]
fn c10_ruby_a_method_reopened_in_another_file_is_unresolved() {
    // app.rb                          app_patch.rb
    // 1 class App                     1 class App
    // 2   def run; access_logs; end   2   def access_logs; end
    // 3   def access_logs; end        3 end
    // 4 end
    // 5 require_relative "helpers"
    // helpers.rb: `def access_logs; end`, never reached past `App`.
    let (a, cref) = ("app.rb", ["App"]);
    let mut g = Generation::default();
    g.def(a, Class, "App", 1, 4)
        .def(a, Method, "App#run", 2, 2)
        .def(a, Method, "App#access_logs", 3, 3)
        .def("app_patch.rb", Class, "App", 1, 3)
        .def("app_patch.rb", Method, "App#access_logs", 2, 2)
        .function("helpers.rb", "access_logs", 1, 1);
    g.import(a, 5, "require_relative", "helpers", "./helpers");
    let bare = g.call(a, "App#run", 2, "", "access_logs", &cref);
    let on_self = g.call(a, "App#run", 2, "self", "access_logs", &cref);
    let dst = g.resolve();
    // Ruby keeps whichever file loads last, which the graph cannot tell.
    assert_eq!(dst[bare], None);
    assert_eq!(dst[on_self], None);
}

#[test]
fn c10_ruby_the_last_definition_in_a_file_wins() {
    // a.rb (v1 kept the last row through `INSERT OR REPLACE`; decision 5)
    //  1 class Svc
    //  2   def run; helper; end
    //  3   def helper; 1; end
    //  4   def helper; 2; end
    //  5 end
    //  6 def tool; 1; end
    //  7 def tool; 2; end
    //  8 tool
    let mut g = Generation::default();
    g.def("a.rb", Class, "Svc", 1, 5)
        .def("a.rb", Method, "Svc#run", 2, 2)
        .def("a.rb", Method, "Svc#helper", 3, 3)
        .def("a.rb", Method, "Svc#helper", 4, 4)
        .function("a.rb", "tool", 6, 6)
        .function("a.rb", "tool", 7, 7);
    let method = g.call("a.rb", "Svc#run", 2, "", "helper", &["Svc"]);
    let function = g.call("a.rb", "a.rb", 8, "", "tool", &[]);
    let dst = g.resolve();
    assert_eq!(dst[method].as_deref(), Some("a.rb#Svc#helper#method~2"));
    assert_eq!(dst[function].as_deref(), Some("a.rb#tool#function~2"));
}

#[test]
fn c10_ruby_top_level_code_calls_the_unique_top_level_function_it_loads() {
    // lib.rb: `def foo; end`
    // script.rb                      other.rb
    // 1 require_relative "lib"       1 foo
    // 2 foo()
    // 3 def main
    // 4   foo
    // 5 end
    // v1 (and decision 2 as written) took the only file defining `foo`
    // for `other.rb` too; a script's `def` exists once it is loaded,
    // and on once-campfire that edge was wrong (`bin/release` calling
    // `bin/bundle`'s `gemfile`).
    let mut g = Generation::default();
    g.function("lib.rb", "foo", 1, 2)
        .function("script.rb", "main", 3, 5);
    g.import("script.rb", 1, "require_relative", "lib", "./lib");
    let root = g.call("script.rb", "script.rb", 2, "", "foo", &[]);
    let nested = g.call("script.rb", "main", 4, "", "foo", &[]);
    let unloaded = g.call("other.rb", "other.rb", 1, "", "foo", &[]);
    let dst = g.resolve();
    assert_eq!(dst[root].as_deref(), Some("lib.rb#foo#function"));
    assert_eq!(dst[nested].as_deref(), Some("lib.rb#foo#function"));
    assert_eq!(dst[unloaded], None);
}

#[test]
fn c10_ruby_a_top_level_function_defined_in_two_files_is_unresolved() {
    // lib/local.rb: `def application; end` and its own top-level call;
    // lib/remote.rb: `def application; end`, loaded first or last by
    // whatever loads both.
    let mut g = Generation::default();
    g.function("lib/local.rb", "application", 1, 3)
        .function("lib/remote.rb", "application", 1, 3);
    let call = g.call("lib/local.rb", "lib/local.rb", 5, "", "application", &[]);
    assert_eq!(g.resolve()[call], None);
}

#[test]
fn c10_ruby_a_method_whose_chain_is_known_reaches_a_top_level_function() {
    // helpers.rb: `def slugify(text); end`
    // post.rb                       record.rb
    // 1 class Post                  1 class Record < Base
    // 2   include Sluggable         2   def run; slugify; end
    // 3   def run; slugify; end     3   def self.build; slugify; end
    // 4 end                         4 end
    // 5 module Sluggable; end         5 require_relative "helpers"
    // 6 require_relative "helpers"
    let mut g = Generation::default();
    g.function("helpers.rb", "slugify", 1, 1)
        .def("post.rb", Class, "Post", 1, 4)
        .def("post.rb", Method, "Post#run", 3, 3)
        .def("post.rb", Module, "Sluggable", 5, 5)
        .def("record.rb", Class, "Record", 1, 4)
        .def("record.rb", Method, "Record#run", 2, 2)
        .def("record.rb", Method, "Record.build", 3, 3);
    g.import("post.rb", 6, "require_relative", "helpers", "./helpers");
    g.import("record.rb", 5, "require_relative", "helpers", "./helpers");
    g.mixin("post.rb", "Post", 2, "include", "Sluggable", &["Post"]);
    g.mixin("record.rb", "Record", 1, "superclass", "Base", &[]);
    let known = g.call("post.rb", "Post#run", 3, "", "slugify", &["Post"]);
    let instance = g.call("record.rb", "Record#run", 2, "", "slugify", &["Record"]);
    let class = g.call("record.rb", "Record.build", 3, "", "slugify", &["Record"]);
    let dst = g.resolve();
    assert_eq!(dst[known].as_deref(), Some("helpers.rb#slugify#function"));
    // `Base` is not in the graph (a gem's class): it may define `slugify`.
    assert_eq!(dst[instance], None);
    assert_eq!(dst[class], None);
}

#[test]
fn c10_ruby_a_receiver_that_is_not_self_or_a_constant_is_unresolved() {
    // a.rb: class Svc with `def save; end` and, in `def run`,
    // `user.save`, `@record.save`, `@@cache.save`, `$store.save`,
    // `items.first.save`, `self.class.save`, `super.save`.
    // v1 resolved `user.save` (sole method of the name) and the others
    // through its name tiers; decision 1 makes them unresolved.
    let mut g = Generation::default();
    g.def("a.rb", Class, "Svc", 1, 9)
        .def("a.rb", Method, "Svc#save", 2, 2)
        .def("a.rb", Method, "Svc#run", 3, 8);
    let receivers = [
        "user",
        "@record",
        "@@cache",
        "$store",
        "items.first",
        "self.class",
        "super",
    ];
    let calls: Vec<usize> = receivers
        .iter()
        .map(|receiver| g.call("a.rb", "Svc#run", 4, receiver, "save", &["Svc"]))
        .collect();
    let dst = g.resolve();
    for (receiver, call) in receivers.iter().zip(calls) {
        assert_eq!(dst[call], None, "{receiver}.save");
    }
}

#[test]
fn c10_ruby_a_name_defined_only_in_another_class_is_unresolved() {
    // x.rb                       other.rb
    // 1 class X                  1 class A
    // 2   def go                 2   def helper; end
    // 3     helper               3 end
    // 4   end
    // 5 end
    // v1 Q3 linked `X#go` to `A#helper` (the only file defining the
    // name); decision 1 makes it unresolved.
    let mut g = Generation::default();
    g.def("x.rb", Class, "X", 1, 5)
        .def("x.rb", Method, "X#go", 2, 4)
        .def("other.rb", Class, "A", 1, 3)
        .def("other.rb", Method, "A#helper", 2, 2);
    let call = g.call("x.rb", "X#go", 3, "", "helper", &["X"]);
    assert_eq!(g.resolve()[call], None);
}

#[test]
fn c10_ruby_callbacks_name_an_instance_method_of_the_declaring_owner() {
    // record.rb                          other.rb
    // 1 class Record                     1 class Other
    // 2   validate :check, if: :ready?   2   def check; end
    // 3   after_commit :foreign          3   def foreign; end
    // 4   after_save :class_only         4 end
    // 5   def check; end
    // 6   def ready?; true; end
    // 7   def self.class_only; end
    let (r, cref) = ("record.rb", ["Record"]);
    let mut g = Generation::default();
    g.def(r, Class, "Record", 1, 8)
        .def(r, Method, "Record#check", 5, 5)
        .def(r, Method, "Record#ready?", 6, 6)
        .def(r, Method, "Record.class_only", 7, 7)
        .def("other.rb", Method, "Other#check", 2, 2)
        .def("other.rb", Method, "Other#foreign", 3, 3);
    let check = g.reference(r, "Record", 2, "validate", "check", &cref);
    let ready = g.reference(r, "Record", 2, "validate", "ready?", &cref);
    let foreign = g.reference(r, "Record", 3, "after_commit", "foreign", &cref);
    let class_only = g.reference(r, "Record", 4, "after_save", "class_only", &cref);
    let dst = g.resolve();
    assert_eq!(dst[check].as_deref(), Some("record.rb#Record#check#method"));
    assert_eq!(
        dst[ready].as_deref(),
        Some("record.rb#Record#ready?#method")
    );
    // Never another class's method, nor the class's own `class_only`.
    assert_eq!(dst[foreign], None);
    assert_eq!(dst[class_only], None);
}

#[test]
fn c10_ruby_a_module_callback_reaches_a_method_its_reopening_defines() {
    // record.rb: `module Rules; validate :check; end`; rules.rb:
    // `module Rules; def check; true; end; end`. The module opens in
    // two files; its method is defined in one.
    let mut g = Generation::default();
    g.def("record.rb", Module, "Rules", 1, 3)
        .def("rules.rb", Module, "Rules", 1, 3)
        .def("rules.rb", Method, "Rules#check", 2, 2);
    let check = g.reference("record.rb", "Rules", 2, "validate", "check", &["Rules"]);
    assert_eq!(
        g.resolve()[check].as_deref(),
        Some("rules.rb#Rules#check#method")
    );
}

#[test]
fn c10_ruby_a_callback_needs_a_class_body_and_one_definition() {
    // record.rb: `class Record; validate :check; def check; end;
    // def run; before_save :check; end; end`; then reopened.rb reopens
    // `Record` with its own `check`.
    let (r, cref) = ("record.rb", ["Record"]);
    let mut g = Generation::default();
    g.def(r, Class, "Record", 1, 6)
        .def(r, Method, "Record#check", 3, 3)
        .def(r, Method, "Record#run", 4, 5);
    let in_method = g.reference(r, "Record#run", 4, "before_save", "check", &cref);
    let in_body = g.reference(r, "Record", 2, "validate", "check", &cref);
    let dst = g.resolve();
    assert_eq!(dst[in_method], None);
    assert_eq!(
        dst[in_body].as_deref(),
        Some("record.rb#Record#check#method")
    );

    g.def("reopened.rb", Class, "Record", 1, 3)
        .def("reopened.rb", Method, "Record#check", 2, 2);
    assert_eq!(g.resolve()[in_body], None);
}

#[test]
fn c10_ruby_decisions_do_not_depend_on_the_order_of_the_rows() {
    // order.rb: `class Order; include Known; def run; helper; end`;
    // order_ext.rb reopens it: `class Order; include(plugins.first)`;
    // known.rb: `module Known; def helper; end`. Declarations are read
    // in path order, so the unknown module is looked up before Known
    // whichever order a run lists the files in: no edge either way.
    let mut g = Generation::default();
    g.def("known.rb", Module, "Known", 1, 3)
        .def("known.rb", Method, "Known#helper", 2, 2)
        .def("order.rb", Class, "Order", 1, 4)
        .def("order.rb", Method, "Order#run", 3, 3)
        .def("order_ext.rb", Class, "Order", 1, 3);
    g.mixin("order.rb", "Order", 2, "include", "Known", &["Order"]);
    g.mixin(
        "order_ext.rb",
        "Order",
        2,
        "include",
        "plugins.first",
        &["Order"],
    );
    let helper = g.call("order.rb", "Order#run", 3, "", "helper", &["Order"]);
    assert_eq!(g.resolve()[helper], None);
    assert_eq!(g.files_reversed().decisions(), g.decisions());
}

#[test]
fn c10_ruby_a_reference_past_an_unknown_ancestor_is_unresolved() {
    // record.rb
    // 1 class Record
    // 2   prepend Auditing              # a gem's module, looked up first
    // 3   before_save :check
    // 4   alias_method :verify, :check
    // 5   def check; end
    let (r, cref) = ("record.rb", ["Record"]);
    let mut g = Generation::default();
    g.def(r, Class, "Record", 1, 6)
        .def(r, Method, "Record#verify", 4, 4)
        .def(r, Method, "Record#check", 5, 5);
    g.mixin(r, "Record", 2, "prepend", "Auditing", &cref);
    let callback = g.reference(r, "Record", 3, "before_save", "check", &cref);
    let alias = g.reference(r, "Record#verify", 4, ALIAS, "check", &cref);
    let dst = g.resolve();
    // `Auditing` may define `check` before `Record` does.
    assert_eq!(dst[callback], None);
    assert_eq!(dst[alias], None);
}

#[test]
fn c10_ruby_send_names_a_method_of_the_side_self_is_on() {
    // record.rb
    //  1 class Record
    //  2   send(:check, :payload)
    //  3   def self.check; end
    //  4   def check; end
    //  5   def run
    //  6     send(:check, :payload)
    //  7     self.public_send(:check)
    //  8   end
    //  9   def self.run
    // 10     public_send(:check)
    let (r, cref) = ("record.rb", ["Record"]);
    let mut g = Generation::default();
    g.def(r, Class, "Record", 1, 12)
        .def(r, Method, "Record.check", 3, 3)
        .def(r, Method, "Record#check", 4, 4)
        .def(r, Method, "Record#run", 5, 8)
        .def(r, Method, "Record.run", 9, 11);
    let body = g.reference(r, "Record", 2, "send", "check", &cref);
    let instance = g.reference(r, "Record#run", 6, "send", "check", &cref);
    let public = g.reference(r, "Record#run", 7, "public_send", "check", &cref);
    let class = g.reference(r, "Record.run", 10, "public_send", "check", &cref);
    let dst = g.resolve();
    let of_class = Some("record.rb#Record.check#method");
    let of_instance = Some("record.rb#Record#check#method");
    assert_eq!(dst[body].as_deref(), of_class);
    assert_eq!(dst[instance].as_deref(), of_instance);
    assert_eq!(dst[public].as_deref(), of_instance);
    assert_eq!(dst[class].as_deref(), of_class);
}

#[test]
fn c10_ruby_alias_and_delegate_name_a_method_of_their_own_owner() {
    // record.rb: `class Record; def persisted_label; end; end`
    // invoice.rb
    // 1 class Invoice < Record
    // 2   alias_method :label, :persisted_label
    // 3   delegate :city, to: :address
    // 4   def address; end
    // other.rb: class Other with `address` and `persisted_label`.
    let (i, cref) = ("invoice.rb", ["Invoice"]);
    let mut g = Generation::default();
    g.def("record.rb", Class, "Record", 1, 3)
        .def("record.rb", Method, "Record#persisted_label", 2, 2)
        .def(i, Class, "Invoice", 1, 5)
        .def(i, Method, "Invoice#label", 2, 2)
        .def(i, Method, "Invoice#city", 3, 3)
        .def(i, Method, "Invoice#address", 4, 4)
        .def("other.rb", Method, "Other#address", 2, 2)
        .def("other.rb", Method, "Other#persisted_label", 3, 3);
    g.mixin(i, "Invoice", 1, "superclass", "Record", &[]);
    let alias = g.reference(i, "Invoice#label", 2, ALIAS, "persisted_label", &cref);
    let delegate = g.reference(i, "Invoice#city", 3, DELEGATE, "address", &cref);
    let dst = g.resolve();
    let inherited = Some("record.rb#Record#persisted_label#method");
    assert_eq!(dst[alias].as_deref(), inherited);
    let own = Some("invoice.rb#Invoice#address#method");
    assert_eq!(dst[delegate].as_deref(), own);
}

#[test]
fn c10_ruby_a_route_reaches_its_controller_action_or_an_inherited_one() {
    // config/routes.rb
    // 3 namespace :admin { resources :orders, only: [:index, :create] }
    // 6 resources :orders, only: [:create]
    // admin/orders.rb: `module Admin; class OrdersController <
    // BaseController; def create; end`; admin/base.rb: `module Admin;
    // class BaseController < ApplicationController; def index; end`;
    // app.rb: `class ApplicationController < ActionController::Base`;
    // orders.rb: `class OrdersController` with `create` and `index`.
    let (base, admin) = ("Admin::BaseController", "Admin::OrdersController");
    let mut g = Generation::default();
    g.def("app.rb", Class, "ApplicationController", 1, 2)
        .def("admin/base.rb", Class, base, 2, 5)
        .def("admin/base.rb", Method, "Admin::BaseController#index", 3, 4)
        .def("admin/orders.rb", Class, admin, 2, 5)
        .def(
            "admin/orders.rb",
            Method,
            "Admin::OrdersController#create",
            3,
            4,
        )
        .def("orders.rb", Class, "OrdersController", 1, 6)
        .def("orders.rb", Method, "OrdersController#create", 2, 3)
        .def("orders.rb", Method, "OrdersController#index", 4, 5);
    let (app, parent) = ("ApplicationController", "ActionController::Base");
    g.mixin("app.rb", app, 1, "superclass", parent, &[]);
    g.mixin("admin/base.rb", base, 2, "superclass", app, &["Admin"]);
    g.mixin(
        "admin/orders.rb",
        admin,
        2,
        "superclass",
        "BaseController",
        &["Admin"],
    );
    let index = g.route("config/routes.rb", 3, admin, "index");
    let create = g.route("config/routes.rb", 3, admin, "create");
    let root = g.route("config/routes.rb", 6, "OrdersController", "create");
    let dst = g.resolve();
    let inherited = Some("admin/base.rb#Admin::BaseController#index#method");
    assert_eq!(dst[index].as_deref(), inherited);
    let own = Some("admin/orders.rb#Admin::OrdersController#create#method");
    assert_eq!(dst[create].as_deref(), own);
    let top = Some("orders.rb#OrdersController#create#method");
    assert_eq!(dst[root].as_deref(), top);
}

#[test]
fn c10_ruby_a_route_never_falls_back_to_a_same_name_action() {
    // `resource :settings, only: :show` in `namespace :admin` names
    // `Admin::SettingsController`, only a module here;
    // `admin/orders#index` names an `Admin::OrdersController` without
    // `index`. `OrdersController` defines both actions.
    let mut g = Generation::default();
    g.def("admin/orders.rb", Class, "Admin::OrdersController", 2, 5)
        .def("orders.rb", Class, "OrdersController", 1, 6)
        .def("orders.rb", Method, "OrdersController#show", 2, 3)
        .def("orders.rb", Method, "OrdersController#index", 4, 5)
        .def("settings.rb", Module, "Admin::SettingsController", 1, 2)
        .def(
            "settings.rb",
            Method,
            "Admin::SettingsController#show",
            1,
            1,
        );
    let routes = "config/routes.rb";
    let missing_class = g.route(routes, 4, "Admin::SettingsController", "show");
    let missing_action = g.route(routes, 3, "Admin::OrdersController", "index");
    let dst = g.resolve();
    assert_eq!(dst[missing_class], None);
    assert_eq!(dst[missing_action], None);
}

#[test]
fn c10_ruby_an_association_names_the_class_active_record_loads() {
    // admin/order.rb
    // 1 module Admin
    // 2   class Order < ApplicationRecord
    // 3     belongs_to :customer
    // 4     has_many :line_items
    // 5     has_one :invoice, class_name: "Billing::Invoice"
    // 6     has_many :notes, class_name: "::Note"
    // 7     has_many :tags        (only a module `Tag` exists)
    let mut g = Generation::default();
    g.def("admin/order.rb", Class, "Admin::Order", 2, 8);
    let classes = ["Customer", "Admin::Customer", "LineItem", "Invoice"];
    for class in classes
        .into_iter()
        .chain(["Billing::Invoice", "Note", "Admin::Note"])
    {
        g.def("models.rb", Class, class, 1, 2);
    }
    g.def("models.rb", Module, "Tag", 1, 2);
    let (order, owner) = ("admin/order.rb", "Admin::Order");
    let customer = g.association(order, owner, 3, "belongs_to", "Customer");
    let items = g.association(order, owner, 4, "has_many", "LineItem");
    let invoice = g.association(order, owner, 5, "has_one", "Billing::Invoice");
    let notes = g.association(order, owner, 6, "has_many", "::Note");
    let tags = g.association(order, owner, 7, "has_many", "Tag");
    let dst = g.resolve();
    let class = |name: &str| Some(format!("models.rb#{name}#class"));
    assert_eq!(dst[customer], class("Admin::Customer"));
    assert_eq!(dst[items], class("LineItem"));
    assert_eq!(dst[invoice], class("Billing::Invoice"));
    assert_eq!(dst[notes], class("Note"));
    assert_eq!(dst[tags], None);
}

#[test]
fn c10_ruby_an_association_to_a_reopened_model_is_unresolved() {
    // customer.rb and customer/billing.rb both open `class Customer`;
    // order.rb: `class Order; belongs_to :customer`. In admin/order.rb
    // `Admin::Order#setup` declares `has_many :items`: a method body
    // declares no association.
    let mut g = Generation::default();
    g.def("customer.rb", Class, "Customer", 1, 2)
        .def("customer/billing.rb", Class, "Customer", 1, 3)
        .def("order.rb", Class, "Order", 1, 3)
        .def("item.rb", Class, "Item", 1, 2)
        .def("admin/order.rb", Method, "Admin::Order#setup", 2, 4);
    let reopened = g.association("order.rb", "Order", 2, "belongs_to", "Customer");
    let setup = "Admin::Order#setup";
    let in_method = g.association("admin/order.rb", setup, 3, "has_many", "Item");
    let dst = g.resolve();
    assert_eq!(dst[reopened], None);
    assert_eq!(dst[in_method], None);
}

#[test]
fn c10_ruby_an_ancestor_declaration_references_the_module_it_names() {
    // admin/order.rb
    // 1 module Admin
    // 2   class Order < ApplicationRecord
    // 3     include Trackable
    // 4     include(plugin)
    // 5     extend ActiveSupport::Concern
    let mut g = Generation::default();
    g.def("admin/order.rb", Module, "Admin", 1, 7)
        .def("admin/order.rb", Class, "Admin::Order", 2, 6)
        .def("record.rb", Class, "ApplicationRecord", 1, 2)
        .def("admin/trackable.rb", Module, "Admin::Trackable", 2, 4)
        .def("trackable.rb", Module, "Trackable", 1, 2);
    let (order, owner) = ("admin/order.rb", "Admin::Order");
    let nesting = ["Admin::Order", "Admin"];
    let parent = g.mixin(
        order,
        owner,
        2,
        "superclass",
        "ApplicationRecord",
        &["Admin"],
    );
    let module = g.mixin(order, owner, 3, "include", "Trackable", &nesting);
    let dynamic = g.mixin(order, owner, 4, "include", "plugin", &nesting);
    let gem = g.mixin(
        order,
        owner,
        5,
        "extend",
        "ActiveSupport::Concern",
        &nesting,
    );
    let dst = g.resolve();
    let record = Some("record.rb#ApplicationRecord#class");
    assert_eq!(dst[parent].as_deref(), record);
    // The nearer `Admin::Trackable` shadows the top-level `Trackable`.
    let trackable = Some("admin/trackable.rb#Admin::Trackable#module");
    assert_eq!(dst[module].as_deref(), trackable);
    assert_eq!(dst[dynamic], None);
    assert_eq!(dst[gem], None);
}

#[test]
fn c10_ruby_a_function_caller_is_top_level_code() {
    // lib.rb: `def helper; end` and `def main; helper; self.helper; end`
    let mut g = Generation::default();
    g.function("lib.rb", "helper", 1, 1)
        .function("lib.rb", "main", 2, 4);
    let bare = g.call("lib.rb", "main", 3, "", "helper", &[]);
    let on_self = g.call("lib.rb", "main", 3, "self", "helper", &[]);
    let dst = g.resolve();
    assert_eq!(dst[bare].as_deref(), Some("lib.rb#helper#function"));
    assert_eq!(dst[on_self].as_deref(), Some("lib.rb#helper#function"));
}

#[test]
fn audited_hook_mixins_should_be_brought_under_the_declaring_module_not_the_hook_method_name() {
    use super::{Index, stubs::Tests};
    use rustc_hash::FxHashSet as HashSet;
    let mut g = Generation::default();
    g.def("hooks.rb", Module, "Hook", 1, 5)
        .def("hooks.rb", Method, "Hook.included", 2, 4)
        .def("inside.rb", Module, "Inside", 1, 2);
    g.mixin(
        "hooks.rb",
        "Hook.included",
        3,
        "hook:include",
        "Inside",
        &["Hook"],
    );
    let world = g.world();
    let index = Index::build(&world, &|_| true, &Tests::Without);
    assert_eq!(
        index.brought(vec!["Hook"]),
        HashSet::from_iter(["Hook", "Inside"])
    );
}
