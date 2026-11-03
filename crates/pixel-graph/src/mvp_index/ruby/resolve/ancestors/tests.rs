// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

use rustc_hash::FxHashSet as HashSet;
use std::rc::Rc;

use mvp_model::SymbolKind::{Class, Method, Module};

use super::super::fixture::Generation;
use super::super::stubs::Tests;
use super::super::{Index, Side};
use super::MAX_ANCESTOR_DEPTH;
use crate::mvp_index::ruby::Fact;

// R6-04: a walk nested in one that met the cap was kept, at each depth an
// owner was met, as the steps it reached: the square of the cap in steps
// per owner, 511 MB for a chain of 3 000 classes.
#[test]
fn a_walk_past_the_cap_keeps_one_chain_per_owner() {
    let classes = 500;
    let mut g = Generation::default();
    g.def("chain.rb", Class, "C0", 1, 1);
    for i in 1..classes {
        let line = i + 1;
        let (class, parent) = (format!("C{i}"), format!("C{}", i - 1));
        g.def("chain.rb", Class, &class, line, line);
        g.mixin("chain.rb", &class, line, "superclass", &parent, &[]);
    }
    let world = g.world();
    let index = Index::build(&world, &|_| true, &Tests::Without);
    index.contained();
    let chains = &index.walks.chains;
    let whole = chains.whole.borrow();
    let capped = chains.capped.borrow();
    let mut seen = HashSet::default();
    let steps: usize = whole
        .values()
        .map(|(chain, _)| chain)
        .chain(capped.values())
        .filter(|chain| seen.insert(Rc::as_ptr(chain)))
        .map(|chain| chain.steps.len())
        .sum();
    let bound = 2 * classes as usize * (MAX_ANCESTOR_DEPTH + 2);
    assert!(capped.len() > classes as usize, "the cap was not met");
    assert!(steps <= bound, "{steps} steps kept for {classes} classes");
}

// R10-01: a call on `self` was looked up from every object holding the
// caller's step, once per bare call: a base class with 2 000 self calls
// and 10 000 subclasses indexed in 11 s against 0.6 s on main. Subclasses
// whose chains are the same past their own step share one lookup, but
// one that defines the name itself (`Sub7#halt`) is still looked up.
#[test]
fn c10_ruby_subclasses_with_the_same_chain_share_one_lookup() {
    // base.rb: class Base; def save; def run; save; def halt; def stop; halt
    // subN.rb: class SubN < Base; def own; (Sub7: def halt)
    let base = "base.rb";
    let mut g = Generation::default();
    g.def(base, Class, "Base", 1, 6)
        .def(base, Method, "Base#save", 2, 2)
        .def(base, Method, "Base#run", 3, 3)
        .def(base, Method, "Base#halt", 4, 4)
        .def(base, Method, "Base#stop", 5, 5);
    for i in 0..50 {
        let (file, class) = (format!("sub{i}.rb"), format!("Sub{i}"));
        g.def(&file, Class, &class, 1, 3)
            .def(&file, Method, &format!("{class}#own"), 2, 2);
        g.mixin(&file, &class, 1, "superclass", "Base", &[]);
    }
    g.def("sub7.rb", Method, "Sub7#halt", 2, 2);
    let save = g.call(base, "Base#run", 3, "", "save", &["Base"]);
    let halt = g.call(base, "Base#stop", 5, "", "halt", &["Base"]);
    let dst = g.resolve();
    assert_eq!(dst[save].as_deref(), Some("base.rb#Base#save#method"));
    assert_eq!(dst[halt], None);

    let world = g.world();
    let index = Index::build(&world, &|_| true, &Tests::Without);
    let held = index.held("Base", Side::Instance).expect("a share");
    assert_eq!((held.shares.len(), held.single.len()), (1, 0));
}

