// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Calls without a receiver or on `self` (dynamic dispatch on the caller's
//! own class), shadowing by locals, and top-level functions.

use mvp_model::EdgeKind::{Call, Import, Reference};

use super::{Row, assert_unresolved, index, labelled, resolved, unresolved};

/// A bare name, a call with parentheses, `self.x` and a receiver that is a
/// bare name all call the caller's own method; a local of that name is not
/// a call. v1 `ruby_callers_should_include_bare_and_receiver_position_calls`.
#[test]
fn c10_ruby_shadowing_a_bare_name_calls_the_own_method_unless_a_local_shadows_it() {
    let rows = index(&[(
        "a.rb",
        "class Svc
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
",
    )]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Svc#chained", Call, 3, "Svc#target"),
            ("Svc#bare", Call, 6, "Svc#target"),
            ("Svc#parens", Call, 9, "Svc#target"),
            ("Svc#with_self", Call, 12, "Svc#target"),
        ])
    );
    // `to_set` is sent to whatever `target` returns; line 16 reads the
    // local, so it is no site at all.
    assert_eq!(
        unresolved(&rows),
        labelled(&[("Svc#chained", Call, 3, "target.to_set")])
    );
}

/// `self` in an instance method is an instance of its class: the bare call
/// reaches `Svc#access_logs`, whatever `Other` defines; in `Svc.build` it is
/// the class, which has no such method; `Admin` has none either. v1
/// `ruby_bare_call_should_reach_its_own_class_method_despite_a_same_name_method_elsewhere`
/// (and the store-level `ruby_self_call_resolves_only_with_matching_local_owner`,
/// `ruby_bare_call_should_resolve_to_the_callers_own_method_like_a_self_call`).
#[test]
fn c10_ruby_dynamic_dispatch_a_self_call_takes_its_own_class_and_side_only() {
    let rows = index(&[
        (
            "a.rb",
            "class Svc
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

",
        ),
        (
            "other.rb",
            "class Other\n  def access_logs\n    []\n  end\nend\n",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[("Svc#run", Call, 3, "Svc#access_logs")])
    );
    // v1 gave `Admin#run` an edge to `Svc#access_logs` once `other.rb` was
    // gone (its same-file tier, Q4); decision 1 makes that unresolved.
    assert_eq!(
        unresolved(&rows),
        labelled(&[
            ("Svc.build", Call, 6, "access_logs"),
            ("Admin#run", Call, 15, "access_logs"),
        ])
    );
}

/// Ruby keeps the definition of whichever file loads last: a class reopened
/// in another file that redefines the method leaves the call without a
/// target, bare or on `self`. v1
/// `ruby_bare_call_should_lose_its_exact_edge_when_a_reopened_class_redefines_it`
/// (full-build form) and `ruby_self_call_should_stay_unresolved_when_a_reopened_class_redefines_it`.
#[test]
fn c10_ruby_dynamic_dispatch_a_method_redefined_by_a_reopening_is_unresolved() {
    let rows = index(&[
        (
            "app/models/app.rb",
            "class App\n  def run\n    access_logs\n    self.access_logs\n  end\n  def access_logs\n  end\nend\n",
        ),
        (
            "config/initializers/app_patch.rb",
            "class App\n  def access_logs(user:)\n    []\n  end\nend\n",
        ),
    ]);
    assert_eq!(resolved(&rows), labelled(&[]));
    assert_unresolved(
        &rows,
        &[
            ("App#run", Call, 3, "access_logs"),
            ("App#run", Call, 4, "self.access_logs"),
        ],
    );
}

/// In `def self.load`, `self` is the class: a bare call reaches a method
/// defined in `class << self`, not `Other`'s instance method. v1
/// `ruby_class_method_should_reach_a_class_self_method_across_build_and_update`.
#[test]
fn c10_ruby_dynamic_dispatch_a_class_method_reaches_a_class_self_method() {
    let rows = index(&[
        (
            "catalog.rb",
            "class Catalog
  def self.load
    importable
  end

  class << self
    def importable
      []
    end
  end
end
",
        ),
        (
            "other.rb",
            "class Other\n  def importable\n    []\n  end\nend\n",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[("Catalog.load", Call, 3, "Catalog.importable")])
    );
}

/// The `self` of a call is where the tree writes it, not the definition
/// spanning its line: in `class C; def a; b; end; ...` the call is in `C#a`,
/// an instance; after `end` on a nested class's line it is in `M`. v1 went
/// by line: `b` belonged to `C`, so the class, and `y` to `K`.
#[test]
fn c10_ruby_dynamic_dispatch_the_caller_is_the_definition_the_call_is_written_in() {
    let rows = index(&[(
        "one_line.rb",
        "class C; def a; b; end; def b; end; def self.b; end; end
module M
  def self.y; end
  class K; def y; end; end; y
end
",
    )]);
    assert_eq!(
        resolved(&rows),
        labelled(&[("C#a", Call, 1, "C#b"), ("M", Call, 4, "M.y")])
    );
}

/// Top-level code runs on `main`: it reaches a top-level `def` of its own
/// file or of a file it requires, when no other file defines the name. A
/// script's `def` exists only once that script is loaded. v1
/// `ruby_root_call_resolves_from_a_script_scope` gave `script.rb`'s `foo()`
/// the only `foo` of the tree (its file tier), and
/// `ruby_unique_unqualified_t0_stays_exact` and
/// `ruby_unqualified_t0_is_unresolved_when_other_files_define_the_name`
/// pinned the same-file and two-file cases; the loaded-file condition is
/// this port's, after once-campfire's `bin/release` got `bin/bundle`'s
/// `gemfile` (decision 2 as written took any file).
#[test]
fn c10_ruby_imports_a_top_level_function_is_reached_from_a_file_that_loads_it() {
    let rows = index(&[
        ("lib.rb", "def foo\nend\n"),
        ("script.rb", "foo()\n"),
        ("loader.rb", "require_relative \"lib\"\nfoo()\n"),
        ("local.rb", "def helper; end\nhelper\n"),
        ("app_a.rb", "def application; end\napplication\n"),
        ("app_b.rb", "def application; end\n"),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("loader.rb", Import, 1, "lib.rb"),
            ("loader.rb", Call, 2, "foo"),
            ("local.rb", Call, 2, "helper"),
        ])
    );
    assert_eq!(
        unresolved(&rows),
        labelled(&[
            ("script.rb", Call, 1, "foo"),
            ("app_a.rb", Call, 2, "application"),
        ])
    );
}

/// A `def` in a block runs wherever the block is evaluated: `Module.new do`
/// defines on the new module, `describe do` on an example group. It is no
/// top-level function, and the calls in its body have a `self` the rules
/// cannot name, which may define even a name a top-level `def` of the file
/// has. once-campfire's `bin/bundle` defines its `gemfile` this way, and
/// v1 linked `bin/load`'s `gemfile do` (Bundler's inline Gemfile) to it.
#[test]
fn c10_ruby_dynamic_dispatch_a_def_in_a_top_level_block_is_no_function() {
    let rows = index(&[
        (
            "bin/bundle.rb",
            "def ruby_version; end
m = Module.new do
  def gemfile; end
  def lockfile
    gemfile
    ruby_version
  end
end
",
        ),
        (
            "bin/load.rb",
            "require_relative \"bundle\"\ngemfile do\nend\n",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[("bin/load.rb", Import, 1, "bin/bundle.rb")])
    );
    assert_unresolved(
        &rows,
        &[
            ("lockfile", Call, 5, "gemfile"),
            ("lockfile", Call, 6, "ruby_version"),
            ("bin/load.rb", Call, 2, "gemfile"),
        ],
    );
}

/// A method reaches a top-level function only through a lookup chain the
/// graph knows to its end, `Object`: an instance of a class with no
/// superclass does; the class itself does not, as it reaches `Object` only
/// past `Module` and `Class` (D31); a class whose superclass is a gem's
/// does not, and neither does a module's instance method, which runs on an
/// instance of whatever includes it (decision 2; the module case answers
/// the resolution port's open question 1).
#[test]
fn c10_ruby_dynamic_dispatch_a_top_level_function_needs_a_chain_known_to_object() {
    let rows = index(&[
        ("helpers.rb", "def slugify(text); end\n"),
        (
            "post.rb",
            "require_relative \"helpers\"
class Post
  def run; slugify(1); end
  def self.build; slugify(2); end
end
module Sluggable
  def slug; slugify(3); end
end
class Record < ActiveRecord::Base
  def run; slugify(4); end
end
",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("post.rb", Import, 1, "helpers.rb"),
            ("Post#run", Call, 3, "slugify"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            // The class reaches `Object` only past `Module` and `Class`,
            // whose methods the graph does not hold (D31).
            ("Post.build", Call, 4, "slugify"),
            ("Sluggable#slug", Call, 7, "slugify"),
            ("Record#run", Call, 10, "slugify"),
        ],
    );
}

/// A call whose receiver is not `self` or a constant names no class the
/// rules can prove: a local, an instance, class or global variable, a
/// chain, `self.class`. v1 resolved `user.save` (the only method of that
/// name, R4) and the others through its name tiers; decision 1 makes them
/// all unresolved.
#[test]
fn c10_ruby_dynamic_dispatch_a_receiver_of_unknown_class_gives_no_edge() {
    let rows = index(&[(
        "svc.rb",
        "class Svc
  def save; end
  def self.save; end
  def run(user, items)
    user.save
    @record.save
    @@cache.save
    $store.save
    items.first.save
    self.class.save
  end
end
",
    )]);
    assert_eq!(resolved(&rows), labelled(&[]));
    assert_unresolved(
        &rows,
        &[
            ("Svc#run", Call, 5, "user.save"),
            ("Svc#run", Call, 6, "@record.save"),
            ("Svc#run", Call, 7, "@@cache.save"),
            ("Svc#run", Call, 8, "$store.save"),
            ("Svc#run", Call, 9, "items.first.save"),
            ("Svc#run", Call, 10, "self.class.save"),
        ],
    );
}

/// Within one file the last definition wins, as Ruby loads it (decision 5;
/// v1 kept the last row through `INSERT OR REPLACE`).
#[test]
fn c10_ruby_shadowing_the_last_definition_in_a_file_wins() {
    let rows = index(&[(
        "a.rb",
        "class Svc
  def run; helper; end
  def helper; 1; end
  def helper; 2; end
end
def tool; 1; end
def tool; 2; end
tool
",
    )]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Svc#run", Call, 2, "Svc#helper"),
            ("a.rb", Call, 8, "tool")
        ])
    );
    let edges = &rows[0].edges;
    let target = |line: u32| {
        edges
            .iter()
            .find(|e| e.line == line && e.dst.is_some())
            .and_then(|e| e.dst.clone())
    };
    assert_eq!(target(2).as_deref(), Some("a.rb#Svc#helper#method~2"));
    assert_eq!(target(8).as_deref(), Some("a.rb#tool#function~2"));
}

/// A region the grammar cannot parse gives no definition and no call: error
/// recovery loses the class around its `def`s, which would read as
/// top-level functions any caller could reach (tree-sitter-ruby 0.23 reads
/// no anonymous block argument after a line continuation, and #107's
/// rewrite leaves one that has no blank before it). On
/// once-campfire, v1 linked `MessagesHelper#message_tag`'s `render` to the
/// `render` of a presenter broken this way.
#[test]
fn c10_ruby_dynamic_dispatch_a_broken_class_lends_no_target() {
    let rows = index(&[
        (
            "app/presenters/card.rb",
            "require_relative \"../helpers/tags\"
class Card
  def render(&)
    form_with \\
      model: 1,
&
  end

  def frame
    render
  end
end
",
        ),
        ("app/helpers/tags.rb", "def url; end\n"),
        (
            "app/views/page.rb",
            "require_relative \"../presenters/card\"\nrender\n",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("app/presenters/card.rb", Import, 1, "app/helpers/tags.rb"),
            ("app/views/page.rb", Import, 1, "app/presenters/card.rb"),
        ])
    );
    let card = &rows[1];
    assert_eq!(card.symbols.len(), 1, "only the File symbol: {card:#?}");
    assert_unresolved(&rows, &[("app/views/page.rb", Call, 2, "render")]);
}

/// A `def` in a block defines where the block runs: an iterator's block
/// runs on the class, so `iter` is `Outer#iter`; `Struct.new`, `Class.new`,
/// `Module.new` and `Data.define` assigned to a constant define that class
/// or module, whose methods the block's `def`s and the members' readers
/// (`Pair#y`) are; any other block runs
/// on another object (`class_eval` on another class, `instance_eval`, an
/// association's extension, `refine`), so its `def`s have no owner and no
/// call of `Outer` reaches them. An iterator over a literal that is not
/// empty runs its block, so `Outer#iter` exists (M2). Ruby 3.3 on this
/// file: `Outer.instance_methods(false)` is `[:iter, :run]`, and `run`
/// returns `Outer::Point.build` and `Outer::Kind#a1`, calls on a
/// constant that a constructor made, which abstain as on main (R12-03).
#[test]
fn c10_ruby_dynamic_dispatch_a_def_in_a_block_belongs_to_the_object_the_block_defines_on() {
    // D01
    let rows = index(&[
        ("lib/base.rb", "class Base\nend\n"),
        (
            "lib/outer.rb",
            "class Outer
  Point = Struct.new(:x) do
    def norm; end
    def self.build; end
  end
  Kind = Class.new(Base) do
    def a1; end
  end
  Mixin = Module.new do
    def a2; end
  end
  Other.class_eval do
    def a3; end
  end
  LOG.instance_eval do
    def a4; end
  end
  Pair = Data.define(:y) do
    def a5; y; end
  end
  [1].each do
    def iter; end
  end
  has_many :memberships do
    def grant; end
    def revise; grant; end
  end
  def run
    norm; a1; a2; a3; a4; a5; grant
    iter
    Point.build
    Kind.new.a1
  end
end
module Ref
  refine String do
    def a6; end
  end
end
",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Outer::Kind", Reference, 6, "Base"),
            ("Outer::Pair#a5", Call, 19, "Outer::Pair#y"),
            ("Outer#run", Call, 30, "Outer#iter"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("Outer#run", Call, 31, "Point.build"),
            ("Outer#run", Call, 32, "Kind.new.a1"),
            ("Outer::Point", Reference, 2, "Struct"),
            ("Outer::Pair", Reference, 18, "Data"),
            ("Outer#run", Call, 29, "norm"),
            ("Outer#run", Call, 29, "a1"),
            ("Outer#run", Call, 29, "a2"),
            ("Outer#run", Call, 29, "a3"),
            ("Outer#run", Call, 29, "a4"),
            ("Outer#run", Call, 29, "a5"),
            ("Outer#run", Call, 29, "grant"),
            ("revise", Call, 26, "grant"),
        ],
    );
}

/// `class << x` and `def x.m` define on `x`, and `class << self` in an
/// instance method on that instance: none is a method of the class, so a
/// bare call in it reaches none. Ruby 3.3: `Outer.new.run` raises
/// `NameError` for `elsewhere`, and `Outer.instance_methods(false)` is
/// `[:build, :run]`.
#[test]
fn c10_ruby_dynamic_dispatch_a_singleton_of_another_object_lends_the_class_nothing() {
    // D32
    let rows = index(&[(
        "lib/outer.rb",
        "class Outer
  class << LOGGER
    def elsewhere; end
  end
  def LOGGER.other; end
  def build
    class << self
      def per_instance; end
    end
  end
  def run; elsewhere; other; per_instance; end
end
",
    )]);
    assert_eq!(resolved(&rows), labelled(&[]));
    assert_unresolved(
        &rows,
        &[
            ("Outer#run", Call, 11, "elsewhere"),
            ("Outer#run", Call, 11, "other"),
            ("Outer#run", Call, 11, "per_instance"),
        ],
    );
}

/// A regexp literal with named groups on the left of `=~` binds each group
/// as a local, so a later bare `year` reads it; on the right it binds
/// nothing, and `month` stays a call. Ruby 3.3: `Parse.new.run` returns
/// `Parse#month`'s value, and reads `year` as `"2024"`.
#[test]
fn c10_ruby_shadowing_a_named_capture_of_a_match_is_a_local() {
    // D33
    let rows = index(&[(
        "lib/parse.rb",
        r#"class Parse
  def run
    if /(?<year>\d+)/ =~ "2024"
      year
    end
    "2024" =~ /(?<month>\d+)/
    month
  end
  def year; end
  def month; end
end
"#,
    )]);
    assert_eq!(
        resolved(&rows),
        labelled(&[("Parse#run", Call, 7, "Parse#month")])
    );
    assert!(
        unresolved(&rows).iter().all(|(_, _, line, _)| *line != 4),
        "line 4 reads a local"
    );
}

/// `module_function` copies each `def` as a private instance method, which
/// an `include` reaches before the superclass's method of the same name;
/// `module_function :tag` gives the module its own `Named.tag`. Ruby 3.3:
/// `Post.new.method(:label).owner` is `Util`, `:tag`'s is `Named`.
#[test]
fn c10_ruby_ancestors_a_module_function_is_also_an_instance_method() {
    // D15
    let rows = index(&[
        (
            "lib/util.rb",
            "module Util\n  module_function\n  def label; end\nend\n",
        ),
        (
            "lib/named.rb",
            "module Named\n  def tag; end\n  module_function :tag\nend\n",
        ),
        (
            "lib/base.rb",
            "class Base\n  def label; end\n  def tag; end\nend\n",
        ),
        (
            "lib/post.rb",
            "class Post < Base
  include Util
  include Named
  def show
    label
    tag
    Util.label
    Named.tag
  end
end
",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Post", Reference, 1, "Base"),
            ("Post", Reference, 2, "Util"),
            ("Post", Reference, 3, "Named"),
            ("Post#show", Call, 5, "Util#label"),
            ("Post#show", Call, 6, "Named#tag"),
            ("Post#show", Call, 7, "Util.label"),
            ("Post#show", Call, 8, "Named.tag"),
        ])
    );
}

/// `define_method(:helper)` defines `Post#helper`, which comes before
/// `Base#helper`; `define_singleton_method("build")` defines `Post.build`.
/// A computed name may be any name, so a lookup through `Dyn` proves
/// nothing, even `Base#other`. Ruby 3.3: `Post.new.show` calls the block
/// `define_method` was given.
#[test]
fn c10_ruby_dynamic_dispatch_define_method_defines_its_literal_name_only() {
    // D16
    let rows = index(&[
        (
            "lib/base.rb",
            "class Base\n  def helper; end\n  def other; end\nend\n",
        ),
        (
            "lib/post.rb",
            r#"class Post < Base
  define_method(:helper) { 1 }
  define_singleton_method("build") { 2 }
  def show
    helper
    Post.build
  end
end
"#,
        ),
        (
            "lib/dyn.rb",
            "class Dyn < Base
  [:a].each { |name| define_method(name) { 3 } }
  def show
    other
  end
end
",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Post", Reference, 1, "Base"),
            ("Dyn", Reference, 1, "Base"),
            ("Post#show", Call, 5, "Post#helper"),
            ("Post#show", Call, 6, "Post.build"),
        ])
    );
    assert_unresolved(&rows, &[("Dyn#show", Call, 4, "other")]);
}

/// `remove_method :tag` takes `Child#tag` away, so Ruby finds `Base#tag`;
/// `undef_method` and `undef` make it raise. A lookup that reaches the
/// owner for that name proves nothing. Ruby 3.3: `Child.new.tag` is
/// `"base"`, `Gone.new.run` and `Undone.new.run` raise `NameError`.
/// The fixture is the review's `remove_method` (F4).
#[test]
fn c10_ruby_dynamic_dispatch_a_removed_method_proves_no_target() {
    // F4
    let rows = index(&[(
        "main.rb",
        r#"class Base
  def tag; "base"; end
end
class Child < Base
  def tag; "removed"; end
  remove_method :tag
end
puts Child.new.tag
class Gone < Base
  def tag; end
  undef_method :tag
  def run; tag; end
end
class Undone < Base
  undef tag
  def run; tag; end
end
"#,
    )]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Child", Reference, 4, "Base"),
            ("Gone", Reference, 9, "Base"),
            ("Undone", Reference, 14, "Base"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("main.rb", Call, 8, "Child.new.tag"),
            ("Gone#run", Call, 12, "tag"),
            ("Undone#run", Call, 16, "tag"),
        ],
    );
}

/// Two definitions of a method in one class, one of them under a
/// condition, leave the choice to run time; unconditional ones keep the
/// last in the file. The same holds for a top-level function. Ruby 3.3:
/// `Cond.new.run` is `:new` (line 3), not the later line 5.
#[test]
fn c10_ruby_shadowing_a_definition_under_a_condition_proves_no_target() {
    // D06
    let rows = index(&[
        (
            "lib/cond.rb",
            r#"class Cond
  if RUBY_VERSION >= "3"
    def fast; :new; end
  else
    def fast; :old; end
  end
  def run; fast; end
  def slow; 1; end
  def slow; 2; end
  def walk; slow; end
end
"#,
        ),
        (
            "lib/tool.rb",
            r#"if ENV["X"]
  def tool; 1; end
else
  def tool; 2; end
end
tool
"#,
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[("Cond#walk", Call, 10, "Cond#slow")])
    );
    assert_unresolved(
        &rows,
        &[
            ("Cond#run", Call, 7, "fast"),
            ("lib/tool.rb", Call, 6, "tool"),
        ],
    );
}

/// A call on `self` in a method runs on an instance of the method's class
/// or of any subclass, or of any class including the method's module: it
/// resolves only when every one of them reaches the same method. The same
/// holds for a method of the class itself, and for a callback or a `send`.
/// A call on `Child.new` names its class. Ruby 3.3: `Child.new.run` calls
/// `Child#hook`, `Child.build` `Child.make`, `Person#greet` `Person#name`;
/// `stable`, `plain` and `wave` reach the one method every class shares.
#[test]
fn c10_ruby_dynamic_dispatch_a_self_call_holds_for_every_subclass() {
    // F1 (Astra's `inherited_self`)
    let rows = index(&[(
        "lib/base.rb",
        "class Base
  def hook; end
  def run; hook; end
  def stable; helper; end
  def helper; end
  def self.build; make; end
  def self.make; end
  def self.plain; tidy; end
  def self.tidy; end
end
class Child < Base
  def hook; end
  def self.make; end
end
Child.new.run
module Greeting
  def greet; name; end
  def wave; greet; end
  def name; end
end
class Person
  include Greeting
  def name; end
end
class Robot
  include Greeting
end
class Controller
  before_action :authenticate
  def authenticate; end
  def show; send(:authenticate); end
end
class AdminController < Controller
  def authenticate; end
end
",
    )]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Base#stable", Call, 4, "Base#helper"),
            ("Base.plain", Call, 8, "Base.tidy"),
            ("Child", Reference, 11, "Base"),
            ("lib/base.rb", Call, 15, "Base#run"),
            ("Greeting#wave", Call, 18, "Greeting#greet"),
            ("Person", Reference, 22, "Greeting"),
            ("Robot", Reference, 26, "Greeting"),
            ("AdminController", Reference, 33, "Controller"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("Base#run", Call, 3, "hook"),
            ("Base.build", Call, 6, "make"),
            ("Greeting#greet", Call, 17, "name"),
            ("Controller", Reference, 29, "authenticate"),
            ("Controller#show", Reference, 31, "authenticate"),
        ],
    );
}

/// A bare call in a top-level block runs on whatever object the method
/// given the block chooses (RSpec's `describe` and `it` run it on an
/// example group, Sinatra's `get` on a request): it never reaches a
/// top-level function. Straight top-level code runs on `main` and does,
/// unless a `def` in such a block, which defines on that object, may be
/// `main`'s (a block only yielded to defines on `Object`, R5-04).
/// rspec-core 3.13 gives `:example_group` for the call in `it`; `helper`
/// is the top-level function.
#[test]
fn c10_ruby_dynamic_dispatch_a_top_level_block_runs_on_an_object_the_rules_cannot_name() {
    // D08, R5-04
    let rows = index(&[
        (
            "spec/support/helpers.rb",
            "def current_user; end\ndef helper; end\n",
        ),
        (
            "spec/user_spec.rb",
            "require_relative \"support/helpers\"
describe \"User\" do
  def current_user; end
  it \"works\" do
    current_user
  end
end
current_user
helper
",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("spec/user_spec.rb", Import, 1, "spec/support/helpers.rb"),
            ("spec/user_spec.rb", Call, 9, "helper"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("spec/user_spec.rb", Call, 5, "current_user"),
            ("spec/user_spec.rb", Call, 8, "current_user"),
        ],
    );
}

/// In a method, a block keeps `self` unless the method given it runs it
/// elsewhere (`instance_eval` and its family, `Class.new`, and
/// `define_method`, whose block runs on an instance); in a class body,
/// only iteration keeps the class: Rails runs a callback's block or a
/// scope's lambda on a record or a relation, `with_options` on its option
/// merger, a concern's `included do` on the includer; a `class << self`
/// body runs on the singleton class. A callback declared in `included do`
/// is the includer's. Ruby 3.3 with Active Support 7.1: `map`, a plain
/// method's block and a lambda called right away reach `Widget#helper`;
/// `instance_eval`, `Class.new` and `included do` raise NameError.
#[test]
fn c10_ruby_dynamic_dispatch_a_block_keeps_self_unless_its_method_rebinds_it() {
    // D08
    let rows = index(&[(
        "lib/widget.rb",
        "class Widget
  def helper; end
  def self.visible; end
  def run
    [1].map { helper }
    Fetch.cached { helper }
    other.instance_eval { helper }
    Class.new { helper }
    -> { helper }.call
  end
  def self.build
    define_method(:x) { visible }
  end
  [1].each { visible }
  before_save do
    helper
  end
  scope :recent, -> { visible }
  with_options if: :ready? do
    visible
  end
  class << self
    visible
  end
end
module Tracked
  extend ActiveSupport::Concern
  def self.helper; end
  def stamp; end
  included do
    helper
    before_save :stamp
  end
end
class Order
  include Tracked
end
",
    )]);
    let calls: Vec<Row> = resolved(&rows)
        .into_iter()
        .filter(|row| row.1 != Import)
        .collect();
    assert_eq!(
        calls,
        vec![
            ("Order".to_owned(), Reference, 36, "Tracked".to_owned()),
            (
                "Tracked".to_owned(),
                Reference,
                32,
                "Tracked#stamp".to_owned()
            ),
            ("Widget".to_owned(), Call, 14, "Widget.visible".to_owned()),
            ("Widget#run".to_owned(), Call, 5, "Widget#helper".to_owned()),
            ("Widget#run".to_owned(), Call, 6, "Widget#helper".to_owned()),
            ("Widget#run".to_owned(), Call, 9, "Widget#helper".to_owned()),
        ]
    );
    assert_unresolved(
        &rows,
        &[
            ("Widget#run", Call, 7, "helper"),
            ("Widget#run", Call, 8, "helper"),
            ("Widget.build", Call, 12, "visible"),
            ("Widget", Call, 16, "helper"),
            ("Widget.recent", Call, 18, "visible"),
            ("Widget", Call, 20, "visible"),
            ("Widget", Call, 23, "visible"),
            ("Tracked", Call, 31, "helper"),
        ],
    );
}

/// Top-level code runs in order: a function defined below the call, or
/// in a file required below it, does not exist yet (Ruby raises
/// NameError); a call in a `def` runs later and sees both.
#[test]
fn c10_ruby_imports_top_level_code_sees_only_what_is_loaded_above_it() {
    // D34
    let rows = index(&[
        ("lib/helpers.rb", "def helper; end\n"),
        (
            "bin/script.rb",
            "helper
require_relative \"../lib/helpers\"
early
def early; end
helper
early
def later_caller
  late
end
def late; end
",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("bin/script.rb", Import, 2, "lib/helpers.rb"),
            ("bin/script.rb", Call, 5, "helper"),
            ("bin/script.rb", Call, 6, "early"),
            ("later_caller", Call, 8, "late"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("bin/script.rb", Call, 1, "helper"),
            ("bin/script.rb", Call, 3, "early"),
        ],
    );
}

/// `class_eval`, `class_exec`, `module_eval` and `module_exec` sent to the
/// class or module whose body they are in define its instance methods;
/// `instance_eval` defines a `def` on the class itself, an `attr_reader`
/// sent to it still on its instances. Ruby 3.3: `K.new.run` is `[:k, :k,
/// :k, nil]` (`K#d` the reader), `K.go` `K.s`, `K2.new.run` `M#mm`.
#[test]
fn c10_ruby_dynamic_dispatch_a_block_evaluated_on_its_own_class_defines_there() {
    // R1
    let rows = index(&[(
        "lib/k.rb",
        "class Base
  def a; end; def b; end; def c; end; def d; end
  def self.s; end
end
class K < Base
  class_eval do
    def a; end
  end
  self.class_exec do
    def b; end
  end
  module_exec { def c; end }
  instance_eval do
    def s; end
    attr_reader :d
  end
  def run; a; b; c; d; end
  def self.go; s; end
end
module M
  module_eval do
    def mm; end
  end
end
class Base2; def mm; end; end
class K2 < Base2
  include M
  def run; mm; end
end
",
    )]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("K", Reference, 5, "Base"),
            ("K#run", Call, 17, "K#a"),
            ("K#run", Call, 17, "K#b"),
            ("K#run", Call, 17, "K#c"),
            ("K#run", Call, 17, "K#d"),
            ("K.go", Call, 18, "K.s"),
            ("K2", Reference, 26, "Base2"),
            ("K2", Reference, 27, "M"),
            ("K2#run", Call, 28, "M#mm"),
        ])
    );
}

/// A `def` nested in a method defines, when the method runs, a method of
/// the class the outer one was defined in: in a method of `class << self`
/// one of the class itself, in `def self.m` an instance method; until it
/// runs there is none, so no call proves one (M2). Ruby 3.3: after
/// `Outer.make` and `Outer.make2`, `Outer.singleton_methods` holds
/// `built`, `Outer.instance_methods(false)` `built2`; `Outer.go` is
/// `:built`, and `built` in `Outer#run` raises NameError.
#[test]
fn c10_ruby_dynamic_dispatch_a_def_nested_in_a_method_defines_on_its_lexical_class() {
    // R14
    let rows = index(&[(
        "lib/outer.rb",
        "class Outer
  class << self
    def make
      def built; end
    end
  end
  def self.make2
    def built2; end
  end
  def run; built; built2; end
  def self.go; built; end
end
",
    )]);
    assert_eq!(resolved(&rows), labelled(&[]));
    assert_unresolved(
        &rows,
        &[
            ("Outer#run", Call, 10, "built"),
            ("Outer#run", Call, 10, "built2"),
            ("Outer.go", Call, 11, "built"),
        ],
    );
    let names: Vec<&str> = rows
        .iter()
        .flat_map(|r| &r.symbols)
        .map(|s| s.name.as_str())
        .collect();
    assert!(names.contains(&"Outer.built") && names.contains(&"Outer#built2"));
}

/// Code evaluated on a class from outside its body (`Other.class_eval do
/// def`, `Child2.send(:remove_method, :tag)`, `Child3.class_eval {
/// remove_method }`) changes it where the rules do not replay it, and a
/// string `class_eval` in its body defines what the string says: a lookup
/// reaching it for those names proves nothing, any name for an
/// interpolated string. Ruby 3.3: `K.new.run` is `[:k, :base]`, `L.new.run`
/// `[:l, :base]`, `C.new.r` `[:other, :base, :base, :k]`.
#[test]
fn c10_ruby_dynamic_dispatch_code_evaluated_on_a_class_proves_nothing_it_may_define() {
    // R9, R10
    let rows = index(&[(
        "lib/a.rb",
        "class Base; def a3; end; def tag; end; def foo; end; def bar; end; end
class Other < Base; end
class Outer
  Other.class_eval do
    def a3; end
  end
end
class Child2 < Base; def tag; end; end
Child2.send(:remove_method, :tag)
class Child3 < Base; def tag; end; end
Child3.class_eval { remove_method :tag }
class K < Base
  %w[foo].each do |n|
    class_eval <<-M
      def #{n}; end
    M
  end
  def run; foo; tag; end
end
class L < Base
  class_eval \"def bar; end\"
  def run; bar; tag; end
end
class C
  def r; Other.new.a3; Child2.new.tag; Child3.new.tag; K.new.foo; end
end
",
    )]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Other", Reference, 2, "Base"),
            ("Child2", Reference, 8, "Base"),
            ("Child3", Reference, 10, "Base"),
            ("K", Reference, 12, "Base"),
            ("L", Reference, 20, "Base"),
            ("L#run", Call, 22, "Base#tag"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("K#run", Call, 18, "foo"),
            ("K#run", Call, 18, "tag"),
            ("L#run", Call, 22, "bar"),
            ("C#r", Call, 25, "Other.new.a3"),
            ("C#r", Call, 25, "Child2.new.tag"),
            ("C#r", Call, 25, "Child3.new.tag"),
            ("C#r", Call, 25, "K.new.foo"),
        ],
    );
}

/// A call on `self` in a module runs on every object that includes it,
/// through another module too (`Object.new.extend(Json)`), and in a core
/// class on instances of subclasses the graph does not hold: Ruby 3.3
/// gives `:json` for `Object.new.extend(Json).load(1)` and `:foo` for
/// `Foo.new.blank?` from `Object#present?`; `Wrap` overrides nothing.
#[test]
fn c10_ruby_dynamic_dispatch_a_self_call_holds_for_every_module_and_core_subclass() {
    // R4, R5
    let rows = index(&[
        (
            "lib/ser.rb",
            "module Ser
  def load(d); detect(d); end
  def detect(d); end
end
module Json
  include Ser
  def detect(d); end
end
module Plain
  def run; helper; end
  def helper; end
end
module Wrap
  include Plain
end
",
        ),
        (
            "lib/blank.rb",
            "class Object
  def present?; blank?; end
  def blank?; end
end
class Foo
  def blank?; end
end
",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Json", Reference, 6, "Ser"),
            ("Plain#run", Call, 10, "Plain#helper"),
            ("Wrap", Reference, 14, "Plain"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("Ser#load", Call, 2, "detect"),
            ("Object#present?", Call, 2, "blank?"),
        ],
    );
}

/// A module included where the rules do not replay it (`W4.include H4`)
/// runs on an object whose own methods come first; a `new` defined through
/// `singleton_class.send(:define_method)` is a custom `new`; `extend self`
/// puts the module's methods on the module itself, where a call on it
/// abstains as on main (R12-03). Ruby 3.3: `W4.new.run` is `:w4`,
/// `A7.new.run` `:proxy`, `Guard.resolve` `Guard#resolve`.
#[test]
fn c10_ruby_dynamic_dispatch_a_mixin_or_new_outside_the_body_proves_nothing() {
    // R15
    let rows = index(&[(
        "lib/a.rb",
        "module H4
  def run; label; end
  def label; end
end
class W4; def label; end; end
W4.include H4
class Proxy; def run; end; end
class A7; def run; end; end
class A7
  singleton_class.send(:define_method, :new) { Proxy.allocate }
end
module Guard
  extend self
  def resolve; end
end
class C; def r; A7.new.run; Guard.resolve; end; end
",
    )]);
    assert_eq!(
        resolved(&rows),
        labelled(&[("Guard", Reference, 13, "Guard")])
    );
    assert_unresolved(
        &rows,
        &[
            ("H4#run", Call, 2, "label"),
            ("C#r", Call, 16, "A7.new.run"),
            ("C#r", Call, 16, "Guard.resolve"),
        ],
    );
}

/// A class or module nested in a core or standard-library namespace may be
/// Ruby's (`URI::Generic`, `Net::HTTP`, `Gem::Command`), reopened with
/// methods, a superclass and subclasses the graph does not hold, unless it
/// declares a superclass of the graph's (an application's `Data::Export <
/// Base`, R11): a call on `self` in it proves nothing, and never reaches a
/// top-level function (R5-10). Ruby 3.3 with `uri` loaded:
/// `URI("https://x").secure?` is `true` (`URI::Generic#scheme`, not the
/// top-level `scheme`), `Data::Export.new.run` is `:export`.
#[test]
fn c10_ruby_dynamic_dispatch_a_class_in_a_stdlib_namespace_may_be_rubys() {
    // R11, R5-10
    let rows = index(&[
        (
            "lib/helpers.rb",
            "def scheme; :top; end
def helper; :top; end
",
        ),
        (
            "lib/a.rb",
            "require_relative \"helpers\"
module Net
  class A
    def run; helper; end
    def helper; end
  end
  class B < A; end
end
class Gem::Command
  attr_reader :options
  def run; options; end
end
class String
  def shout; upcase; end
end
class URI::Generic
  def secure?; scheme == \"https\"; end
end
class Base; end
class Data::Export < Base
  def run; helper; end
  def helper; :export; end
end
",
        ),
    ]);
    let calls: Vec<Row> = resolved(&rows)
        .into_iter()
        .filter(|row| row.1 == Call)
        .collect();
    assert_eq!(
        calls,
        vec![(
            "Data::Export#run".to_owned(),
            Call,
            21,
            "Data::Export#helper".to_owned()
        )]
    );
    assert_unresolved(
        &rows,
        &[
            ("Net::A#run", Call, 4, "helper"),
            ("Gem::Command#run", Call, 11, "options"),
            ("String#shout", Call, 14, "upcase"),
            ("URI::Generic#secure?", Call, 17, "scheme"),
        ],
    );
    assert!(resolved(&rows).contains(&("Net::B".to_owned(), Reference, 7, "Net::A".to_owned())));
}

/// A block passed to a method the rules do not model may be only yielded
/// to, with the class as `self` and definee: what it defines may be a
/// method of the class. Ruby 3.3 (`some_dsl` yields, `grep` too):
/// `Widget.new.run` is `[:widget, :widget, :base]`.
#[test]
fn c10_ruby_dynamic_dispatch_a_block_a_method_may_yield_defines_on_the_class() {
    // R1
    let rows = index(&[(
        "lib/widget.rb",
        "class Base
  def x; end; def y; end; def w; end
  def self.some_dsl; yield; end
end
class Widget < Base
  some_dsl do
    def x; end
  end
  %i[y].grep(/y/) do
    alias_method :y, :x
  end
  def run; x; y; w; end
end
",
    )]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Widget", Reference, 5, "Base"),
            ("Widget", Call, 6, "Base.some_dsl"),
            ("Widget#run", Call, 12, "Base#w"),
        ])
    );
    assert_unresolved(
        &rows,
        &[("Widget#run", Call, 12, "x"), ("Widget#run", Call, 12, "y")],
    );
}

/// A `require` under a condition, in a block or in a method body may not
/// run when the file loads: what it loads is no proof of a function.
/// Ruby 3.3: `helper_a` raises NameError at line 2.
#[test]
fn c10_ruby_imports_a_conditional_require_loads_nothing_proven() {
    // R13
    let rows = index(&[
        ("lib/a.rb", "def helper_a; end\n"),
        ("lib/b.rb", "def helper_b; end\n"),
        (
            "bin/s.rb",
            "require_relative \"../lib/a\" if false
helper_a
require_relative \"../lib/b\"
helper_b
def run
  helper_a
end
",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("bin/s.rb", Import, 1, "lib/a.rb"),
            ("bin/s.rb", Import, 3, "lib/b.rb"),
            ("bin/s.rb", Call, 4, "helper_b"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("bin/s.rb", Call, 2, "helper_a"),
            ("run", Call, 6, "helper_a"),
        ],
    );
}

/// A file loaded in a cycle may still be loading when code it requires
/// runs: top-level code proves a function of a file it requires only when
/// that file does not require it back. Ruby 3.3: `ruby lib/b.rb` raises
/// `NameError` for `fa` (lib/a.rb, which it requires, requires it first);
/// `ruby lib/c.rb` calls `fd`.
#[test]
fn c10_ruby_imports_a_file_required_in_a_cycle_proves_no_function_above_it() {
    // M1 (the review's `s01_circular`)
    let rows = index(&[
        ("lib/a.rb", "require_relative \"b\"\ndef fa; :a; end\n"),
        ("lib/b.rb", "require_relative \"a\"\nfa\n"),
        ("lib/c.rb", "require_relative \"d\"\nfd\n"),
        ("lib/d.rb", "def fd; :d; end\n"),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("lib/a.rb", Import, 1, "lib/b.rb"),
            ("lib/b.rb", Import, 1, "lib/a.rb"),
            ("lib/c.rb", Import, 1, "lib/d.rb"),
            ("lib/c.rb", Call, 2, "fd"),
        ])
    );
    assert_unresolved(&rows, &[("lib/b.rb", Call, 2, "fa")]);
}

/// A method defined once, under a condition, in another method's body or
/// in a block an iterator may run no time, may not exist when it is called:
/// lookup proves nothing for its name. Over a literal that is not empty an
/// iterator runs its block. Ruby 3.3: `K.new.run` is `[:base, :base,
/// :base_n, :base_o, :k_p]`.
#[test]
fn c10_ruby_shadowing_a_definition_that_may_not_run_proves_no_target() {
    // M2 (the review's `e03_cond_two_owners` and `x02_nested_def_unrun`)
    let rows = index(&[(
        "lib/k.rb",
        "class Base; def f; :base; end; def m; :base; end; def n; :base_n; end; def o; :base_o; end; def p; :base_p; end; end
class K < Base
  if ENV[\"X\"]
    def f; :k; end
  end
  def outer; def m; :k; end; end
  ENV.fetch(\"NAMES\", \"\").split.each { def o; :k_o; end }
  [1].each { def p; :k_p; end }
  def run; [f, m, n, o, p]; end
end
",
    )]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("K", Reference, 2, "Base"),
            ("K#run", Call, 9, "Base#n"),
            ("K#run", Call, 9, "K#p"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("K#run", Call, 9, "f"),
            ("K#run", Call, 9, "m"),
            ("K#run", Call, 9, "o"),
        ],
    );
}

/// An iterator over a literal runs its block to its end only with no
/// splat among the elements, which may be empty, and no jump or raise in
/// the block, which may leave it before the `def` (R5-09); past a
/// `break`, which may leave more than its block, the rest of the class
/// body may not run either, past a `next` it does (R8-04). Ruby 3.3:
/// `K.new.run` is `[:base, :base, :base, :base, :k, :k, :base, :k]`.
#[test]
fn c10_ruby_shadowing_an_iterator_that_may_stop_short_proves_no_target() {
    // R5-09
    let rows = index(&[(
        "lib/k.rb",
        "class Base; def a; :base; end; def b; :base; end; def c; :base; end; def d; :base; end; def e; :base; end; def f; :base; end; def g; :base; end; def h; :base; end; end
class K < Base
  -1.times do
    def a; :k; end
  end
  [*[]].each do
    def b; :k; end
  end
  00.times do
    def c; :k; end
  end
  [*nil].each do
    def d; :k; end
  end
  [
    # a comment is no element
    1,
  ].each do
    def f; :k; end
  end
  %w[x].each do
    def h; :k; end
  end
  def run; [a, b, c, d, e, f, g, h]; end
  [1, 2].each do |i|
    next if i == 1
    def e; :k; end
  end
  [1].each do
    break
    def g; :k; end
  end
end
",
    )]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("K", Reference, 2, "Base"),
            ("K#run", Call, 24, "K#f"),
            ("K#run", Call, 24, "K#h"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("K#run", Call, 24, "a"),
            ("K#run", Call, 24, "b"),
            ("K#run", Call, 24, "c"),
            ("K#run", Call, 24, "d"),
            ("K#run", Call, 24, "e"),
            ("K#run", Call, 24, "g"),
        ],
    );
}

/// A bare `it` in a block that declares no parameter and uses no numbered
/// one is the block's first argument in Ruby 3.4 and a call of `it` in
/// Ruby 3.3; which Ruby runs the file is unknown, so it never resolves to
/// a method named `it`. With a parameter declared, or outside a block, it
/// is a call in both. Ruby 3.3: `K.new.run` is `[[:method], [:method],
/// :method]`; Ruby 3.4 gives `[[1], [:method], :method]`.
#[test]
fn c10_ruby_shadowing_a_bare_it_in_a_block_may_be_its_parameter() {
    // Parser study: Ruby 3.4's `it`.
    let rows = index(&[(
        "lib/k.rb",
        "class K
  def it; :method; end
  def run
    a = [1].map { it }
    b = [1].map { |x| it }
    c = -> { it.to_s }
    [a, b, it]
  end
end
",
    )]);
    let calls: Vec<Row> = resolved(&rows)
        .into_iter()
        .filter(|row| row.1 == Call)
        .collect();
    assert_eq!(
        calls,
        vec![
            ("K#run".to_owned(), Call, 5, "K#it".to_owned()),
            ("K#run".to_owned(), Call, 7, "K#it".to_owned()),
        ]
    );
    let sites: Vec<Row> = unresolved(&rows)
        .into_iter()
        .filter(|row| row.3 == "it")
        .collect();
    assert_eq!(sites, vec![]);
}
