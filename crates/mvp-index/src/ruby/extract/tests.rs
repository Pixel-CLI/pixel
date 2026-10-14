// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The walker's rows, one test per class of input. The fixtures named after
//! a v1 test are its sources byte for byte, and the rows they expect are
//! v1's (`crates/pixel-graph/src/extract.rs` tests, and the dumps of v1 on
//! the same bytes), mapped to the MVP: the `File` symbol for v1's `script`,
//! the qualified name as the name, the cref added; a change from v1 is
//! named in the test's comment.

use std::time::Duration;

use super::parse;
use super::testing::{ancestors, calls, imports, references, rows, symbols};
use crate::guard::over_budget;
use crate::text;

/// A definition's signature is its first line read from the start of the
/// line it opens on, so it is always a prefix of that trimmed line: a
/// two-line parameter list keeps its first line, `private def` keeps
/// `private`, a declaration's methods carry the declaring line.
#[test]
fn c10_a_signature_is_a_prefix_of_the_trimmed_line_at_its_symbol() {
    let source = "class Box; def tiny; end; end
module Util
  def multi(a,
            b)
  end
  private def hidden; end
  scope :active, -> {
    where(on: true)
  }
end
";
    let rows = rows("app/box.rb", source);
    assert_eq!(
        symbols(&rows),
        [
            "file app/box.rb 1-11",
            "class Box 1-1 | class Box; def tiny; end; end",
            "method Box#tiny 1-1 | class Box; def tiny; end",
            "module Util 2-10 | module Util",
            "method Util#multi 3-5 | def multi(a,",
            "method Util#hidden 6-6 | private def hidden; end",
            "method Util.active 7-9 | scope :active, -> {",
        ]
    );
    for symbol in &rows.symbols[1..] {
        let line = text::line(source, symbol.line).unwrap_or_default().trim();
        assert!(line.starts_with(&symbol.signature), "{symbol:?}");
    }
}

/// A call is at the line of its method name, where the source writes it:
/// a chain opened two lines above puts `.map` on its own line (v1 used the
/// line where the chain starts).
#[test]
fn c10_a_call_is_at_the_line_of_its_method_name() {
    let source = "class Feed
  def build
    items
      .select(&:ok)
      .map(&:id)
  end
end
";
    let rows = rows("app/feed.rb", source);
    assert_eq!(
        calls(&rows),
        [
            "5 Feed#build: map on items\n      .select(&:ok) [Feed]",
            "4 Feed#build: select on items [Feed]",
            "3 Feed#build: items [Feed]",
        ]
    );
    for edge in &rows.edges {
        let line = text::line(source, edge.line).unwrap_or_default();
        let name = super::Context::decode(&edge.context).name;
        assert!(line.contains(&name), "{name} at {}", edge.line);
    }
}

/// Every site carries the class and module nesting it is written in,
/// innermost first and fully qualified: `class Admin::Order` in `module
/// Shop` is the `Admin::Order` Ruby finds `Admin` for, the top-level one
/// here, since this file gives `Shop` no `Admin` (D09; v1 named it
/// `Shop::Admin::Order`), and does not open `Admin`; `class << self` opens
/// the singleton class's scope, `#<Class:Admin::Order>` (Ruby 3.3's
/// `Module.nesting` there, R2), and a `class_methods` block opens none. The
/// `class_methods` call itself is a call of the concern's body.
#[test]
fn the_cref_of_a_site_is_its_lexical_nesting() {
    let rows = rows(
        "app/shop.rb",
        "module Shop
  class Admin::Order
    def total; sum; end
    class << self
      def make; fetch; end
    end
  end
  module Tracked
    class_methods do
      def since; recent; end
    end
  end
  helper
end
top
",
    );
    assert_eq!(
        calls(&rows),
        [
            "3 Admin::Order#total: sum [Admin::Order, Shop]",
            "5 Admin::Order.make: fetch [#<Class:Admin::Order>, Admin::Order, Shop]",
            "9 Shop::Tracked: class_methods [Shop::Tracked, Shop]",
            "10 Shop::Tracked::ClassMethods#since: recent [Shop::Tracked, Shop]",
            "13 Shop: helper [Shop]",
            "15 app/shop.rb: top []",
        ]
    );
}

/// Ruby evaluates a superclass expression before it opens the class: its
/// constant and its calls are read in the scope outside, and its calls
/// belong to that scope (v1 gave them to the class, by line).
#[test]
fn a_superclass_is_read_in_the_scope_outside_its_class() {
    let rows = rows(
        "app/shop.rb",
        "module Shop
  class Order < Base
  end
  class Point < Struct.new(:x)
  end
end
",
    );
    assert_eq!(
        ancestors(&rows),
        [
            "2 Shop::Order: superclass Base [Shop]",
            "4 Shop::Point: superclass - [Shop]",
        ]
    );
    assert_eq!(calls(&rows), ["4 Shop: new on Struct [Shop]"]);
}

/// Code after `end` on a class's last line runs outside the class, which a
/// line span cannot tell: the cref and the owner come from the tree (v1
/// gave both calls to the definition spanning their line).
#[test]
fn code_after_end_on_a_class_line_is_outside_the_class() {
    let rows = rows(
        "app/shop.rb",
        "module Shop
  class Order; end; Order.build
end; Shop.boot
",
    );
    assert_eq!(
        calls(&rows),
        [
            "2 Shop: build on Order [Shop]",
            "3 app/shop.rb: boot on Shop []",
        ]
    );
}

/// `class ::Top` defines the top-level `Top`, wherever it is written (v1
/// qualified it `A::B::::Top`); its body still sees `A::B` lexically.
#[test]
fn a_rooted_class_name_is_qualified_from_the_top_level() {
    let rows = rows(
        "app/top.rb",
        "module A::B
  class ::Top
    def t; run; end
  end
end
",
    );
    assert_eq!(
        symbols(&rows),
        [
            "file app/top.rb 1-6",
            "module A::B 1-5 | module A::B",
            "class Top 2-4 | class ::Top",
            "method Top#t 3-3 | def t; run; end",
        ]
    );
    assert_eq!(calls(&rows), ["3 Top#t: run [Top, A::B]"]);
}

