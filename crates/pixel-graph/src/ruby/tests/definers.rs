// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Methods defined other than by a plain `def` in their owner's body: on a
//! constant's singleton class (`def X.m`, `class << X`), in a concern's
//! blocks, through `send` or `self.`, by a class macro or a splat; and what
//! a lookup may not take for proof where the rules cannot name the method.

use std::rc::Rc;

use pixel_model::EdgeKind::{Call, Reference};

use super::{assert_unresolved, index, labelled, resolved};
use crate::ruby::Fact;

/// `def Outer.s1` and `def s2` in `class << Outer` define methods of the
/// class the constant names, as `def self.s1` does; so does a top-level
/// `def Foo.helper`. `class << expr` may be the class itself: what it
/// defines there is unknown. A constant another file's nesting may give
/// another class proves nothing either way. Ruby 3.3: `Outer.go` is
/// `[:outer_s1, :outer_s2, :expr]`, `Foo.go` `:top_def`, and with
/// lib/app.rb loaded first `App::Config.reload` is `:x`.
#[test]
fn c10_ruby_dynamic_dispatch_a_def_on_a_constant_defines_on_its_class() {
    // B2
    let rows = index(&[
        (
            "lib/a.rb",
            "class Base; def self.s1; :bs1; end; def self.s2; :bs2; end; def self.s3; :bs3; end; def self.helper; :base; end; end
class Outer < Base
  def Outer.s1; :outer_s1; end
  class << Outer
    def s2; :outer_s2; end
  end
  class << (true ? self : Other)
    def s3; :expr; end
  end
  def self.go; [s1, s2, s3]; end
end
class Foo < Base; end
def Foo.helper; :top_def; end
class Foo; def self.go; helper; end; end
",
        ),
        (
            "lib/app.rb",
            "module App
  class Config; def self.load; :app; end; end
end
class Config; def self.reload; :top; end; end
",
        ),
        ("lib/x.rb", "module App\n  def Config.reload; :x; end\nend\n"),
        (
            "lib/c.rb",
            "class C; def r; [Outer.s1, Outer.s2, Outer.s3, Foo.helper, Foo.go, App::Config.reload, Config.reload]; end; end\n",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Outer", Reference, 2, "Base"),
            ("Outer.go", Call, 10, "Outer.s1"),
            ("Outer.go", Call, 10, "Outer.s2"),
            ("Foo", Reference, 12, "Base"),
            ("Foo.go", Call, 14, "Foo.helper"),
            ("C#r", Call, 1, "Outer.s1"),
            ("C#r", Call, 1, "Outer.s2"),
            ("C#r", Call, 1, "Foo.helper"),
            ("C#r", Call, 1, "Foo.go"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("Outer.go", Call, 10, "s3"),
            ("C#r", Call, 1, "Outer.s3"),
            ("C#r", Call, 1, "App::Config.reload"),
            ("C#r", Call, 1, "Config.reload"),
        ],
    );
}

/// A definition on a constant's class keeps, of the nesting it is read
/// in, the innermost scopes that fit in 256 bytes (R10-03): each kept the
/// whole nesting, which cost the square of its depth. Past them, the
/// constant may name any class or module of the generation holding the
/// path. `Helper` twelve modules into `L0` is this file's `L0::Helper`,
/// which a constant named `Helper` in another namespace may then hide;
/// one module in, the nesting is kept whole. Ruby 3.3: `[:near, :deep]`.
#[test]
fn c10_ruby_dynamic_dispatch_a_def_on_a_constant_deep_in_modules_keeps_its_innermost_scopes() {
    // R10-03
    let mut source = String::from(
        "module L0
  class Helper; end
  module L1
    def Helper.near; :near; end
  end
",
    );
    for d in 1..12 {
        source.push_str(&format!("module L{d}\n"));
    }
    source.push_str("def Helper.deep; :deep; end\n");
    source.push_str(&"end\n".repeat(12));
    let caller = "class C; def r; [L0::Helper.near, L0::Helper.deep]; end; end\n";
    let rows = index(&[("lib/l0.rb", &source), ("lib/c.rb", caller)]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("C#r", Call, 1, "L0::Helper.near"),
            ("C#r", Call, 1, "L0::Helper.deep"),
        ])
    );
    let other = "module Other; class Helper; end; end\n";
    let rows = index(&[
        ("lib/l0.rb", &source),
        ("lib/c.rb", caller),
        ("lib/o.rb", other),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[("C#r", Call, 1, "L0::Helper.near")])
    );
    assert_unresolved(&rows, &[("C#r", Call, 1, "L0::Helper.deep")]);
}