// R11-01: a model that mixed in modules of its own was looked up alone, so
// that a base class's self calls cost one lookup per model, 7 to 17 times
// main on Rails-shaped trees. Models share their superclass's chain past
// their own lists: a concern's method is read once from the concern's
// step, and a model that overrides it (`Sub7#tock`) is still looked up.
#[test]
fn c10_ruby_models_with_mixins_of_their_own_share_one_lookup() {
    // base.rb: class Base; def save; def run; save
    // shared.rb: module Shared; def tick; tock; def tock; def tack; tuck;
    // def tuck
    // subN.rb
    // 1 class SubN < Base
    // 2   module Own
    // 3     def own; end
    // 4   end
    // 5   include Own
    // 6   include Shared
    // 7   def tock; end       # Sub7 only
    let (base, shared) = ("base.rb", "shared.rb");
    let mut g = Generation::default();
    g.def(base, Class, "Base", 1, 4)
        .def(base, Method, "Base#save", 2, 2)
        .def(base, Method, "Base#run", 3, 3)
        .def(shared, Module, "Shared", 1, 6)
        .def(shared, Method, "Shared#tick", 2, 2)
        .def(shared, Method, "Shared#tock", 3, 3)
        .def(shared, Method, "Shared#tack", 4, 4)
        .def(shared, Method, "Shared#tuck", 5, 5);
    for i in 0..50 {
        let (file, class) = (format!("sub{i}.rb"), format!("Sub{i}"));
        let own = format!("{class}::Own");
        g.def(&file, Class, &class, 1, 8)
            .def(&file, Module, &own, 2, 4)
            .def(&file, Method, &format!("{own}#own"), 3, 3);
        g.mixin(&file, &class, 1, "superclass", "Base", &[]);
        g.mixin(&file, &class, 5, "include", "Own", &[&class]);
        g.mixin(&file, &class, 6, "include", "Shared", &[&class]);
    }
    g.def("sub7.rb", Method, "Sub7#tock", 7, 7);
    let save = g.call(base, "Base#run", 3, "", "save", &["Base"]);
    let tock = g.call(shared, "Shared#tick", 2, "", "tock", &["Shared"]);
    let tuck = g.call(shared, "Shared#tack", 4, "", "tuck", &["Shared"]);
    let dst = g.resolve();
    assert_eq!(dst[save].as_deref(), Some("base.rb#Base#save#method"));
    assert_eq!(dst[tock], None);
    assert_eq!(dst[tuck].as_deref(), Some("shared.rb#Shared#tuck#method"));

    let world = g.world();
    let index = Index::build(&world, &|_| true, &Tests::Without);
    for owner in ["Base", "Shared"] {
        let held = index.held(owner, Side::Instance).expect("a share");
        assert_eq!((held.shares.len(), held.single.len()), (1, 0), "{owner}");
    }
}

// R12-01: a model a concern's `included do` block defines a method on, or
// whose class macro on its base may define one, was looked up alone, so a
// base class's self calls cost one lookup per model again, 7 to 11 times
// main. Such a step is still read for a share, unsure of the names a block
// or a macro may define alone: those models are looked up one by one for
// those names (`dd`, `flag?`), shared for the others.
#[test]
fn c10_ruby_models_a_block_or_a_macro_defines_on_still_share_one_lookup() {
    // base.rb: class Base; def self.flag(n) define_method("#{n}?"); def
    // save; def run; save; dd; flag?
    // shared.rb: module Shared; extend ActiveSupport::Concern; included do
    // delegate :dd; end; def tick; tock; end; def tock
    // subN.rb: class SubN < Base; include Shared
    let (base, shared) = ("base.rb", "shared.rb");
    let pattern = format!("{}?", crate::mvp_index::ruby::unread::INTERPOLATED);
    let mut g = Generation::default();
    g.def(base, Class, "Base", 1, 6)
        .def(base, Method, "Base.flag", 2, 2)
        .def(base, Method, "Base#save", 3, 3)
        .def(base, Method, "Base#dd", 4, 4)
        .def(base, Method, "Base#flag?", 5, 5)
        .def(base, Method, "Base#run", 6, 6)
        .def(shared, Module, "Shared", 1, 7)
        .def(shared, Method, "Shared#tick", 5, 5)
        .def(shared, Method, "Shared#tock", 6, 6)
        .facts(
            base,
            &[Fact::Macro {
                owner: "Base".to_owned(),
                method: '.',
                side: '#',
                name: Some(pattern),
            }],
        )
        .facts(
            shared,
            &[Fact::Block {
                concern: "Shared".to_owned(),
                prepended: false,
                side: '#',
                name: Some("dd".to_owned()),
            }],
        );
    g.mixin(
        shared,
        "Shared",
        2,
        "extend",
        "ActiveSupport::Concern",
        &["Shared"],
    );
    for i in 0..50 {
        let (file, class) = (format!("sub{i}.rb"), format!("Sub{i}"));
        g.def(&file, Class, &class, 1, 3);
        g.mixin(&file, &class, 1, "superclass", "Base", &[]);
        g.mixin(&file, &class, 2, "include", "Shared", &[&class]);
    }
    let save = g.call(base, "Base#run", 6, "", "save", &["Base"]);
    let dd = g.call(base, "Base#run", 6, "", "dd", &["Base"]);
    let flag = g.call(base, "Base#run", 6, "", "flag?", &["Base"]);
    let tick = g.call(shared, "Shared#tick", 5, "", "tock", &["Shared"]);
    let dst = g.resolve();
    assert_eq!(dst[save].as_deref(), Some("base.rb#Base#save#method"));
    assert_eq!(dst[tick].as_deref(), Some("shared.rb#Shared#tock#method"));
    assert_eq!((dst[dd].as_deref(), dst[flag].as_deref()), (None, None));

    let world = g.world();
    let index = Index::build(&world, &|_| true, &Tests::Without);
    for owner in ["Base", "Shared"] {
        let held = index.held(owner, Side::Instance).expect("a share");
        assert_eq!((held.shares.len(), held.single.len()), (1, 0), "{owner}");
    }
}