/// A multiple assignment through receivers calls each writer, as a single
/// one does (v1 recorded the readers); a bare name in it binds a local.
#[test]
fn a_multiple_assignment_calls_the_writers() {
    let rows = rows(
        "app/point.rb",
        "class Point
  def reset
    self.x, self.y = 0, 0
    @origin.a, (self.b, c) = 1, [2, 3]
    c
  end
end
",
    );
    assert_eq!(
        calls(&rows),
        [
            "3 Point#reset: x= on self [Point]",
            "3 Point#reset: y= on self [Point]",
            "4 Point#reset: a= on @origin [Point]",
            "4 Point#reset: b= on self [Point]",
        ]
    );
}

/// `super` names the next method in the ancestry, which the rules cannot
/// prove, and an identifier passed as an argument is a local or already a
/// call: neither is recorded (MVP decision 6). Their arguments still are.
#[test]
fn super_and_identifier_arguments_are_not_recorded() {
    let rows = rows(
        "app/child.rb",
        "class Child < Parent
  def run(x)
    super
    super(x, extra)
    notify(x, other)
  end
end
",
    );
    assert_eq!(
        calls(&rows),
        [
            "4 Child#run: extra [Child]",
            "5 Child#run: notify [Child]",
            "5 Child#run: other [Child]",
        ]
    );
    assert!(references(&rows).is_empty());
}

/// A site's target is what it names, as written and on one line: a call's
/// receiver and method, a constant, a class path, a spec.
#[test]
fn a_site_target_is_what_it_names_on_one_line() {
    let rows = rows(
        "app/models/order.rb",
        "require \"json\"
class Order < Base
  include Trackable
  has_many :items
  before_action :load
  def total
    User.where(on: true)
    items
      .select(&:paid)
      .sum
  end
end
",
    );
    let targets: Vec<String> = rows
        .edges
        .iter()
        .map(|e| format!("{} {} {}", e.kind, e.line, e.target))
        .collect();
    assert_eq!(
        targets,
        [
            "import 1 json",
            "reference 2 Base",
            "reference 3 Trackable",
            "call 3 include",
            "association 4 Item",
            "call 4 has_many",
            "call 5 before_action",
            "reference 5 load",
            "call 7 User.where",
            "call 10 items .select(&:paid).sum",
            "call 9 items.select",
            "call 8 items",
        ]
    );
}

/// Two definitions of one method in a file are both kept, the second under
/// a numbered uid: Ruby keeps the last, which resolution decides.
#[test]
fn a_method_defined_twice_keeps_both_rows_under_numbered_uids() {
    let rows = rows(
        "app/job.rb",
        "class Job
  def run; end
  def run; end
end
",
    );
    let uids: Vec<&str> = rows.symbols.iter().map(|s| s.uid.as_str()).collect();
    assert_eq!(
        uids,
        [
            "app/job.rb#app/job.rb#file",
            "app/job.rb#Job#class",
            "app/job.rb#Job#run#method",
            "app/job.rb#Job#run#method~2",
        ]
    );
}

/// A node past the depth cap is skipped with its subtree, so the recursive
/// walk stays within a thread's stack. v1
/// `ruby_walk_should_stop_at_the_depth_cap`.
#[test]
fn the_walk_stops_at_the_depth_cap() {
    let deep = format!("{}deep_call(){}", "[".repeat(600), "]".repeat(600));
    let rows = rows("gen.rb", &format!("shallow_call()\n{deep}\n"));
    assert_eq!(symbols(&rows), ["file gen.rb 1-3"]);
    assert_eq!(calls(&rows), ["1 gen.rb: shallow_call []"]);
}

/// The cap is inclusive: a call nested in 511 arrays sits at depth 512 and
/// is read, one more array puts it past the cap. v1
/// `walkers_should_visit_the_depth_cap_and_stop_past_it`.
#[test]
fn a_node_at_the_depth_cap_is_read_and_one_past_it_is_not() {
    let nested = |arrays: usize| {
        let source = format!("{}at_cap(){}\n", "[".repeat(arrays), "]".repeat(arrays));
        calls(&rows("gen.rb", &source))
    };
    assert_eq!(nested(super::MAX_DEPTH - 1), ["1 gen.rb: at_cap []"]);
    assert!(nested(super::MAX_DEPTH).is_empty());
}

/// A parse that took exactly its budget is within it.
#[test]
fn over_budget_is_strictly_past_the_budget() {
    let budget = Duration::from_millis(200);
    assert!(!over_budget(Duration::from_millis(199), budget));
    assert!(!over_budget(budget, budget));
    assert!(over_budget(budget + Duration::from_nanos(1), budget));
}

/// A parse past its budget is cancelled and yields no tree, as a grammar
/// failure does: a file that stalls the parser costs the run its budget,
/// not minutes.
#[test]
fn a_parse_past_its_budget_yields_no_tree() {
    let source = "def ok; call(1, 2, 3); end\n".repeat(20_000);
    assert!(parse(source.as_bytes(), Duration::from_secs(60)).is_some());
    assert!(parse(source.as_bytes(), Duration::ZERO).is_none());
}

/// A generated or minified blob is never parsed: its rows are its `File`
/// symbol alone.
#[test]
fn a_generated_blob_yields_only_its_file_symbol() {
    let blob = format!("x = [{}]\n", "1, ".repeat(30_000));
    let rows = rows("app/assets/bundle.rb", &blob);
    assert_eq!(symbols(&rows), ["file app/assets/bundle.rb 1-2"]);
    assert!(rows.edges.is_empty());
}