/// An empty `class << Helper` records nothing, as main does; the
/// definitions on a constant in one nesting share what they keep of it,
/// written once in the file's facts (R11-02): each copied the whole
/// nesting, and 9,900 empty ones 49 modules deep with 95-character names
/// indexed in 1.5 s and 209 MB against main's 0.2 s and 22 MB. Ruby 3.3:
/// `[:a, :b]`.
#[test]
fn c10_ruby_dynamic_dispatch_an_empty_singleton_body_records_nothing() {
    // R11-02
    let source = "module Outer
  class Helper; end
  class << Helper; end
  class << Helper
    # nothing
  end
  def Helper.a; :a; end
  def Helper.b; :b; end
end
";
    let caller = "class C; def r; [Outer::Helper.a, Outer::Helper.b]; end; end\n";
    let rows = index(&[("lib/outer.rb", source), ("lib/c.rb", caller)]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("C#r", Call, 1, "Outer::Helper.a"),
            ("C#r", Call, 1, "Outer::Helper.b"),
        ])
    );
    let outer = rows.iter().find(|r| r.file.path == "lib/outer.rb");
    let facts = Fact::decode_all(&outer.expect("lib/outer.rb").facts);
    let definees: Vec<(&str, &Rc<[String]>)> = facts
        .iter()
        .filter_map(|fact| match fact {
            Fact::Definee { name, scope, .. } => Some((name.as_str(), scope)),
            _ => None,
        })
        .collect();
    let names: Vec<&str> = definees.iter().map(|(name, _)| *name).collect();
    assert_eq!(names, ["Outer::Helper.a", "Outer::Helper.b"]);
    assert_eq!(&definees[0].1[..], ["Outer".to_owned()]);
    assert!(Rc::ptr_eq(definees[0].1, definees[1].1));
}

/// A subclass or an includer reached through a constant whose value the
/// rules do not read (`B2 = Base`), or through an expression (`class
/// Child3 < pick`), may hold the method's class or module like any other:
/// a call on `self` there resolves only when its own methods cannot come
/// first (`Child3` may hold `Base4` too, but defines no `tag`). An
/// expression rooted outside the graph (`Struct.new`) makes no class of
/// it. Ruby 3.3: `Child.new.run` is `:child`, `K.new.run` `:k`,
/// `Child3.new.run` `:child`, `Base4.new.run` `:base`.
#[test]
fn c10_ruby_dynamic_dispatch_a_self_call_holds_for_a_subclass_the_rules_cannot_name() {
    // B5
    let rows = index(&[(
        "lib/base.rb",
        "class Base
  def hook; :base; end
  def run; hook; end
  def stable; :stable; end
  def walk; stable; end
end
B2 = Base
class Child < B2
  def hook; :child; end
end
module M
  def label; :m; end
  def run; label; end
end
Mixin = M
class K
  include Mixin
  def label; :k; end
end
class Base3
  def hook; :base; end
  def run; hook; end
end
def pick; Base3; end
class Child3 < pick
  def hook; :child; end
end
class Base4
  def tag; :base; end
  def run; tag; end
end
class Point < Struct.new(:x)
  def tag; :point; end
end
",
    )]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Base#walk", Call, 5, "Base#stable"),
            ("lib/base.rb", Call, 25, "pick"),
            ("Base4#run", Call, 30, "Base4#tag"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("Base#run", Call, 3, "hook"),
            ("M#run", Call, 13, "label"),
            ("Base3#run", Call, 22, "hook"),
        ],
    );
}