#[test]
fn c10_ruby_self_calls_follow_prepend_owner_include_then_superclass() {
    // order.rb
    // 1 class Order < Record
    // 2   include Trackable      # defines `track` and `save`
    // 3   prepend Loud           # defines `save`
    // 4   def save; end
    // 5   def checkout
    // 6     track; save; label; self.track
    // record.rb: `class Record < ActiveRecord::Base; def label; end`.
    let (o, cref) = ("order.rb", ["Order"]);
    let mut g = Generation::default();
    g.def("trackable.rb", Module, "Trackable", 1, 4)
        .def("trackable.rb", Method, "Trackable#track", 2, 2)
        .def("trackable.rb", Method, "Trackable#save", 3, 3)
        .def("loud.rb", Module, "Loud", 1, 3)
        .def("loud.rb", Method, "Loud#save", 2, 2)
        .def("record.rb", Class, "Record", 1, 3)
        .def("record.rb", Method, "Record#label", 2, 2)
        .def(o, Class, "Order", 1, 8)
        .def(o, Method, "Order#save", 4, 4)
        .def(o, Method, "Order#checkout", 5, 7);
    g.mixin("record.rb", "Record", 1, "superclass", "Base", &[]);
    g.mixin(o, "Order", 1, "superclass", "Record", &[]);
    g.mixin(o, "Order", 2, "include", "Trackable", &cref);
    g.mixin(o, "Order", 3, "prepend", "Loud", &cref);
    let track = g.call(o, "Order#checkout", 6, "", "track", &cref);
    let save = g.call(o, "Order#checkout", 6, "", "save", &cref);
    let label = g.call(o, "Order#checkout", 6, "", "label", &cref);
    let on_self = g.call(o, "Order#checkout", 6, "self", "track", &cref);
    let dst = g.resolve();
    let tracked = Some("trackable.rb#Trackable#track#method");
    assert_eq!(dst[track].as_deref(), tracked);
    assert_eq!(dst[on_self].as_deref(), tracked);
    assert_eq!(dst[save].as_deref(), Some("loud.rb#Loud#save#method"));
    // The unknown step past Record (a gem's `Base`) comes after the hit.
    assert_eq!(dst[label].as_deref(), Some("record.rb#Record#label#method"));
}

#[test]
fn c10_ruby_a_concerns_included_include_lands_on_the_includer() {
    // concerns/admin/trackable.rb
    // 1 module Admin
    // 2   module Trackable
    // 3     extend ActiveSupport::Concern
    // 4     included do
    // 5       include Auditable       # Admin::Auditable defines audit
    // admin/order.rb: `module Admin; class Order; include Trackable;
    // def checkout; audit; end`.
    let (concern, in_concern) = ("trackable.rb", ["Admin::Trackable", "Admin"]);
    let (order, nesting) = ("admin/order.rb", ["Admin::Order", "Admin"]);
    let mut g = Generation::default();
    g.def(concern, Module, "Admin::Trackable", 2, 8)
        .def("auditable.rb", Module, "Admin::Auditable", 2, 4)
        .def("auditable.rb", Method, "Admin::Auditable#audit", 3, 3)
        .def(order, Class, "Admin::Order", 2, 6)
        .def(order, Method, "Admin::Order#checkout", 4, 5);
    let marker = "ActiveSupport::Concern";
    g.mixin(
        concern,
        "Admin::Trackable",
        3,
        "extend",
        marker,
        &in_concern,
    );
    g.mixin(
        concern,
        "Admin::Trackable",
        5,
        "included:include",
        "Auditable",
        &in_concern,
    );
    g.mixin(order, "Admin::Order", 3, "include", "Trackable", &nesting);
    let audit = g.call(order, "Admin::Order#checkout", 5, "", "audit", &nesting);
    let dst = g.resolve();
    let target = Some("auditable.rb#Admin::Auditable#audit#method");
    assert_eq!(dst[audit].as_deref(), target);
}