/// `class << self` and a bare `module_function` define methods of the
/// class or module itself (`Klass.name`), `module_function` with the
/// private instance copy Ruby makes (`Klass#name`, D15); the mode ends
/// where Ruby ends it, and never leaks out of a `def` body. `class <<
/// other`, and `class << self` where `self` is an instance (in `def
/// build`), define on an object the rules cannot name: no owner (D32;
/// Ruby 3.3 lists neither `elsewhere` nor `per_instance` among the
/// class's methods). v1 `ruby_singleton_defs_should_be_qualified_as_class_methods`
/// recorded both as instance methods.
#[test]
fn class_methods_are_qualified_with_a_dot_where_ruby_defines_them() {
    let rows = rows(
        "lib/user.rb",
        r#"
class User
  def self.direct; end
  class << self
    def importable; end
    private
    def hidden; end
    [1].each do
      def in_block; end
    end
  end
  def after; end
  class << other
    def elsewhere; end
  end
end

module Util
  def before; end
  module_function
  def slug(s); end
  def with_private_inside
    private
  end
  def still_function; end
  public
  def back_to_instance; end
end

module Plain
  def helper
    module_function
  end
  def instance_too; end
end

class Widget
  def build
    class << self
      def per_instance; end
    end
  end
  def self.setup
    class << self
      def meta; end
    end
  end
end

module Parens
  module_function()
  def a; end
  private :a
  def b; end
  public()
  def c; end
end
"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file lib/user.rb 1-58",
            "class User 2-16 | class User",
            "method User.direct 3-3 | def self.direct; end",
            "method User.importable 5-5 | def importable; end",
            "method User.hidden 7-7 | def hidden; end",
            "method User.in_block 9-9 | def in_block; end",
            "method User#after 12-12 | def after; end",
            "method elsewhere 14-14 | def elsewhere; end",
            "module Util 18-28 | module Util",
            "method Util#before 19-19 | def before; end",
            "method Util.slug 21-21 | def slug(s); end",
            "method Util#slug 21-21 | def slug(s); end",
            "method Util.with_private_inside 22-24 | def with_private_inside",
            "method Util#with_private_inside 22-24 | def with_private_inside",
            "method Util.still_function 25-25 | def still_function; end",
            "method Util#still_function 25-25 | def still_function; end",
            "method Util#back_to_instance 27-27 | def back_to_instance; end",
            "module Plain 30-35 | module Plain",
            "method Plain#helper 31-33 | def helper",
            "method Plain#instance_too 34-34 | def instance_too; end",
            "class Widget 37-48 | class Widget",
            "method Widget#build 38-42 | def build",
            "method per_instance 40-40 | def per_instance; end",
            "method Widget.setup 43-47 | def self.setup",
            "method Widget.meta 45-45 | def meta; end",
            "module Parens 50-57 | module Parens",
            "method Parens.a 52-52 | def a; end",
            "method Parens#a 52-52 | def a; end",
            "method Parens.b 54-54 | def b; end",
            "method Parens#b 54-54 | def b; end",
            "method Parens#c 56-56 | def c; end",
        ]
    );
    assert_eq!(
        calls(&rows),
        [
            "6 User: private [#<Class:User>, User]",
            "8 User: each on [1] [#<Class:User>, User]",
            "13 User: other [User]",
            "20 Util: module_function [Util]",
            "23 Util.with_private_inside: private [Util]",
            "26 Util: public [Util]",
            "32 Plain#helper: module_function [Plain]",
            "51 Parens: module_function [Parens]",
            "53 Parens: private [Parens]",
            "55 Parens: public [Parens]",
        ]
    );
    assert!(references(&rows).is_empty());
}

/// Modules, classes, `#` and `.` methods, a top-level function, paren-less
/// DSL calls, receiver calls owned by their method, and `require`s that
/// are imports, never calls. v1 `ruby_extracts_rails_symbols_calls_and_imports`.
#[test]
fn a_rails_controller_gives_its_symbols_calls_and_imports() {
    let rows = rows(
        "app/controllers/admin/users_controller.rb",
        r#"
require "json"
require_relative "../lib/pricing"

module Admin
  class UsersController < ApplicationController
    before_action :authenticate!

    def index
      @users = User.where(active: true)
      render json: @users
    end

    def self.permitted_params
    end

    private

    def authenticate!
    end
  end
end

def helper
end
"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file app/controllers/admin/users_controller.rb 1-26",
            "module Admin 5-22 | module Admin",
            "class Admin::UsersController 6-21 | class UsersController < ApplicationController",
            "method Admin::UsersController#index 9-12 | def index",
            "method Admin::UsersController.permitted_params 14-15 | def self.permitted_params",
            "method Admin::UsersController#authenticate! 19-20 | def authenticate!",
            "function helper 24-25 | def helper",
        ]
    );
    assert_eq!(
        calls(&rows),
        [
            "7 Admin::UsersController: before_action [Admin::UsersController, Admin]",
            "10 Admin::UsersController#index: where on User [Admin::UsersController, Admin]",
            "11 Admin::UsersController#index: render [Admin::UsersController, Admin]",
            "17 Admin::UsersController: private [Admin::UsersController, Admin]",
        ]
    );
    assert_eq!(
        references(&rows),
        ["7 Admin::UsersController: authenticate! (before_action) [Admin::UsersController, Admin]",]
    );
    assert_eq!(
        ancestors(&rows),
        ["6 Admin::UsersController: superclass ApplicationController [Admin]",]
    );
    assert_eq!(
        imports(&rows),
        [
            "2 app/controllers/admin/users_controller.rb: require json -> \"json\" []",
            "3 app/controllers/admin/users_controller.rb: require_relative ../lib/pricing -> \"../lib/pricing\" []",
        ]
    );
}

/// The `File` symbol owns top-level code (v1's `script`), spanning one
/// line past a final newline. v1
/// `ruby_root_calls_attach_to_script_while_method_calls_keep_method_owner`.
#[test]
fn top_level_calls_belong_to_the_file_and_method_calls_to_their_method() {
    let rows = rows(
        "scripts/run.rb",
        r#"root_call()

def worker
  nested_call()
end
"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file scripts/run.rb 1-6",
            "function worker 3-5 | def worker",
        ]
    );
    assert_eq!(
        calls(&rows),
        ["1 scripts/run.rb: root_call []", "4 worker: nested_call []",]
    );
}