/// A class macro that mixes a module in runs that module's hooks on its
/// caller, which may act on the other side too (`extended` prepending to
/// the class, `included` extending it): a lookup through the caller proves
/// nothing on either side, whether the macro is the class's own method or
/// one of a module it extends (R7-08). Found by the R6 and R7 differential
/// fuzzers. Ruby 3.3: `K4.new.probe_c`, `K6.new.probe_c` and `K5.load` are
/// `[:m1, :m1, :cm]`.
#[test]
fn c10_ruby_dynamic_dispatch_a_macro_mixin_may_act_on_both_sides() {
    // B7, R7-08
    let rows = index(&[(
        "lib/k.rb",
        "module M1; def c; :m1; end; end
module M0
  def self.extended(base); base.prepend(M1); end
end
class K4
  def self.setup; extend M0; end
  def c; :k4; end
  def probe_c; c; end
end
K4.setup
module Setup; def setup!; extend M0; end; end
class K6
  extend Setup
  def c; :k6; end
  def probe_c; c; end
end
K6.setup!
module CM; def find; :cm; end; end
module N0; def self.included(base); base.extend(CM); end; end
module Setup2; def setup2!; include N0; end; end
class Base; def self.find; :base; end; end
class K5 < Base
  extend Setup2
  def self.load; find; end
end
K5.setup2!
",
    )]);
    assert_unresolved(
        &rows,
        &[
            ("K4#probe_c", Call, 8, "c"),
            ("K6#probe_c", Call, 15, "c"),
            ("K5.load", Call, 24, "find"),
        ],
    );
}

/// What the steps before one the rules cannot name hold is read from the
/// names their owners define and from the facts that touch them, never by
/// walking every such object for each call (R6-03): `Child`, behind `B2 =
/// Base`, undefines `stable`, `Other` defines `size`, and neither touches
/// `tag`. Ruby 3.3: `Child.new.walk` raises NoMethodError,
/// `Other.new.measure` is `:other`, `Child.new.label` `:tag`.
#[test]
fn c10_ruby_dynamic_dispatch_steps_before_an_opaque_one_are_read_by_name() {
    // R6-03
    let rows = index(&[(
        "lib/base.rb",
        "class Base
  def stable; :stable; end
  def walk; stable; end
  def size; :base; end
  def measure; size; end
  def tag; :tag; end
  def label; tag; end
end
B2 = Base
class Child < B2
  undef_method :stable
end
class Other < B2
  def size; :other; end
end
",
    )]);
    assert_eq!(
        resolved(&rows),
        labelled(&[("Base#label", Call, 7, "Base#tag")])
    );
    assert_unresolved(
        &rows,
        &[
            ("Base#walk", Call, 3, "stable"),
            ("Base#measure", Call, 5, "size"),
        ],
    );
}