#[test]
fn c10_ruby_class_level_calls_follow_extends_and_concern_class_methods() {
    // order.rb
    // 1 class Order
    // 2   include Trackable      # a concern with `class_methods do
    // 3   extend Sortable        #   def tracked_since; end end`
    // 4   def self.report
    // 5     tracked_since; sort_key; track
    // Trackable also defines the instance method `track`.
    let (t, cref) = ("trackable.rb", ["Order"]);
    let mut g = Generation::default();
    g.def(t, Module, "Trackable", 1, 9)
        .def(t, Module, "Trackable::ClassMethods", 3, 6)
        .def(t, Method, "Trackable::ClassMethods#tracked_since", 4, 5)
        .def(t, Method, "Trackable#track", 7, 8)
        .def("sortable.rb", Module, "Sortable", 1, 3)
        .def("sortable.rb", Method, "Sortable#sort_key", 2, 2)
        .def("order.rb", Class, "Order", 1, 7)
        .def("order.rb", Method, "Order.report", 4, 6);
    g.mixin(
        t,
        "Trackable",
        2,
        "extend",
        "ActiveSupport::Concern",
        &["Trackable"],
    );
    g.mixin("order.rb", "Order", 2, "include", "Trackable", &cref);
    g.mixin("order.rb", "Order", 3, "extend", "Sortable", &cref);
    let since = g.call("order.rb", "Order.report", 5, "", "tracked_since", &cref);
    let sort = g.call("order.rb", "Order.report", 5, "", "sort_key", &cref);
    let track = g.call("order.rb", "Order.report", 5, "", "track", &cref);
    let dst = g.resolve();
    let class_method = Some("trackable.rb#Trackable::ClassMethods#tracked_since#method");
    assert_eq!(dst[since].as_deref(), class_method);
    assert_eq!(
        dst[sort].as_deref(),
        Some("sortable.rb#Sortable#sort_key#method")
    );
    // An instance method of the concern is not a method of the class.
    assert_eq!(dst[track], None);
}

#[test]
fn c10_ruby_a_concerns_included_extend_lands_on_the_includer() {
    // tracked.rb                         order.rb
    // 1 module Tracked                   1 class Order
    // 2   extend ActiveSupport::Concern  2   include Tracked
    // 3   included do                    3   def self.load
    // 4     extend Finders               4     find_by_slug
    // finders.rb: `module Finders; def find_by_slug; end; end`
    let mut g = Generation::default();
    g.def("tracked.rb", Module, "Tracked", 1, 6)
        .def("finders.rb", Module, "Finders", 1, 3)
        .def("finders.rb", Method, "Finders#find_by_slug", 2, 2)
        .def("order.rb", Class, "Order", 1, 6)
        .def("order.rb", Method, "Order.load", 3, 5);
    g.mixin(
        "tracked.rb",
        "Tracked",
        2,
        "extend",
        "ActiveSupport::Concern",
        &["Tracked"],
    );
    g.mixin(
        "tracked.rb",
        "Tracked",
        4,
        "included:extend",
        "Finders",
        &["Tracked"],
    );
    g.mixin("order.rb", "Order", 2, "include", "Tracked", &["Order"]);
    let call = g.call("order.rb", "Order.load", 4, "", "find_by_slug", &["Order"]);
    assert_eq!(
        g.resolve()[call].as_deref(),
        Some("finders.rb#Finders#find_by_slug#method")
    );
}

#[test]
fn c10_ruby_the_owners_own_definition_overrides_an_included_one() {
    // invoice.rb
    // 1 class Invoice
    // 2   include Trackable       # trackable.rb defines Trackable#track
    // 3   def track; end
    // 4   def close
    // 5     track
    let mut g = Generation::default();
    g.def("trackable.rb", Module, "Trackable", 1, 3)
        .def("trackable.rb", Method, "Trackable#track", 2, 2)
        .def("invoice.rb", Class, "Invoice", 1, 7)
        .def("invoice.rb", Method, "Invoice#track", 3, 3)
        .def("invoice.rb", Method, "Invoice#close", 4, 6);
    g.mixin(
        "invoice.rb",
        "Invoice",
        2,
        "include",
        "Trackable",
        &["Invoice"],
    );
    let call = g.call("invoice.rb", "Invoice#close", 5, "", "track", &["Invoice"]);
    assert_eq!(
        g.resolve()[call].as_deref(),
        Some("invoice.rb#Invoice#track#method")
    );
}