/// A bare name, `name()`, `self.name`, a receiver, a splat or a block
/// argument: each calls the method unless a local has that name, and
/// `def target` itself is no call. v1
/// `ruby_bare_identifier_should_be_a_call_when_no_local_of_that_name_is_in_scope`;
/// the identifier argument is no reference (MVP decision 6).
#[test]
fn every_form_of_a_call_of_self_is_a_call_site() {
    let rows = rows(
        "app/svc.rb",
        r#"class Svc
  def chained
    @ids ||= target.to_set
  end
  def bare
    target
  end
  def parens
    target()
  end
  def with_self
    self.target
  end
  def as_argument
    render(target, *target, &target, key: target)
  end
  def target
    [1]
  end
end"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file app/svc.rb 1-20",
            "class Svc 1-20 | class Svc",
            "method Svc#chained 2-4 | def chained",
            "method Svc#bare 5-7 | def bare",
            "method Svc#parens 8-10 | def parens",
            "method Svc#with_self 11-13 | def with_self",
            "method Svc#as_argument 14-16 | def as_argument",
            "method Svc#target 17-19 | def target",
        ]
    );
    assert_eq!(
        calls(&rows),
        [
            "3 Svc#chained: to_set on target [Svc]",
            "3 Svc#chained: target [Svc]",
            "6 Svc#bare: target [Svc]",
            "9 Svc#parens: target [Svc]",
            "12 Svc#with_self: target on self [Svc]",
            "15 Svc#as_argument: render [Svc]",
            "15 Svc#as_argument: target [Svc]",
            "15 Svc#as_argument: target [Svc]",
            "15 Svc#as_argument: target [Svc]",
            "15 Svc#as_argument: target [Svc]",
        ]
    );
    assert!(references(&rows).is_empty());
}

/// Assignments, parameters of every kind, block and lambda parameters,
/// `for`, `rescue =>`, every pattern binding and `alias other target`
/// make `target` a local or a name: no read of it is a call. v1
/// `ruby_identifier_should_not_be_a_call_when_it_names_a_local`.
#[test]
fn c10_a_name_bound_as_a_local_is_no_call_site() {
    let rows = rows(
        "app/svc.rb",
        r#"class Svc
  def assigned
    target = 1
    target
  end
  def op_assigned
    target ||= 1
    target.succ
  end
  def multiple
    first, target = 1, 2
    target
  end
  def splat_assigned
    first, *target = 1, 2
    target
  end
  def nested_assigned
    first, (second, target) = 1, [2, 3]
    target
  end
  def positional(target)
    target
  end
  def optional(target = 1)
    target
  end
  def keyword(target: 1)
    target
  end
  def splat(*target)
    target
  end
  def double_splat(**target)
    target
  end
  def block_arg(&target)
    target
  end
  def block_param
    [1].each { |target| target }
  end
  def block_local
    [1].each { |x; target| target }
  end
  def destructured
    [[1, 2]].each { |(x, target)| target }
  end
  def lambda_param
    ->(target) { target }
  end
  def outer_local_in_block
    target = 1
    [1].each do target end
  end
  def looped
    for target in [1]; end
    target
  end
  def rescued
    raise 'x'
  rescue => target
    target
  end
  def matched_array(v)
    case v
    in [target] then target
    end
  end
  def matched_hash(v)
    case v
    in {k: target} then target
    end
  end
  def matched_shorthand(v)
    case v
    in {target:} then target
    end
  end
  def matched_as(v)
    case v
    in Integer => target then target
    end
  end
  def matched_pin(v, target)
    case v
    in ^target then 1
    end
  end
  alias other target
  def target
    [1]
  end
end"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file app/svc.rb 1-94",
            "class Svc 1-94 | class Svc",
            "method Svc#assigned 2-5 | def assigned",
            "method Svc#op_assigned 6-9 | def op_assigned",
            "method Svc#multiple 10-13 | def multiple",
            "method Svc#splat_assigned 14-17 | def splat_assigned",
            "method Svc#nested_assigned 18-21 | def nested_assigned",
            "method Svc#positional 22-24 | def positional(target)",
            "method Svc#optional 25-27 | def optional(target = 1)",
            "method Svc#keyword 28-30 | def keyword(target: 1)",
            "method Svc#splat 31-33 | def splat(*target)",
            "method Svc#double_splat 34-36 | def double_splat(**target)",
            "method Svc#block_arg 37-39 | def block_arg(&target)",
            "method Svc#block_param 40-42 | def block_param",
            "method Svc#block_local 43-45 | def block_local",
            "method Svc#destructured 46-48 | def destructured",
            "method Svc#lambda_param 49-51 | def lambda_param",
            "method Svc#outer_local_in_block 52-55 | def outer_local_in_block",
            "method Svc#looped 56-59 | def looped",
            "method Svc#rescued 60-64 | def rescued",
            "method Svc#matched_array 65-69 | def matched_array(v)",
            "method Svc#matched_hash 70-74 | def matched_hash(v)",
            "method Svc#matched_shorthand 75-79 | def matched_shorthand(v)",
            "method Svc#matched_as 80-84 | def matched_as(v)",
            "method Svc#matched_pin 85-89 | def matched_pin(v, target)",
            "method Svc#other 90-90 | alias other target",
            "method Svc#target 91-93 | def target",
        ]
    );
    assert_eq!(
        calls(&rows),
        [
            "8 Svc#op_assigned: succ on target [Svc]",
            "41 Svc#block_param: each on [1] [Svc]",
            "44 Svc#block_local: each on [1] [Svc]",
            "47 Svc#destructured: each on [[1, 2]] [Svc]",
            "54 Svc#outer_local_in_block: each on [1] [Svc]",
            "61 Svc#rescued: raise [Svc]",
        ]
    );
    assert_eq!(references(&rows), ["90 Svc#other: target (:alias) [Svc]",]);
}