/// `concerning :Topic do` makes the concern `Owner::Topic`, whose block
/// defines its methods and `ClassMethods`, and the owner includes it
/// there: the owner's own methods come first. A concern's `included do`
/// block runs on each includer, so what it defines is the includer's,
/// after what the includer defined above the `include` and before what
/// it defines below. Ruby 3.3 with Active Support 7.1.5: `K.new.run` is
/// `:tagging`, `K.go` `:tag_s`, `K2.new.run` `:own`; `L.new.run`
/// `:tag_included`, `L3.new.run` `[:tag_included, :tag_b]`, `L4.new.run`
/// `:k4_own`, `L4.go` `:act`, and `Tag.instance_methods(false)` is `[:b]`.
#[test]
fn c10_ruby_ancestors_a_concerns_blocks_define_on_the_class_that_runs_them() {
    // B6
    let rows = index(&[
        (
            "lib/k.rb",
            "class Base; def a; :base; end; def self.s; :base_s; end; end
class K < Base
  concerning :Tagging do
    def a; :tagging; end
    class_methods do
      def s; :tag_s; end
    end
  end
  def run; a; end
  def self.go; s; end
end
class K2 < Base
  def a; :own; end
  concerning :T2 do
    def a; :t2; end
  end
  def run; a; end
end
",
        ),
        (
            "lib/l.rb",
            "class Base6; def a; :base; end; def b; :base_b; end; end
module Tag
  extend ActiveSupport::Concern
  included do
    def a; :tag_included; end
  end
  def b; :tag_b; end
end
class L < Base6
  def a; :own; end
  include Tag
  def run; a; end
end
class L3 < Base6
  include Tag
  def run; [a, b]; end
end
module Tag2
  extend ActiveSupport::Concern
  included do
    def b; :tag2_inc; end
    define_singleton_method(:active) { :act }
  end
end
class L4 < Base6
  include Tag2
  def b; :k4_own; end
  def run; b; end
  def self.go; active; end
end
",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("K", Reference, 2, "Base"),
            ("K", Reference, 3, "K::Tagging"),
            ("K#run", Call, 9, "K::Tagging#a"),
            ("K.go", Call, 10, "K::Tagging::ClassMethods#s"),
            ("K2", Reference, 12, "Base"),
            ("K2", Reference, 14, "K2::T2"),
            ("K2#run", Call, 17, "K2#a"),
            ("L", Reference, 9, "Base6"),
            ("L", Reference, 11, "Tag"),
            ("L3", Reference, 14, "Base6"),
            ("L3", Reference, 15, "Tag"),
            ("L3#run", Call, 16, "Tag#b"),
            ("L4", Reference, 25, "Base6"),
            ("L4", Reference, 26, "Tag2"),
            ("L4#run", Call, 28, "L4#b"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("L#run", Call, 12, "a"),
            ("L3#run", Call, 16, "a"),
            ("L4.go", Call, 29, "active"),
        ],
    );
    let symbols: Vec<&str> = rows
        .iter()
        .flat_map(|r| &r.symbols)
        .map(|s| s.name.as_str())
        .collect();
    for absent in ["K#a", "K#s", "Tag#a", "Tag2#b", "Tag2.active"] {
        assert!(!symbols.contains(&absent), "{absent} is not a method there");
    }
}

/// `define_method`, `remove_method` and their family define or remove as
/// written whether sent with `send` or called on `self`. Ruby 3.3:
/// `K.new.run` is `[:k1, :k1, :k3, :k4]`, `K.go` `[:ks1, :ks2]`, and
/// `C.new.r` `[:b, :b]`.
#[test]
fn c10_ruby_dynamic_dispatch_a_definer_sent_or_called_on_self_defines_as_written() {
    // B7, B8
    let rows = index(&[(
        "lib/k.rb",
        "class Base; def h1; :b; end; def h2; :b; end; def h3; :b; end; def h4; :b; end; def self.s1; :bs; end; def self.s2; :bs; end; end
class K < Base
  define_method \"h1\" do :k1 end
  define_method(:h2, instance_method(:h1))
  send(:define_method, :h3) { :k3 }
  class << self
    define_method(:s1) { :ks1 }
  end
  define_singleton_method(:s2) { :ks2 }
  self.define_method(:h4) { :k4 }
  def run; [h1, h2, h3, h4]; end
  def self.go; [s1, s2]; end
end
class Base2; def t1; :b; end; def t2; :b; end; end
class K2 < Base2
  def t1; :k; end; def t2; :k; end
  send(:remove_method, :t1)
  self.remove_method(:t2)
end
class C; def r; [K2.new.t1, K2.new.t2]; end; end
",
    )]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("K", Reference, 2, "Base"),
            ("K#run", Call, 11, "K#h1"),
            ("K#run", Call, 11, "K#h2"),
            ("K#run", Call, 11, "K#h3"),
            ("K#run", Call, 11, "K#h4"),
            ("K.go", Call, 12, "K.s1"),
            ("K.go", Call, 12, "K.s2"),
            ("K2", Reference, 15, "Base2"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("C#r", Call, 20, "K2.new.t1"),
            ("C#r", Call, 20, "K2.new.t2"),
        ],
    );
}

/// A definer in a method body defines on whatever object calls the method
/// (a class macro: `Post.make_helper` and its subclasses, any class
/// extending `Macro`), and a splat may hold any name unless it reads a
/// literal array of names above it: a lookup through such a class proves
/// nothing for what they may define, on both sides for a mixin, whose
/// hooks may act on either (R7-08), `acts` itself included. Ruby 3.3:
/// `Post.new.show` is `[:post, :own]`, `Sub.new.run` `:post`, `Q.new.run`
/// `[:macro, :macro, :mixy]`, and `Dyn.new.run` reads `Dyn#title`.
#[test]
fn c10_ruby_dynamic_dispatch_a_class_macro_or_a_splat_proves_nothing_it_may_define() {
    // B7
    let rows = index(&[(
        "lib/post.rb",
        "class Base; def helper; :base; end; def title; :base; end; def x; :base; end; def y; :base; end; def z; :base; end; end
class Post < Base
  def self.make_helper
    define_method(:helper) { :post }
  end
  make_helper
  ATTRS = %i[title body]
  attr_accessor(*ATTRS)
  def initialize; @title = :own; end
  def show; [helper, title]; end
end
class Sub < Post
  def helper; :sub; end
  make_helper
  def run; helper; end
end
module Macro
  def acts; class_eval do def x; :macro; end end; class_eval \"def y; :macro; end\"; include Mixy; end
end
module Mixy; def z; :mixy; end; end
class Q < Base
  extend Macro
  acts
  def run; [x, y, z]; end
end
class Dyn < Base
  fields = %i[title]; attr_reader(*fields)
  def run; title; end
end
",
    )]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Post", Reference, 2, "Base"),
            ("Post", Call, 6, "Post.make_helper"),
            ("Sub", Reference, 12, "Post"),
            ("Sub", Call, 14, "Post.make_helper"),
            ("Q", Reference, 21, "Base"),
            ("Q", Reference, 22, "Macro"),
            ("Post#show", Call, 10, "Post#title"),
            ("Dyn", Reference, 26, "Base"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("Post#show", Call, 10, "helper"),
            ("Sub#run", Call, 15, "helper"),
            ("Dyn#run", Call, 28, "title"),
            ("Q", Call, 23, "acts"),
            ("Q#run", Call, 24, "x"),
            ("Q#run", Call, 24, "y"),
            ("Q#run", Call, 24, "z"),
        ],
    );
}

/// A literal array a constant holds is what a splat or an `each` loop
/// reads only while nothing may have changed it: `ATTRS << :y` (or any
/// read but a splat and a few calls that leave it as it is) leaves the
/// splat any name, and a loop over a literal array of constants includes
/// each in turn. A name with an interpolation (`"#{name}="`) is a pattern:
/// only a name that fits it may be what the macro defines. Ruby 3.3:
/// `[C.new.run, D.new.run, E.new.run, F.new.run]` is `[:b, nil, :b, [nil,
/// :base, 1]]`.
#[test]
fn c10_ruby_dynamic_dispatch_a_literal_array_is_read_only_while_unchanged() {
    // B7: a splat read an array a later `<<` had changed (stage 4 review).
    let rows = index(&[(
        "lib/a.rb",
        "module A; def who; :a; end; end
module B; def who; :b; end; end
class Base; def y; :base; end; def who; :base; end; def other; :base; end; end
class C < Base
  MODS = [A, B]
  MODS.each do |m|
    include m
  end
  def run; who; end
end
class D < Base
  ATTRS = [:x]
  ATTRS << :y
  attr_reader(*ATTRS)
  def run; y; end
end
class E < Base
  def self.helper(list); list; end
  LIST = [A, B]
  helper(LIST)
  LIST.each { |m| include m }
  def run; who; end
end
class F < Base
  def self.flag(name)
    define_method(\"#{name}=\") { |v| v }
  end
  flag :on
  attr_accessor :pos
  def run; [pos, other, self.pos = 1]; end
end
",
    )]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("C", Reference, 4, "Base"),
            ("C", Reference, 7, "A"),
            ("C", Reference, 7, "B"),
            ("C#run", Call, 9, "B#who"),
            ("D", Reference, 11, "Base"),
            ("E", Reference, 17, "Base"),
            ("E", Call, 20, "E.helper"),
            ("F", Reference, 24, "Base"),
            ("F", Call, 28, "F.flag"),
            ("F#run", Call, 30, "F#pos"),
            ("F#run", Call, 30, "Base#other"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("D#run", Call, 15, "y"),
            ("E#run", Call, 22, "who"),
            ("F#run", Call, 30, "self.pos="),
        ],
    );
}

/// The `alias` keyword in a method body or in `class << K2` may define
/// its new name only: the keyword's own token, of the same node kind, is
/// not another `alias` whose name the rules cannot read. Ruby 3.3:
/// `[K.new.run, K2.run]` is `[[:base_b, :base_c], [:y, :x]]`, and `b`
/// is `K#a` once `setup` has run.
#[test]
fn c10_ruby_dynamic_dispatch_an_alias_keyword_may_define_its_new_name_only() {
    // B7: the keyword token read as an alias of any name (stage 4).
    let rows = index(&[(
        "lib/k.rb",
        "class Base; def a; :base_a; end; def b; :base_b; end; def c; :base_c; end; end
class K < Base
  def a; :k_a; end
  def setup; alias b a; end
  def run; [b, c]; end
end
class K2
  def self.x; :x; end
  def self.y; :y; end
  class << K2
    alias z x
  end
  def self.run; [y, z]; end
end
",
    )]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("K", Reference, 2, "Base"),
            ("K#run", Call, 5, "Base#c"),
            ("K2.run", Call, 13, "K2.y"),
        ])
    );
    assert_unresolved(&rows, &[("K#run", Call, 5, "b"), ("K2.run", Call, 13, "z")]);
}

/// A string a binding evaluates runs on a `self` the rules cannot name,
/// so a method it defines may be any object's, a subclass's override of
/// `Bank#kind` among them: `Bank#to_hash` proves nothing of `kind`. A
/// Redis script (`$redis.eval(<<~LUA)`, gumroad's offer-code job) defines
/// nothing on any `self`, so it changes no class, and the self call keeps
/// its edge (#111). Ruby 3.3: with `$r = binding`, `Sub.new.to_hash` is
/// `:sub` once the string defining `Sub#kind` ran; a string of calls only
/// leaves it `:bank`.
#[test]
fn c10_ruby_an_evaluated_string_that_defines_nothing_changes_no_class() {
    // #111, R7-03
    let bank = "class Bank
  def to_hash; kind; end
  def kind; :bank; end
end
";
    let script = "class Job
  def perform(id)
    $redis.eval(<<~LUA, keys: [\"lock:#{id}\"])
      if redis.call('GET', KEYS[1]) == ARGV[1] then
        return redis.call('DEL', KEYS[1])
      end
    LUA
  end
end
";
    let rows = index(&[("lib/bank.rb", bank), ("lib/job.rb", script)]);
    assert!(resolved(&rows).is_superset(&labelled(&[("Bank#to_hash", Call, 2, "Bank#kind")])));
    let defining = "class Job
  def perform
    $r.eval(\"class Sub < Bank; def kind; :sub; end; end\")
  end
end
";
    let rows = index(&[("lib/bank.rb", bank), ("lib/job.rb", defining)]);
    assert_unresolved(&rows, &[("Bank#to_hash", Call, 2, "kind")]);
}

const HELPER: &str = "module Helper
  def a; b; end
  def b; end
end
class Report
  include Helper
end
";

/// A module `Module.new` builds where it is mixed in is none of the
/// graph's: prepending it to a gem's singleton class (gumroad's
/// `Alterity.singleton_class.prepend(Module.new do … end)`) puts no
/// module of the graph anywhere, so `Helper#a -> Helper#b` stays proven;
/// a module a method returns may be `Helper` itself, mixed in where no
/// lookup replays it, so the same call proves nothing (R8-05, #111).
#[test]
fn c10_ruby_a_mixin_of_a_new_module_leaves_the_graphs_modules_alone() {
    // #111, R8-05
    let fresh = "Alterity.singleton_class.prepend(Module.new do
  def execute_alter(t); Helper; end
end)
";
    let rows = index(&[("lib/helper.rb", HELPER), ("config/alterity.rb", fresh)]);
    assert!(resolved(&rows).is_superset(&labelled(&[("Helper#a", Call, 2, "Helper#b")])));
    let rows = index(&[
        ("lib/helper.rb", HELPER),
        (
            "config/alterity.rb",
            "Alterity.singleton_class.prepend(Cfg.mod)\n",
        ),
    ]);
    assert_unresolved(&rows, &[("Helper#a", Call, 2, "b")]);
}

/// Code a string evaluates on an object the rules cannot name mixes what
/// it names into that object: written in a spec (`@klass.class_eval
/// "include Helper"`, gumroad's search-index spec), a stub only a test's
/// callers may meet, so the application's `Helper#a -> Helper#b` stays
/// proven; written in the application, any object may hold `Helper`
/// first, and the call proves nothing (R5-04, R6-14, #106, #111).
#[test]
fn c10_ruby_a_string_mixin_on_an_unnamed_object_is_a_stub_in_a_test() {
    // #111, #106
    let eval =
        "class Setup\n  def run(klass)\n    klass.class_eval \"include Helper\"\n  end\nend\n";
    let rows = index(&[("lib/helper.rb", HELPER), ("spec/support/setup.rb", eval)]);
    assert!(resolved(&rows).is_superset(&labelled(&[("Helper#a", Call, 2, "Helper#b")])));
    let rows = index(&[("lib/helper.rb", HELPER), ("lib/setup.rb", eval)]);
    assert_unresolved(&rows, &[("Helper#a", Call, 2, "b")]);
}