#[test]
fn c10_ruby_the_last_module_included_or_prepended_is_looked_up_first() {
    // a.rb
    // 1 class A
    // 2   include First; include Second    # both define `name`
    // 3   prepend Outer; prepend Inner     # both define `save`
    // 4   def save; end
    // 5   def run; name; save; end
    let mut g = Generation::default();
    g.def("mixins.rb", Module, "First", 1, 1)
        .def("mixins.rb", Method, "First#name", 1, 1)
        .def("mixins.rb", Module, "Second", 2, 2)
        .def("mixins.rb", Method, "Second#name", 2, 2)
        .def("mixins.rb", Module, "Outer", 3, 3)
        .def("mixins.rb", Method, "Outer#save", 3, 3)
        .def("mixins.rb", Module, "Inner", 4, 4)
        .def("mixins.rb", Method, "Inner#save", 4, 4)
        .def("a.rb", Class, "A", 1, 6)
        .def("a.rb", Method, "A#save", 4, 4)
        .def("a.rb", Method, "A#run", 5, 5);
    g.mixin("a.rb", "A", 2, "include", "First", &["A"]);
    g.mixin("a.rb", "A", 2, "include", "Second", &["A"]);
    g.mixin("a.rb", "A", 3, "prepend", "Outer", &["A"]);
    g.mixin("a.rb", "A", 3, "prepend", "Inner", &["A"]);
    let name = g.call("a.rb", "A#run", 5, "", "name", &["A"]);
    let save = g.call("a.rb", "A#run", 5, "", "save", &["A"]);
    let dst = g.resolve();
    assert_eq!(dst[name].as_deref(), Some("mixins.rb#Second#name#method"));
    assert_eq!(dst[save].as_deref(), Some("mixins.rb#Inner#save#method"));
}

#[test]
fn c10_ruby_mutual_includes_end_the_chain_without_hiding_object() {
    // cycle.rb
    // 1 module Ping; include Pong; def ping; end; end
    // 2 module Pong; include Ping; def pong; end; end
    // 3 class Cycler
    // 4   include Ping
    // 5   def run; pong; ping; helper; end
    // 7 require_relative "helpers"
    // helpers.rb: `def helper; end`. v1 left `helper` unresolved (no
    // name tier applied); decision 2: a chain known to its end reaches
    // `Object`, where top-level functions live.
    let mut g = Generation::default();
    g.def("cycle.rb", Module, "Ping", 1, 1)
        .def("cycle.rb", Method, "Ping#ping", 1, 1)
        .def("cycle.rb", Module, "Pong", 2, 2)
        .def("cycle.rb", Method, "Pong#pong", 2, 2)
        .def("cycle.rb", Class, "Cycler", 3, 6)
        .def("cycle.rb", Method, "Cycler#run", 5, 5)
        .function("helpers.rb", "helper", 1, 1);
    g.import("cycle.rb", 7, "require_relative", "helpers", "./helpers");
    g.mixin("cycle.rb", "Ping", 1, "include", "Pong", &["Ping"]);
    g.mixin("cycle.rb", "Pong", 2, "include", "Ping", &["Pong"]);
    g.mixin("cycle.rb", "Cycler", 4, "include", "Ping", &["Cycler"]);
    let pong = g.call("cycle.rb", "Cycler#run", 5, "", "pong", &["Cycler"]);
    let ping = g.call("cycle.rb", "Cycler#run", 5, "", "ping", &["Cycler"]);
    let helper = g.call("cycle.rb", "Cycler#run", 5, "", "helper", &["Cycler"]);
    let dst = g.resolve();
    assert_eq!(dst[pong].as_deref(), Some("cycle.rb#Pong#pong#method"));
    assert_eq!(dst[ping].as_deref(), Some("cycle.rb#Ping#ping#method"));
    assert_eq!(dst[helper].as_deref(), Some("helpers.rb#helper#function"));
}