/// `def`, `class` and `module` start a fresh local table; a block sees the
/// locals around it but its own die with it; a name read before its
/// assignment is still a call. v1
/// `ruby_local_should_be_visible_only_after_its_assignment_and_inside_its_scope`.
#[test]
fn c10_a_local_is_seen_only_after_its_assignment_and_inside_its_scope() {
    let rows = rows(
        "app/svc.rb",
        r#"target = 1
module Ns
  target
end
class Svc
  target = 2
  def gated
    target
  end
  def not_leaked
    [1].each { |target| target }
    [1].each { target = 1 }
    target
  end
  def before_assignment
    target
    target = 1
  end
  def self.class_side
    target
  end
  def matched_key_with_value(v)
    case v
    in {target: 1} then target
    end
  end
  class << self
    target
  end
  def target
  end
end"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file app/svc.rb 1-32",
            "module Ns 2-4 | module Ns",
            "class Svc 5-32 | class Svc",
            "method Svc#gated 7-9 | def gated",
            "method Svc#not_leaked 10-14 | def not_leaked",
            "method Svc#before_assignment 15-18 | def before_assignment",
            "method Svc.class_side 19-21 | def self.class_side",
            "method Svc#matched_key_with_value 22-26 | def matched_key_with_value(v)",
            "method Svc#target 30-31 | def target",
        ]
    );
    assert_eq!(
        calls(&rows),
        [
            "3 Ns: target [Ns]",
            "8 Svc#gated: target [Svc]",
            "11 Svc#not_leaked: each on [1] [Svc]",
            "12 Svc#not_leaked: each on [1] [Svc]",
            "13 Svc#not_leaked: target [Svc]",
            "16 Svc#before_assignment: target [Svc]",
            "20 Svc.class_side: target [Svc]",
            "24 Svc#matched_key_with_value: target [Svc]",
            "28 Svc: target [#<Class:Svc>, Svc]",
        ]
    );
}

/// `v => target`, `v in target` and `in (target)` bind `target`: the
/// pattern is no call, and neither is a later read. v1
/// `ruby_standalone_pattern_should_bind_its_names_like_a_case_in`.
#[test]
fn c10_a_standalone_pattern_binds_its_names() {
    let rows = rows(
        "app/svc.rb",
        r#"class Svc
  def rightward(v)
    v => target
    target
  end
  def boolean(v)
    v in target
    target
  end
  def parenthesized(v)
    case v
    in (target) then target
    end
  end
  def target
  end
end"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file app/svc.rb 1-17",
            "class Svc 1-17 | class Svc",
            "method Svc#rightward 2-5 | def rightward(v)",
            "method Svc#boolean 6-9 | def boolean(v)",
            "method Svc#parenthesized 10-14 | def parenthesized(v)",
            "method Svc#target 15-16 | def target",
        ]
    );
    assert!(calls(&rows).is_empty());
}

/// Ruby 3.1 `render(target:)` is `render(target: target)`: a call unless a
/// local `target` is in scope; a key written with its value calls
/// nothing. v1
/// `ruby_hash_shorthand_should_call_the_method_unless_a_local_has_that_name`.
#[test]
fn c10_a_hash_shorthand_calls_the_method_unless_a_local_has_that_name() {
    let rows = rows(
        "app/svc.rb",
        r#"class Svc
  def as_argument
    render(target:, other: 1)
  end
  def as_literal
    { target: }
  end
  def shadowed(target)
    render(target:)
  end
  def target
  end
end"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file app/svc.rb 1-13",
            "class Svc 1-13 | class Svc",
            "method Svc#as_argument 2-4 | def as_argument",
            "method Svc#as_literal 5-7 | def as_literal",
            "method Svc#shadowed 8-10 | def shadowed(target)",
            "method Svc#target 11-12 | def target",
        ]
    );
    assert_eq!(
        calls(&rows),
        [
            "3 Svc#as_argument: render [Svc]",
            "3 Svc#as_argument: target [Svc]",
            "6 Svc#as_literal: target [Svc]",
            "9 Svc#shadowed: render [Svc]",
        ]
    );
}

/// The rows v1's `ruby_callers_should_include_bare_and_receiver_position_calls`
/// resolves: every caller of `target` but the method reading a local.
#[test]
fn bare_receiver_and_self_calls_are_call_sites_and_a_local_read_is_not() {
    let rows = rows(
        "a.rb",
        r#"class Svc
  def chained
    @ids ||= target.to_set
  end
  def bare
    target
  end
  def parens
    target()
  end
  def with_self
    self.target
  end
  def shadowed
    target = 1
    target
  end
  def target
    [1]
  end
end
"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file a.rb 1-22",
            "class Svc 1-21 | class Svc",
            "method Svc#chained 2-4 | def chained",
            "method Svc#bare 5-7 | def bare",
            "method Svc#parens 8-10 | def parens",
            "method Svc#with_self 11-13 | def with_self",
            "method Svc#shadowed 14-17 | def shadowed",
            "method Svc#target 18-20 | def target",
        ]
    );
    assert_eq!(
        calls(&rows),
        [
            "3 Svc#chained: to_set on target [Svc]",
            "3 Svc#chained: target [Svc]",
            "6 Svc#bare: target [Svc]",
            "9 Svc#parens: target [Svc]",
            "12 Svc#with_self: target on self [Svc]",
        ]
    );
}

/// The rows v1's bare-call resolution tests read: each `access_logs` site
/// is owned by its own method, of whichever class and side.
#[test]
fn a_bare_call_belongs_to_the_method_it_is_written_in() {
    let rows = rows(
        "a.rb",
        r#"class Svc
  def run
    access_logs(user: 1)
  end
  def self.build
    access_logs(user: 2)
  end
  def access_logs(user:)
    [user]
  end
end

class Admin
  def run
    access_logs(user: 3)
  end
end

"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file a.rb 1-19",
            "class Svc 1-11 | class Svc",
            "method Svc#run 2-4 | def run",
            "method Svc.build 5-7 | def self.build",
            "method Svc#access_logs 8-10 | def access_logs(user:)",
            "class Admin 13-17 | class Admin",
            "method Admin#run 14-16 | def run",
        ]
    );
    assert_eq!(
        calls(&rows),
        [
            "3 Svc#run: access_logs [Svc]",
            "6 Svc.build: access_logs [Svc]",
            "15 Admin#run: access_logs [Admin]",
        ]
    );
}

/// The rows of v1's
/// `ruby_class_method_should_reach_a_class_self_method_across_build_and_update`.
#[test]
fn a_def_in_class_self_is_a_class_method() {
    let rows = rows(
        "catalog.rb",
        r#"class Catalog
  def self.load
    importable
  end

  class << self
    def importable
      []
    end
  end
end
"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file catalog.rb 1-12",
            "class Catalog 1-11 | class Catalog",
            "method Catalog.load 2-4 | def self.load",
            "method Catalog.importable 7-9 | def importable",
        ]
    );
    assert_eq!(calls(&rows), ["3 Catalog.load: importable [Catalog]",]);
}

/// Setters, operators, `?` and `!`, an endless def, `def obj.x`, nested and
/// namespaced classes, top-level `def self.x`, `class << self` and blocks.
/// Changes from v1: `class ::Top` in `module A::B` defines `Top`, not
/// `A::B::::Top`; a `def` in a top-level `class << self` or block
/// (`describe do`) is a method of no owner, not a function: it defines on
/// `main` or on whatever object runs the block, not on `Object`; `Point =
/// Struct.new(:x) do` defines the class `Point`, whose methods are the
/// member's `x` and `x=` and the block's `norm` (D01);
/// `def obj.on_object` defines on `obj`, which the rules cannot name (D32).
#[test]
fn method_name_forms_nested_classes_and_top_level_defs() {
    let rows = rows(
        "app/a1.rb",
        r#"class Foo::Bar
  def name=(v); end
  def ==(o); end
  def [](i); end
  def +@; end
  def valid?; end
  def save!; end
  def endless = compute(1)
  def obj.on_object; end
  class Inner
    def deep; end
  end
end
module A::B
  class ::Top
    def t; end
  end
end
def self.top_singleton; end
class << self
  def top_meta; end
end
Point = Struct.new(:x) do
  def norm; end
end
describe Foo do
  def helper; end
  it "works" do
    expect(helper).to eq(1)
  end
end
"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file app/a1.rb 1-32",
            "class Foo::Bar 1-13 | class Foo::Bar",
            "method Foo::Bar#name= 2-2 | def name=(v); end",
            "method Foo::Bar#== 3-3 | def ==(o); end",
            "method Foo::Bar#[] 4-4 | def [](i); end",
            "method Foo::Bar#+@ 5-5 | def +@; end",
            "method Foo::Bar#valid? 6-6 | def valid?; end",
            "method Foo::Bar#save! 7-7 | def save!; end",
            "method Foo::Bar#endless 8-8 | def endless = compute(1)",
            "method on_object 9-9 | def obj.on_object; end",
            "class Foo::Bar::Inner 10-12 | class Inner",
            "method Foo::Bar::Inner#deep 11-11 | def deep; end",
            "module A::B 14-18 | module A::B",
            "class Top 15-17 | class ::Top",
            "method Top#t 16-16 | def t; end",
            "method top_singleton 19-19 | def self.top_singleton; end",
            "method top_meta 21-21 | def top_meta; end",
            "class Point 23-25 | Point = Struct.new(:x) do",
            "method Point#x 23-23 | Point = Struct.new(:x)",
            "method Point#x= 23-23 | Point = Struct.new(:x)",
            "method Point#norm 24-24 | def norm; end",
            "method helper 27-27 | def helper; end",
        ]
    );
    assert_eq!(
        calls(&rows),
        [
            "8 Foo::Bar#endless: compute [Foo::Bar]",
            "23 app/a1.rb: new on Struct []",
            "26 app/a1.rb: describe []",
            "28 app/a1.rb: it []",
            "29 app/a1.rb: to on expect(helper) []",
            "29 app/a1.rb: expect []",
            "29 app/a1.rb: helper []",
            "29 app/a1.rb: eq []",
        ]
    );
}

/// `Foo.new(x)` and `Job.set(x)` keep their constant, any other receiver
/// its text. Two changes from v1: a call is at the line of its method name
/// (`.compact` at 16, not where its chain opens), and `self.a, self.b =`
/// calls the writers, as a single assignment does; `super(zz)` is no call.
#[test]
fn receivers_are_kept_as_written_and_assignments_call_writers() {
    let rows = rows(
        "app/a2.rb",
        r#"class R
  def m(x)
    Foo::Bar.new(x).call
    Job.set(wait: 1).perform_later
    foo.new.bar
    x&.y
    Foo::bar
    a.()
    Foo()
    Foo
    Foo::BAR
    valid?
    x.valid?
    items
      .map { |i| i }
      .compact
    self.a, self.b = 1, 2
    x.y += 1
    @x.z = 1
    x[1] = 2
    super(zz)
    yield ww
    foo bar
    foo(bar: baz)
    !negated
    q ? r : s
  end
end
"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file app/a2.rb 1-29",
            "class R 1-28 | class R",
            "method R#m 2-27 | def m(x)",
        ]
    );
    assert_eq!(
        calls(&rows),
        [
            "3 R#m: call on Foo::Bar.new [R]",
            "3 R#m: new on Foo::Bar [R]",
            "4 R#m: perform_later on Job.set [R]",
            "4 R#m: set on Job [R]",
            "5 R#m: bar on foo.new [R]",
            "5 R#m: new on foo [R]",
            "5 R#m: foo [R]",
            "6 R#m: y on x [R]",
            "7 R#m: bar on Foo [R]",
            "8 R#m: a [R]",
            "9 R#m: Foo [R]",
            "12 R#m: valid? [R]",
            "13 R#m: valid? on x [R]",
            "16 R#m: compact on items\n      .map { |i| i } [R]",
            "15 R#m: map on items [R]",
            "14 R#m: items [R]",
            "17 R#m: a= on self [R]",
            "17 R#m: b= on self [R]",
            "18 R#m: y= on x [R]",
            "18 R#m: y on x [R]",
            "19 R#m: z= on @x [R]",
            "21 R#m: zz [R]",
            "22 R#m: ww [R]",
            "23 R#m: foo [R]",
            "23 R#m: bar [R]",
            "24 R#m: foo [R]",
            "24 R#m: baz [R]",
            "25 R#m: negated [R]",
            "26 R#m: q [R]",
            "26 R#m: r [R]",
            "26 R#m: s [R]",
        ]
    );
}