#[test]
fn c10_ruby_a_superclass_cycle_or_disagreement_makes_the_rest_unknown() {
    // cycle.rb: `class Alpha < Beta; def run; helper; end; def
    // self.build; helper; end; end` and `class Beta < Alpha; end`.
    // split.rb: `class Gamma < Alpha` with `def run; helper; end`;
    // patch.rb reopens it as `class Gamma < Beta`. helpers.rb: `def
    // helper; end`, which cycle.rb and split.rb require. v1 ended
    // Alpha's class-level chain at the cycle as if it were complete.
    let mut g = Generation::default();
    g.def("cycle.rb", Class, "Alpha", 1, 5)
        .def("cycle.rb", Method, "Alpha#run", 2, 2)
        .def("cycle.rb", Method, "Alpha.build", 3, 3)
        .def("cycle.rb", Class, "Beta", 6, 7)
        .def("split.rb", Class, "Gamma", 1, 3)
        .def("split.rb", Method, "Gamma#run", 2, 2)
        .def("patch.rb", Class, "Gamma", 1, 2)
        .function("helpers.rb", "helper", 1, 1);
    g.import("cycle.rb", 8, "require_relative", "helpers", "./helpers");
    g.import("split.rb", 4, "require_relative", "helpers", "./helpers");
    g.mixin("cycle.rb", "Alpha", 1, "superclass", "Beta", &[]);
    g.mixin("cycle.rb", "Beta", 6, "superclass", "Alpha", &[]);
    g.mixin("split.rb", "Gamma", 1, "superclass", "Alpha", &[]);
    g.mixin("patch.rb", "Gamma", 1, "superclass", "Beta", &[]);
    let cycle = g.call("cycle.rb", "Alpha#run", 2, "", "helper", &["Alpha"]);
    let class_level = g.call("cycle.rb", "Alpha.build", 3, "", "helper", &["Alpha"]);
    let split = g.call("split.rb", "Gamma#run", 2, "", "helper", &["Gamma"]);
    let dst = g.resolve();
    assert_eq!(dst[cycle], None);
    assert_eq!(dst[class_level], None);
    assert_eq!(dst[split], None);
}

#[test]
fn c10_ruby_a_concerns_own_class_level_calls_see_the_extended_concern_module() {
    // trackable.rb
    // 1 module Trackable
    // 2   extend ActiveSupport::Concern
    // 3   def self.build; helper; prepared; end
    // 4 end
    // 5 require_relative "helpers"
    // helpers.rb: `def helper; end`; concern.rb (vendored) defines
    // `ActiveSupport::Concern#prepared`. The extended
    // `ActiveSupport::Concern` is a gem's module on the concern's own
    // chain, which v1 dropped; once the graph defines it (a vendored
    // Rails), its methods are found. The module itself reaches a
    // top-level function only past `Module`, whose methods the graph
    // does not hold (D31): never.
    let (t, cref) = ("trackable.rb", ["Trackable"]);
    let mut g = Generation::default();
    g.def(t, Module, "Trackable", 1, 4)
        .def(t, Method, "Trackable.build", 3, 3)
        .function("helpers.rb", "helper", 1, 1);
    g.import(t, 5, "require_relative", "helpers", "./helpers");
    g.mixin(t, "Trackable", 2, "extend", "ActiveSupport::Concern", &cref);
    let helper = g.call(t, "Trackable.build", 3, "", "helper", &cref);
    let prepared = g.call(t, "Trackable.build", 3, "", "prepared", &cref);
    assert_eq!(g.resolve()[helper], None);
    assert_eq!(g.resolve()[prepared], None);

    g.def("vendor/concern.rb", Module, "ActiveSupport::Concern", 2, 9)
        .def(
            "vendor/concern.rb",
            Method,
            "ActiveSupport::Concern#prepared",
            3,
            3,
        );
    let dst = g.resolve();
    assert_eq!(dst[helper], None);
    assert_eq!(
        dst[prepared].as_deref(),
        Some("vendor/concern.rb#ActiveSupport::Concern#prepared#method")
    );
}

#[test]
fn c10_ruby_class_methods_serve_the_includer_not_the_concern() {
    // vendor/concern.rb defines `module ActiveSupport::Concern`.
    // trackable.rb
    // 1 module Trackable
    // 2   extend ActiveSupport::Concern
    // 3   class_methods do
    // 4     def since; end
    // 5   end
    // 6   def self.build; since; end
    let (t, cref) = ("trackable.rb", ["Trackable"]);
    let mut g = Generation::default();
    g.def("vendor/concern.rb", Module, "ActiveSupport::Concern", 2, 9)
        .def(t, Module, "Trackable", 1, 7)
        .def(t, Module, "Trackable::ClassMethods", 3, 5)
        .def(t, Method, "Trackable::ClassMethods#since", 4, 4)
        .def(t, Method, "Trackable.build", 6, 6);
    g.mixin(t, "Trackable", 2, "extend", "ActiveSupport::Concern", &cref);
    let since = g.call(t, "Trackable.build", 6, "", "since", &cref);
    assert_eq!(g.resolve()[since], None);
}