/// `require_relative` keeps an explicit `./` path; a load-path spec written
/// as a relative or absolute path resolves to nothing (`""`); an
/// interpolated or computed spec, `Kernel.require` and `autoload` are no
/// import.
#[test]
fn every_require_form_gives_its_spec_and_resolvable_path() {
    let rows = rows(
        "app/a3.rb",
        r#"require "./x"
require "/abs/y"
require_relative "./rel"
require_relative "plain"
require_relative "../up"
load "tasks/x.rake"
require "a#{b}"
require name
require 'esc\tx'
Kernel.require "kern"
require_dependency "dep"
autoload :Foo, "foo/bar"
require("paren")
require "first", "second"
"#,
    );
    assert_eq!(symbols(&rows), ["file app/a3.rb 1-15",]);
    assert_eq!(
        calls(&rows),
        [
            "7 app/a3.rb: b []",
            "8 app/a3.rb: name []",
            "10 app/a3.rb: require on Kernel []",
            "12 app/a3.rb: autoload []",
        ]
    );
    assert_eq!(
        imports(&rows),
        [
            "1 app/a3.rb: require ./x -> \"\" []",
            "2 app/a3.rb: require /abs/y -> \"\" []",
            "3 app/a3.rb: require_relative ./rel -> \"./rel\" []",
            "4 app/a3.rb: require_relative plain -> \"./plain\" []",
            "5 app/a3.rb: require_relative ../up -> \"../up\" []",
            "6 app/a3.rb: load tasks/x.rake -> \"tasks/x.rake\" []",
            "9 app/a3.rb: require esc\\tx -> \"esc\\\\tx\" []",
            "11 app/a3.rb: require_dependency dep -> \"dep\" []",
            "13 app/a3.rb: require paren -> \"paren\" []",
            "14 app/a3.rb: require first -> \"first\" []",
        ]
    );
}

/// Lambda and proc parameters, destructured block parameters, `rescue A,
/// B => err`, hash, array and find patterns, `if (m = foo)`, `while (line =
/// gets)` and block-locals bind; an alternative pattern does not.
#[test]
fn c10_blocks_patterns_rescue_and_loops_bind_locals() {
    let rows = rows(
        "app/a7.rb",
        r#"class E
  def run
    lambda do |x| x end
    proc { |y| y }
    l = ->(z) do z end
    [1].each_with_object({}) { |(a, b), h| a; b; h }
    begin
      zz
    rescue Foo, Bar => err
      err
    end
    case v
    in {name: String => nm} then nm
    in [Integer => first, *rest] then rest
    in [*, 3, *post] then post
    in a | b then 1
    end
    if (m = foo)
      m
    end
    w, (u, *t) = 1
    t
    while (line = gets)
      line
    end
    1.times { |;blocal| blocal }
    def nested_def; end
    BEGIN_x = 1
  end
  def kw(a:, b: a, **nil)
    b
  end
  def fwd(...)
    other(...)
  end
end
"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file app/a7.rb 1-37",
            "class E 1-36 | class E",
            "method E#run 2-29 | def run",
            "method E#nested_def 27-27 | def nested_def; end",
            "method E#kw 30-32 | def kw(a:, b: a, **nil)",
            "method E#fwd 33-35 | def fwd(...)",
        ]
    );
    assert_eq!(
        calls(&rows),
        [
            "3 E#run: lambda [E]",
            "4 E#run: proc [E]",
            "6 E#run: each_with_object on [1] [E]",
            "8 E#run: zz [E]",
            "12 E#run: v [E]",
            "16 E#run: a [E]",
            "16 E#run: b [E]",
            "18 E#run: foo [E]",
            "23 E#run: gets [E]",
            "26 E#run: times on 1 [E]",
            "34 E#fwd: other [E]",
        ]
    );
}

/// `module_function :a`, `private def b` and `module_function def d` touch
/// only what they name: `module_function` gives the module its own copy
/// (`Util.a`, `Util.d`); a bare `module_function` applies to the module's
/// own `def`s and blocks, not to a nested class, and each `def` there is
/// both the module's method and its private instance copy (D15; Ruby 3.3:
/// `Mf.singleton_methods` and `Mf.private_instance_methods(false)` are
/// both `[:h, :i]`). A signature is cut from the start of its line
/// (`private def b; end`).
#[test]
fn module_function_with_arguments_leaves_the_def_mode_alone() {
    let rows = rows(
        "app/a8.rb",
        r#"module Util
  def a; end
  module_function :a
  private def b; end
  public def c; end
  module_function def d; end
  class << self
    def e; end
  end
  protected
  def f; end
end
module Mf
  module_function
  class Inner
    def g; end
  end
  def h; end
  [1].each do
    def i; end
  end
  private
  def j; end
end
"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file app/a8.rb 1-25",
            "module Util 1-12 | module Util",
            "method Util#a 2-2 | def a; end",
            "method Util.a 3-3 | module_function :a",
            "method Util#b 4-4 | private def b; end",
            "method Util#c 5-5 | public def c; end",
            "method Util.d 6-6 | module_function def d; end",
            "method Util#d 6-6 | module_function def d; end",
            "method Util.e 8-8 | def e; end",
            "method Util#f 11-11 | def f; end",
            "module Mf 13-24 | module Mf",
            "class Mf::Inner 15-17 | class Inner",
            "method Mf::Inner#g 16-16 | def g; end",
            "method Mf.h 18-18 | def h; end",
            "method Mf#h 18-18 | def h; end",
            "method Mf.i 20-20 | def i; end",
            "method Mf#i 20-20 | def i; end",
            "method Mf#j 23-23 | def j; end",
        ]
    );
    assert_eq!(
        calls(&rows),
        [
            "4 Util: private [Util]",
            "5 Util: public [Util]",
            "10 Util: protected [Util]",
            "14 Mf: module_function [Mf]",
            "19 Mf: each on [1] [Mf]",
            "22 Mf: private [Mf]",
        ]
    );
}