#[test]
fn c10_ruby_an_unknown_module_before_a_hit_abstains() {
    // dynamic.rb
    // 1 class Dynamic
    // 2   include Sortable           # sortable.rb defines sort_key
    // 3   include(PLUGINS.first)     # looked up before Sortable
    // 4   prepend Audit              # a gem's module, before the owner
    // 5   def run; sort_key; name; end
    // 6   def name; end
    let mut g = Generation::default();
    g.def("sortable.rb", Module, "Sortable", 1, 3)
        .def("sortable.rb", Method, "Sortable#sort_key", 2, 2)
        .def("dynamic.rb", Class, "Dynamic", 1, 7)
        .def("dynamic.rb", Method, "Dynamic#run", 5, 5)
        .def("dynamic.rb", Method, "Dynamic#name", 6, 6);
    g.mixin(
        "dynamic.rb",
        "Dynamic",
        2,
        "include",
        "Sortable",
        &["Dynamic"],
    );
    g.mixin(
        "dynamic.rb",
        "Dynamic",
        3,
        "include",
        "PLUGINS.first",
        &["Dynamic"],
    );
    g.mixin("dynamic.rb", "Dynamic", 4, "prepend", "Audit", &["Dynamic"]);
    let sort = g.call("dynamic.rb", "Dynamic#run", 5, "", "sort_key", &["Dynamic"]);
    let own = g.call("dynamic.rb", "Dynamic#run", 5, "", "name", &["Dynamic"]);
    let dst = g.resolve();
    assert_eq!(dst[sort], None);
    assert_eq!(dst[own], None);
}

#[test]
fn c10_ruby_the_depth_cap_makes_the_rest_of_the_chain_unknown() {
    // deep.rb: module M33, then M32 including M33 below it, ..., M0
    // including M1; M31 defines `near`, M33 `far`; class Deep includes
    // M`first`; then `require_relative "helpers"`, which defines a
    // top-level `far`.
    // Module M_k is walked at depth k + 1 - first, so from M2 the chain
    // ends within the cap; from M0, M32's walk meets it, and every walk
    // around it, M31's to M0's, is kept as wholly unknown (R6-04): Deep's
    // chain is its own step, then an unknown one.
    let generation = |first: usize| {
        let mut g = Generation::default();
        for k in 0..=33 {
            g.def("deep.rb", Module, &format!("M{k}"), 34 - k, 34 - k);
        }
        g.def("deep.rb", Method, "M31#near", 3, 3)
            .def("deep.rb", Method, "M33#far", 1, 1)
            .def("deep.rb", Class, "Deep", 40, 43)
            .def("deep.rb", Method, "Deep#run", 42, 42)
            .function("helpers.rb", "far", 1, 1);
        g.import("deep.rb", 44, "require_relative", "helpers", "./helpers");
        for k in 0..33 {
            let (owner, module) = (format!("M{k}"), format!("M{}", k + 1));
            g.mixin(
                "deep.rb",
                &owner,
                34 - k,
                "include",
                &module,
                &[owner.as_str()],
            );
        }
        let first = format!("M{first}");
        g.mixin("deep.rb", "Deep", 41, "include", &first, &["Deep"]);
        let near = g.call("deep.rb", "Deep#run", 42, "", "near", &["Deep"]);
        let far = g.call("deep.rb", "Deep#run", 42, "", "far", &["Deep"]);
        let dst = g.resolve();
        (dst[near].clone(), dst[far].clone())
    };
    assert_eq!(
        generation(2),
        (
            Some("deep.rb#M31#near#method".to_owned()),
            Some("deep.rb#M33#far#method".to_owned())
        )
    );
    // Neither a module's `near` or `far` nor the top-level one: the rest
    // is unknown.
    assert_eq!(generation(0), (None, None));
}

#[test]
fn c10_ruby_declarations_in_two_files_abstain_when_two_ancestors_define_it() {
    // order.rb: `class Order; include Trackable; def checkout; track;
    // end` and order_ext.rb: `class Order; include Billing`.
    // invoice.rb: `class Invoice; prepend Loud; def close; save; end`
    // and invoice_ext.rb: `class Invoice; prepend Quiet`. Trackable and
    // Billing define `track`, Loud and Quiet `save`. The files load in
    // an order the graph cannot tell.
    let (order, invoice) = (["Order"], ["Invoice"]);
    let mut g = Generation::default();
    g.def("modules.rb", Method, "Trackable#track", 1, 1)
        .def("modules.rb", Method, "Billing#track", 2, 2)
        .def("modules.rb", Method, "Loud#save", 3, 3)
        .def("modules.rb", Method, "Quiet#save", 4, 4)
        .def("order.rb", Class, "Order", 1, 4)
        .def("order.rb", Method, "Order#checkout", 3, 3)
        .def("order_ext.rb", Class, "Order", 1, 3)
        .def("invoice.rb", Class, "Invoice", 1, 4)
        .def("invoice.rb", Method, "Invoice#close", 3, 3)
        .def("invoice_ext.rb", Class, "Invoice", 1, 3);
    for module in ["Trackable", "Billing", "Loud", "Quiet"] {
        g.def("modules.rb", Module, module, 1, 4);
    }
    g.mixin("order.rb", "Order", 2, "include", "Trackable", &order);
    g.mixin("order_ext.rb", "Order", 2, "include", "Billing", &order);
    g.mixin("invoice.rb", "Invoice", 2, "prepend", "Loud", &invoice);
    g.mixin("invoice_ext.rb", "Invoice", 2, "prepend", "Quiet", &invoice);
    let track = g.call("order.rb", "Order#checkout", 3, "", "track", &order);
    let save = g.call("invoice.rb", "Invoice#close", 3, "", "save", &invoice);
    let dst = g.resolve();
    assert_eq!(dst[track], None);
    assert_eq!(dst[save], None);
}

#[test]
fn c10_ruby_an_ancestor_is_the_module_its_nesting_names_never_a_same_tail_one() {
    // app/models/admin/order.rb: `module Admin; class Order; include
    // Trackable; def checkout; track; end`. Admin::Trackable defines no
    // `track`, Billing::Trackable does. A nested Admin::Order::Trackable
    // added later shadows Admin::Trackable.
    let mut g = Generation::default();
    let order = "app/models/admin/order.rb";
    g.def("admin/trackable.rb", Module, "Admin::Trackable", 2, 3)
        .def("billing/trackable.rb", Module, "Billing::Trackable", 2, 4)
        .def(
            "billing/trackable.rb",
            Method,
            "Billing::Trackable#track",
            3,
            3,
        )
        .def(order, Class, "Admin::Order", 2, 6)
        .def(order, Method, "Admin::Order#checkout", 4, 5);
    let nesting = ["Admin::Order", "Admin"];
    g.mixin(order, "Admin::Order", 3, "include", "Trackable", &nesting);
    let call = g.call(order, "Admin::Order#checkout", 5, "", "track", &nesting);
    assert_eq!(g.resolve()[call], None);

    g.def(
        "admin/order/trackable.rb",
        Module,
        "Admin::Order::Trackable",
        3,
        6,
    )
    .def(
        "admin/order/trackable.rb",
        Method,
        "Admin::Order::Trackable#track",
        4,
        5,
    );
    assert_eq!(
        g.resolve()[call].as_deref(),
        Some("admin/order/trackable.rb#Admin::Order::Trackable#track#method")
    );
}

#[test]
fn audited_lookup_should_consider_unknown_steps_after_the_hit_only_in_the_same_own_list() {
    use super::{At, Lookup, Node, Step};
    let mut g = Generation::default();
    g.def("a.rb", Module, "A", 1, 2)
        .def("a.rb", Method, "A#hit", 2, 2)
        .def("a.rb", Module, "B", 3, 4);
    g.facts(
        "a.rb",
        &[Fact::Unknown {
            owner: "B".into(),
            side: '#',
            name: Some("hit".into()),
        }],
    );
    let world = g.world();
    let index = Index::build(&world, &|_| true, &Tests::Without);
    let a = Step::new(Node::Known("A", Side::Instance));
    let b = Step::new(Node::Known("B", Side::Instance));
    let found = index.lookup_in(&[a, b], "hit");
    match found {
        Lookup::Found(at) => assert_eq!(index.symbols[at].name, "A#hit"),
        _ => panic!("a later unrelated step cannot precede the known hit"),
    }
    let prepended = Step {
        at: Some(At {
            file: "a.rb",
            line: 3,
            by: "B",
        }),
        ..a
    };
    assert_eq!(index.lookup_in(&[prepended, b], "hit"), Lookup::Abstain);
}