/// Calls inside interpolations and heredocs, `defined?`, optional-parameter
/// defaults and a modifier `unless` are calls; `super`, `yield` and
/// `method(:x)` are not; a two-line parameter list keeps its first line.
#[test]
fn super_yield_and_interpolations_are_read_as_ruby_runs_them() {
    let rows = rows(
        "app/b1.rb",
        r##"class F < Base; end
class G
  # comment
  def a
    super
    super()
    yield
    defined?(zork)
    "#{interp} and #{@iv}"
    :"sym#{symx}"
    /re#{rex}/
    %w[a b]
    method(:meth)
    list.each(&method(:amp))
    foo.bar do |x| x end
    [1].each { send(:in_block) }
    for k, v in pairs; k; v; end
    not_it = 1 unless cond
    not_it
    __method__
    def_local = <<~TXT
      #{heredoc_call}
    TXT
  end
  def opt(a, b = a, c = dflt)
  end
  def multi(a,
            b)
  end
end
require "dq\tesc"
"##,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file app/b1.rb 1-32",
            "class F 1-1 | class F < Base; end",
            "class G 2-30 | class G",
            "method G#a 4-24 | def a",
            "method G#opt 25-26 | def opt(a, b = a, c = dflt)",
            "method G#multi 27-29 | def multi(a,",
        ]
    );
    assert_eq!(
        calls(&rows),
        [
            "8 G#a: zork [G]",
            "9 G#a: interp [G]",
            "10 G#a: symx [G]",
            "11 G#a: rex [G]",
            "13 G#a: method [G]",
            "14 G#a: each on list [G]",
            "14 G#a: list [G]",
            "14 G#a: method [G]",
            "15 G#a: bar on foo [G]",
            "15 G#a: foo [G]",
            "16 G#a: each on [1] [G]",
            "16 G#a: send [G]",
            "17 G#a: pairs [G]",
            "18 G#a: cond [G]",
            "20 G#a: __method__ [G]",
            "22 G#a: heredoc_call [G]",
            "25 G#opt: dflt [G]",
        ]
    );
    assert_eq!(references(&rows), ["16 G#a: in_block (send) [G]",]);
    assert_eq!(ancestors(&rows), ["1 F: superclass Base []",]);
    assert_eq!(
        imports(&rows),
        ["31 app/b1.rb: require dqesc -> \"dqesc\" []",]
    );
}

/// `valid?` cannot be a local, so tree-sitter parses it as a call.
#[test]
fn a_predicate_without_receiver_is_a_call() {
    let rows = rows(
        "app/b2.rb",
        r#"class H
  def valid?; end
  def run
    valid?
    self.valid?
  end
end
"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file app/b2.rb 1-8",
            "class H 1-7 | class H",
            "method H#valid? 2-2 | def valid?; end",
            "method H#run 3-6 | def run",
        ]
    );
    assert_eq!(
        calls(&rows),
        ["4 H#run: valid? [H]", "5 H#run: valid? on self [H]",]
    );
}

/// A tree with a syntax error is still walked, outside the region the
/// grammar could not parse: `, &` (an anonymous block argument without
/// parentheses, which tree-sitter-ruby 0.23 cannot read) costs only itself.
#[test]
fn a_local_syntax_error_costs_only_its_own_region() {
    let rows = rows(
        "app/helpers/clip.rb",
        r#"module Clip
  def copy(url, &)
    tag.button class: "btn", data: {
      a: 1
    }, &
  end
  def other; run; end
end
"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file app/helpers/clip.rb 1-9",
            "module Clip 1-8 | module Clip",
            "method Clip#copy 2-6 | def copy(url, &)",
            "method Clip#other 7-7 | def other; run; end",
        ]
    );
    assert_eq!(
        calls(&rows),
        [
            "3 Clip#copy: button on tag [Clip]",
            "3 Clip#copy: tag [Clip]",
            "7 Clip#other: run [Clip]",
        ]
    );
}

/// A `require` belongs to the file wherever it is written (the contract in
/// `ruby/mod.rs`; v1 gave it to the definition spanning its line), and so
/// does top-level code, also when a declaration overrides a `def` and the
/// overridden body's sites go with it.
#[test]
fn file_level_sites_stay_the_files_beside_an_overridden_def() {
    let rows = rows(
        "app/a.rb",
        "require \"x\"
class A
  require \"y\"
  def name
    compute
  end
  attr_reader :name
end
boot
",
    );
    assert_eq!(
        symbols(&rows),
        [
            "file app/a.rb 1-10",
            "class A 2-8 | class A",
            "method A#name 7-7 | attr_reader :name",
        ]
    );
    assert_eq!(calls(&rows), ["9 app/a.rb: boot []"]);
    assert_eq!(
        imports(&rows),
        [
            "1 app/a.rb: require x -> \"x\" []",
            "3 app/a.rb: require y -> \"y\" [A]",
        ]
    );
}

/// When error recovery swallows a class, its `def`s would read as top-level
/// functions and the class after it as nested in nothing: a region the
/// grammar could not parse yields its `require`s and nothing else (v1 kept
/// `def ok` as a top-level function).
#[test]
fn a_region_the_grammar_cannot_parse_yields_only_its_requires() {
    let rows = rows(
        "app/b3.rb",
        r#"require "json"
class Broken
  require "set"
  def ok; end
  def bad(
end
class After
  def fine; end
end
"#,
    );
    assert_eq!(symbols(&rows), ["file app/b3.rb 1-10"]);
    assert_eq!(calls(&rows), Vec::<String>::new());
    assert_eq!(
        imports(&rows),
        [
            "1 app/b3.rb: require json -> \"json\" []",
            "3 app/b3.rb: require set -> \"set\" []",
        ]
    );
}

/// A site belongs to the definition the tree writes it in: `b` in `class C;
/// def a; b; end; end` is `C#a`'s, where v1, going by line, gave it to the
/// first definition spanning it, `C`. `x = x` and `y = y + 1` read locals.
#[test]
fn a_one_line_class_owns_the_calls_of_its_line() {
    let rows = rows(
        "app/c1.rb",
        r#"class C; def a; b; end; end
class D
  def m
    x = x
    y = y + 1
  end
end
"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file app/c1.rb 1-8",
            "class C 1-1 | class C; def a; b; end; end",
            "method C#a 1-1 | class C; def a; b; end",
            "class D 2-7 | class D",
            "method D#m 3-6 | def m",
        ]
    );
    assert_eq!(calls(&rows), ["1 C#a: b [C]",]);
}
