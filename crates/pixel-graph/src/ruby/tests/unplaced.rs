// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! What the rules cannot place exactly, and the blunt rules that keep it
//! from proving a target (C-10, R5): a module mixed in from outside a body
//! or under a condition, a method defined on an object the rules cannot
//! name, a module a superclass the graph cannot name may already hold, a
//! hook defined other than by `def`, a block passed on to a method that
//! rebinds `self`.

use pixel_model::EdgeKind::{Call, Reference};

use super::{assert_unresolved, index, labelled, resolved};

/// A module mixed into a class from outside its body runs its methods on
/// that class's objects whichever the verb, `prepend`, `include`,
/// `extend` or `singleton_class.include`, and so does every module it
/// brings along: a call on `self` in them proves nothing, a subclass's
/// own method may come first (R5-05). Ruby 3.3: `[K3.new.probe,
/// K5.probe, K3.new.deep]` is `[:k3, :k5s, :k3]`.
#[test]
fn c10_ruby_dynamic_dispatch_a_mixin_from_outside_the_body_proves_nothing_whatever_its_verb() {
    // R5-05
    let rows = index(&[(
        "lib/k.rb",
        "module P1; def c; :p1; end; def probe; c; end; end
module I1; def c; :i1; end; def probe; c; end; end
module E1; def c; :e1; end; def probe; c; end; end
module S1; def c; :s1; end; def probe; c; end; end
module N1; def c; :n1; end; def deep; c; end; end
module W1; include N1; end
class K2; end
class K3 < K2; def c; :k3; end; def self.c; :k3s; end; end
class K5 < K2; def self.c; :k5s; end; end
K2.prepend(P1)
K2.include(I1)
K2.extend(E1)
K2.singleton_class.include(S1)
K2.send(:prepend, W1)
",
    )]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("W1", Reference, 6, "N1"),
            ("K3", Reference, 8, "K2"),
            ("K5", Reference, 9, "K2"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("P1#probe", Call, 1, "c"),
            ("I1#probe", Call, 2, "c"),
            ("E1#probe", Call, 3, "c"),
            ("S1#probe", Call, 4, "c"),
            ("N1#deep", Call, 5, "c"),
        ],
    );
}

/// The same from a block evaluated on the class (`class_eval`,
/// `instance_eval`, `class_exec` passing the module), from a class macro
/// run on it or on a subclass, through a splat or an `each` over a literal
/// array, and at the top level, where `include` mixes into every object
/// (R6-06). Ruby 3.3: each module's `probe` run on `K3.new` (`C1`'s and
/// `Z1`'s on `K3`) is `:k3` (`:k3s`).
#[test]
fn c10_ruby_dynamic_dispatch_a_mixin_from_a_block_or_a_macro_proves_nothing() {
    // R6-06
    let rows = index(&[(
        "lib/k.rb",
        "module B1; def c; :b1; end; def probe; c; end; end
module C1; def c; :c1; end; def probe; c; end; end
module D1; def c; :d1; end; def probe; c; end; end
module E1; def c; :e1; end; def probe; c; end; end
module T1; def c; :t1; end; def probe; c; end; end
module Z1; def c; :z1; end; def probe; c; end; end
module L1; def c; :l1; end; def probe; c; end; end
module G1; def c; :g1; end; def probe; c; end; end
class K2; def self.setup!; prepend D1; end; def self.setup2!; extend Z1; end; end
class K3 < K2; def c; :k3; end; def self.c; :k3s; end; end
K2.class_eval { prepend B1 }
K2.instance_eval { extend C1 }
K2.setup!
K2.include(*[E1])
K2.class_exec(T1) { |m| prepend m }
K3.setup2!
K2.class_eval { [L1].each { |m| include m } }
include G1
",
    )]);
    assert_unresolved(
        &rows,
        &[
            ("B1#probe", Call, 1, "c"),
            ("C1#probe", Call, 2, "c"),
            ("D1#probe", Call, 3, "c"),
            ("E1#probe", Call, 4, "c"),
            ("T1#probe", Call, 5, "c"),
            ("Z1#probe", Call, 6, "c"),
            ("L1#probe", Call, 7, "c"),
            ("G1#probe", Call, 8, "c"),
        ],
    );
}

/// A module mixed in from outside its body through an expression (`(P1)`,
/// `c ? nil : P2`, `P3.itself`) may be any constant the expression
/// writes, and any module of the graph besides (R8-05); one mixed in
/// through a constant whose value the rules do not read (`PA = P4`, then
/// `K2.prepend(PA)`, sent, from a block, an object's singleton class or a
/// hook's unread statement) may be any module of the graph: a call on
/// `self` in it proves nothing (R7-05). Ruby 3.3: each `probe` run on
/// `K3.new` is `:k3`, `K4.new.mprobe` `:k4`.
#[test]
fn c10_ruby_dynamic_dispatch_a_mixin_through_an_expression_or_an_alias_proves_nothing() {
    // R7-05
    let program = |outside: &str| {
        index(&[(
            "lib/k.rb",
            &format!(
                "module P1; def c; :p1; end; def probe; c; end; end
module P2; def c; :p2; end; def probe; c; end; end
module P3; def c; :p3; end; def probe; c; end; end
module P4; def c; :p4; end; def probe; c; end; end
class K2; end
class K3 < K2; def c; :k3; end; end
{outside}
"
            ),
        )])
    };
    let rows =
        program("K2.include((P1))\nK2.include(ENV[\"NOPE\"] ? nil : P2)\nK2.include(P3.itself)");
    assert_unresolved(
        &rows,
        &[
            ("P1#probe", Call, 1, "c"),
            ("P2#probe", Call, 2, "c"),
            ("P3#probe", Call, 3, "c"),
        ],
    );
    assert_unresolved(&rows, &[("P4#probe", Call, 4, "c")]);
    for outside in [
        "PA = P4\nK2.prepend(PA)",
        "PA = P4\nK2.send(:prepend, PA)",
        "PA = P4\nK2.class_eval { prepend PA }",
        "module Cfg; PA = P4; end\nK2.prepend(*[Cfg::PA])",
        "PA = P4\nx = K3.new\nx.singleton_class.prepend(PA)",
    ] {
        assert_unresolved(&program(outside), &[("P4#probe", Call, 4, "c")]);
    }
    let rows = index(&[(
        "lib/k.rb",
        "module M3; def c; :m3; end; def mprobe; c; end; end
module M2
  def self.included(base)
    base.include(self::Inner) if base.is_a?(Class)
  end
  Inner = M3
end
class K4
  include M2
  def c; :k4; end
end
",
    )]);
    assert_unresolved(&rows, &[("M3#mprobe", Call, 1, "c")]);
}

/// A mixin from outside the body of a class whose argument is no constant
/// path (a method's result, `const_get` of a computed name, a splat, sent,
/// in a `class_eval` block or in `class << K2`) is an unknown step on that
/// chain, which may be any module of the graph: a call on `self` in any
/// module proves nothing for an object whose chain holds it, but a
/// module's own methods run on the module alone; on a gem's class, any
/// module of the graph may be held (R8-05). Ruby 3.3: `[(K3.new.probe
/// rescue :none), (K3.probe rescue :none)]` holds `:k3` for each instance
/// form and `:k3s` for each singleton one, and `P1.label` is `:p1`.
#[test]
fn c10_ruby_dynamic_dispatch_a_mixin_of_an_expression_from_outside_may_be_any_module() {
    // R8-05
    for outside in [
        "K2.prepend(Cfg.mod)",
        "name = \"P1\"\nK2.prepend(Object.const_get(name))",
        "n = 1\nK2.prepend(Object.const_get(\"P#{n}\"))",
        "K2.prepend(Object.const_get(:\"P#{1}\"))",
        "K2.send(:prepend, Cfg.mod)",
        "K2.class_eval { prepend Cfg.mod }",
        "K2.include(*[Cfg.mod])",
        "K2.singleton_class.include(Cfg.mod)",
        "K2.extend(Cfg.mod)",
        "class << K2; include Cfg.mod; end",
    ] {
        let rows = index(&[(
            "lib/k.rb",
            &format!(
                "module P1; def c; :p1; end; def probe; c; end; def self.label; tag; end; def self.tag; :p1; end; end
module Cfg; def self.mod = P1; end
class K2; end
class K3 < K2; def c; :k3; end; def self.c; :k3s; end; end
{outside}
"
            ),
        )]);
        assert_unresolved(&rows, &[("P1#probe", Call, 1, "c")]);
        assert!(
            resolved(&rows).contains(&("P1.label".to_owned(), Call, 1, "P1.tag".to_owned())),
            "{outside}"
        );
    }
    // On a gem's class, whose objects the graph holds behind a step it
    // cannot name, the module may be any of the graph's.
    let rows = index(&[(
        "lib/k.rb",
        "module P1; def c; :p1; end; def probe; c; end; end
module Cfg; def self.mod = P1; end
GemBase.prepend(Cfg.mod)
class K3 < GemBase; def c; :k3; end; end
",
    )]);
    assert_unresolved(&rows, &[("P1#probe", Call, 1, "c")]);
}

/// A module mixed into one object runs its hooks there: what a hook
/// statement the rules do not read mixes in (under a condition, in the
/// module's own `included` or in one of a module it extends) may land on
/// that object too, so its names prove nothing (R7-09). Ruby 3.3:
/// `[K2.new.probe_b, o.probe_b]` is `[:k2, :m2]` with each.
#[test]
fn c10_ruby_dynamic_dispatch_a_hook_run_for_one_object_spreads_what_it_mixes_in() {
    // R7-09
    for hook in [
        "module M1; def self.included(base); base.include(M2) if base.is_a?(Class); end; end",
        "module Hooky; def included(base); base.include(M2) if base.is_a?(Class); end; end
module M1; extend Hooky; end",
    ] {
        let rows = index(&[(
            "lib/k.rb",
            &format!(
                "module M2; def b; :m2; end; end
{hook}
class K2; def b; :k2; end; def probe_b; b; end; end
o = K2.new
o.singleton_class.include(M1)
"
            ),
        )]);
        let line = 2 + hook.lines().count() as u32;
        assert_unresolved(&rows, &[("K2#probe_b", Call, line, "b")]);
    }
}

/// A hook statement the rules do not read (a mixin under a condition, in
/// parentheses, a string `module_eval`), and code a string evaluates
/// (`class_eval("include M6")`, `eval`), may mix in any module it names:
/// that module's self calls prove nothing (R6-08, R6-14). An unknown step
/// such code leaves before a superclass the rules cannot name may hold a
/// module defining the name (R6-15). Ruby 3.3: each module's `mprobe` run
/// on `K4.new` is `:k4`, `K3.sprobe` is `:n3`.
#[test]
fn c10_ruby_dynamic_dispatch_a_mixin_the_rules_do_not_read_proves_nothing() {
    // R6-08, R6-14, R6-15
    let rows = index(&[(
        "lib/k.rb",
        "module M3; def c; :m3; end; def mprobe; c; end; end
module M5; def c; :m5; end; def mprobe; c; end; end
module M6; def c; :m6; end; def mprobe; c; end; end
module M7; def c; :m7; end; def mprobe; c; end; end
module M2
  def self.included(base); base.include(M3) if base.is_a?(Class); end
end
module M4
  def self.extended(base) = (base.include(M5))
end
class K4
  include M2
  extend M4
  class_eval(\"include M6\")
  eval(\"include M7\")
  def c; :k4; end
end
module N3; def c; :n3; end; end
class K1
  def self.c; :k1; end
  def self.sprobe; c; end
end
module N1; def self.included(base); base.singleton_class.prepend(N3); end; end
class K3 < (K1)
  include N1
end
",
    )]);
    assert_unresolved(
        &rows,
        &[
            ("M3#mprobe", Call, 1, "c"),
            ("M5#mprobe", Call, 2, "c"),
            ("M6#mprobe", Call, 3, "c"),
            ("M7#mprobe", Call, 4, "c"),
            ("K1.sprobe", Call, 21, "c"),
        ],
    );
}

/// A mixin under a condition, a loop or a block that may run no time may
/// not run: what it would insert may not be there, so a lookup that lands
/// in it, or a constant read through it, proves nothing; the `each` of a
/// constant holding modules runs once per module (R5-07). Ruby 3.3, with
/// neither variable set: `[K.new.run, K.go, K.new.kept, K.new.retry_it,
/// J.new.run, L.new.run]` is `[:base, :base, :quiet, :top, :j, :base]`.
#[test]
fn c10_ruby_ancestors_a_mixin_that_may_not_run_proves_nothing() {
    // R5-07
    let rows = index(&[(
        "lib/k.rb",
        "class Base; def save; :base; end; def self.find; :base; end; def other; :base; end; end
module Loud; def save; :loud; end; class Retry; def self.go; :loud; end; end; end
module Finder; def find; :finder; end; end
module Quiet; def other; :quiet; end; end
class Retry; def self.go; :top; end; end
class K < Base
  include Loud if ENV[\"LOUD\"]
  extend Finder unless ENV[\"PLAIN\"].nil?
  MODS = [Quiet]
  MODS.each { |m| include m }
  def run; save; end
  def self.go; find; end
  def kept; other; end
  def retry_it; Retry.go; end
end
class J < Base
  prepend Loud if false
  def save; :j; end
  def run; save; end
end
class L < Base
  if ENV[\"LOUD\"]
    include Loud
  end
  def run; save; end
end
",
    )]);
    let calls: Vec<_> = resolved(&rows)
        .into_iter()
        .filter(|row| row.1 == Call)
        .collect();
    assert_eq!(
        calls,
        labelled(&[("K#kept", Call, 13, "Quiet#other")])
            .into_iter()
            .collect::<Vec<_>>()
    );
    assert_unresolved(
        &rows,
        &[
            ("K#run", Call, 11, "save"),
            ("K.go", Call, 12, "find"),
            ("K#retry_it", Call, 14, "Retry.go"),
            ("J#run", Call, 19, "save"),
            ("L#run", Call, 25, "save"),
        ],
    );
}

/// What a class written under a condition declares, what a `rescue`
/// guards (a call there may raise and skip the rest), and what follows a
/// top-level `return` may not run either: none of it proves a target
/// (R6-07, R6-09, R6-10). Ruby 3.3, with neither variable set:
/// `K.new.run` is `[:base, :base, :base, :base, :base, :base, :k]`.
#[test]
fn c10_ruby_ancestors_a_body_that_may_not_run_proves_nothing() {
    // R6-07, R6-09, R6-10
    let rows = index(&[
        (
            "lib/base.rb",
            "module Loud; def save; :loud; end; end
module Quiet; def other; :quiet; end; end
class Base
  def save; :base; end; def name; :base; end; def g; :base; end
  def h; :base; end; def tag; :base; end; def other; :base; end
end
class K < Base
  def kept; :k; end
  def run; [save, name, g, h, tag, other, kept]; end
end
if ENV[\"NOPE_X\"]
  class K
    include Loud
    def name; :k; end
  end
end
class K
  begin
    Integer(\"x\")
    def g; :k; end
  rescue ArgumentError
  end
  [1].each do
    Integer(\"x\")
    def h; :k; end
  rescue ArgumentError
  end
end
",
        ),
        (
            "lib/ext.rb",
            "require_relative \"base\"
return unless ENV[\"NOPE_Y\"]
class K
  prepend Quiet
  def tag; :k; end
end
",
        ),
    ]);
    let calls: std::collections::BTreeSet<_> = resolved(&rows)
        .into_iter()
        .filter(|row| row.0 == "K#run")
        .collect();
    assert_eq!(calls, labelled(&[("K#run", Call, 9, "K#kept")]));
    assert_unresolved(
        &rows,
        &[
            ("K#run", Call, 9, "save"),
            ("K#run", Call, 9, "name"),
            ("K#run", Call, 9, "g"),
            ("K#run", Call, 9, "h"),
            ("K#run", Call, 9, "tag"),
            ("K#run", Call, 9, "other"),
        ],
    );
}

/// What a block Ruby runs once where it is written (`class_eval do`,
/// `instance_eval do`, `Class.new do`) holds past a statement that may
/// `next` or `break` out of it may not run either (R7-04), and neither
/// may what follows that statement in the body around the block (R8-04).
/// Ruby 3.3, with the variable unset: `[K.new.run, J.new.run]` is
/// `[[:base, :base], [:base, :base]]`, and `Kc.new.probe` raises
/// `NoMethodError`.
#[test]
fn c10_ruby_ancestors_a_block_past_a_next_may_not_run() {
    // R7-04
    let rows = index(&[
        (
            "lib/base.rb",
            "module Loud; def save; :loud; end; end
class Base; def save; :base; end; def name; :base; end; end
",
        ),
        (
            "lib/k.rb",
            "class K < Base
  def run; [save, name]; end
  class_eval do
    next unless ENV[\"NOPE_X\"]
    include Loud
    def name; :k; end
  end
end
",
        ),
        (
            "lib/j.rb",
            "class J < Base
  def run; [save, name]; end
  instance_eval { break unless ENV[\"NOPE_X\"]; include Loud }
end
",
        ),
        (
            "lib/kc.rb",
            "Kc = Class.new(Base) do
  def probe; c; end
  [1, 2].each { |i| next if i == 1 }
  next unless ENV[\"NOPE_X\"]
  def c; :kc; end
end
",
        ),
    ]);
    let calls: std::collections::BTreeSet<_> = resolved(&rows)
        .into_iter()
        .filter(|row| row.1 == Call)
        .collect();
    assert_eq!(calls, labelled(&[("J#run", Call, 2, "Base#name")]));
    assert_unresolved(
        &rows,
        &[
            ("K#run", Call, 2, "save"),
            ("K#run", Call, 2, "name"),
            ("J#run", Call, 2, "save"),
            ("Kc#probe", Call, 2, "c"),
        ],
    );
}

/// A statement that may leave early (`next`, `break`, `return`, `raise`,
/// `exit` and their family), at any depth but in a method's body, may
/// leave the body Ruby runs once around it before what follows: a
/// concern's `included do` or `prepended do`, a `class_eval` block, the
/// file, wrapped in `begin` or in parentheses or not; a `next` leaves its
/// block alone (R8-04). Ruby 3.3 with Active Support 7.1 and the variable
/// unset: `K1.new.run` is `[:base, :base]`, `K2.new.run` `:k`, `K3` to
/// `K6` give `[:base, :base]`, and `Kept.new.run` `:kept`.
#[test]
fn c10_ruby_ancestors_what_follows_an_early_exit_at_any_depth_may_not_run() {
    // R8-04
    let unset = "ENV[\"NOPE_X\"]";
    let rows = index(&[
        (
            "lib/base.rb",
            "module Loud; def save; :loud; end; end
class Base; def save; :base; end; def name; :base; end; end
",
        ),
        (
            "lib/concerns.rb",
            &format!(
                "module Feature
  extend ActiveSupport::Concern
  included do
    next unless {unset}
    include Loud
  end
end
module Front
  extend ActiveSupport::Concern
  prepended do
    next unless {unset}
    prepend Loud
  end
end
class Kept < Base
  def name; :kept; end
  def run; name; end
end
"
            ),
        ),
        (
            "lib/k.rb",
            &format!(
                "class K1 < Base
  include Feature
  def run; [save, name]; end
end
class K2 < Base
  prepend Front
  def save; :k; end
  def run; save; end
end
class K3 < Base
  def run; [save, name]; end
  class_eval do
    begin
      next unless {unset}
      include Loud
      def name; :k; end
    end
  end
end
class K4 < Base
  def run; [save, name]; end
  class_eval do
    (next unless {unset}; include Loud; def name; :k; end)
  end
end
class K5 < Base
  def run; [save, name]; end
end
class K6 < Base
  def run; [save, name]; end
end
"
            ),
        ),
        (
            "lib/ext5.rb",
            &format!(
                "begin
  return unless {unset}
  class K5
    include Loud
    def name; :k; end
  end
end
"
            ),
        ),
        (
            "lib/ext6.rb",
            &format!("(return unless {unset}; class K6; include Loud; def name; :k; end; end)\n"),
        ),
    ]);
    let calls: std::collections::BTreeSet<_> = resolved(&rows)
        .into_iter()
        .filter(|row| row.1 == Call)
        .collect();
    assert_eq!(
        calls,
        labelled(&[
            ("K1#run", Call, 3, "Base#name"),
            ("Kept#run", Call, 17, "Kept#name"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("K1#run", Call, 3, "save"),
            ("K2#run", Call, 8, "save"),
            ("K3#run", Call, 11, "save"),
            ("K3#run", Call, 11, "name"),
            ("K4#run", Call, 21, "save"),
            ("K4#run", Call, 21, "name"),
            ("K5#run", Call, 27, "save"),
            ("K5#run", Call, 27, "name"),
            ("K6#run", Call, 30, "save"),
            ("K6#run", Call, 30, "name"),
        ],
    );
}

/// A hook defined on a module's singleton other than by a `def self.` in
/// its body (in `class << self` or `class << M1`, as `def M1.included`,
/// by `define_singleton_method`, in `M1.instance_eval`) is one the rules
/// do not read: mixed into one object, its module may bring any module
/// there (R8-06). Ruby 3.3: `[K2.new.probe_b, o.probe_b]` is `[:k2, :m2]`
/// with each.
#[test]
fn c10_ruby_dynamic_dispatch_a_hook_defined_on_a_singleton_in_any_way_is_unknown() {
    // R8-06
    let statement = "base.include(M2) if base.is_a?(Class)";
    for hook in [
        format!("module M1; class << self; def included(base); {statement}; end; end; end"),
        format!("module M1; end\ndef M1.included(base); {statement}; end"),
        format!("module M1; define_singleton_method(:included) {{ |base| {statement} }}; end"),
        format!("module M1; end\nM1.define_singleton_method(:included) {{ |base| {statement} }}"),
        format!("module M1; end\nM1.instance_eval {{ def included(base); {statement}; end }}"),
        format!("module M1; end\nclass << M1; def included(base); {statement}; end; end"),
    ] {
        let rows = index(&[(
            "lib/k.rb",
            &format!(
                "module M2; def b; :m2; end; end
{hook}
class K2; def b; :k2; end; def probe_b; b; end; end
o = K2.new
o.singleton_class.include(M1)
"
            ),
        )]);
        let line = 2 + hook.lines().count() as u32;
        assert_unresolved(&rows, &[("K2#probe_b", Call, line, "b")]);
    }
}

/// `method_added`, defined on a class's singleton in any way, or in a
/// module the class extends, runs on each method defined in the class or
/// a subclass, where it may remove or redefine any name: a lookup through
/// them proves nothing (R8-06). Ruby 3.3, with `Hooks.fire(k, m)`
/// removing `name`: `[K.new.run, J.new.go]` is `[:base, :base]` with each.
#[test]
fn c10_ruby_dynamic_dispatch_a_method_added_hook_may_change_any_name() {
    // R8-06
    for hook in [
        "class K < Base\n  def self.method_added(m); Hooks.fire(self, m); end",
        "class K < Base\n  class << self; def method_added(m); Hooks.fire(self, m); end; end",
        "class K < Base\n  define_singleton_method(:method_added) { |m| Hooks.fire(self, m) }",
        "module Hooky; def method_added(m); Hooks.fire(self, m); end; end\nclass K < Base\n  extend Hooky",
        "class K < Base; end\nK.define_singleton_method(:method_added) { |m| Hooks.fire(self, m) }\nclass K",
        "class K < Base; end\ndef K.method_added(m); Hooks.fire(self, m); end\nclass K",
    ] {
        let rows = index(&[(
            "lib/k.rb",
            &format!(
                "class Base; def name; :base; end; end
{hook}
  def name; :k; end
  def run; name; end
end
class J < K
  def name; :j; end
  def go; name; end
end
"
            ),
        )]);
        let line = 3 + hook.lines().count() as u32;
        assert_unresolved(
            &rows,
            &[
                ("K#run", Call, line, "name"),
                ("J#go", Call, line + 4, "name"),
            ],
        );
    }
}

/// A method defined on one object (`def o.ha`, `define_singleton_method`,
/// its singleton class's `class_eval` or `class <<`, an `extend`), or in a
/// new anonymous subclass, runs before its class's own for that object:
/// no lookup of its name proves anything, whatever the receiver, while a
/// name no such code defines still resolves (R5-04). Ruby 3.3: `o.run` is
/// `[:obj, :obj, :obj, :obj, :obj, :base, :base, :obj, :base]`, `a.run`
/// and `b.run` give `:anon` for `hf` and `hg`.
#[test]
fn c10_ruby_dynamic_dispatch_a_method_defined_on_one_object_proves_nothing() {
    // R5-04
    let rows = index(&[(
        "lib/k.rb",
        "class Base
  def run; [ha, hb, hc, hd, he, hf, hg, hh, kept]; end
  def ha; :base; end
  def hb; :base; end
  def hc; :base; end
  def hd; :base; end
  def he; :base; end
  def hf; :base; end
  def hg; :base; end
  def hh; :base; end
  def kept; :base; end
end
module Over; def hd; :obj; end; end
o = Base.new
def o.ha; :obj; end
o.define_singleton_method(:hb) { :obj }
o.singleton_class.class_eval { def hc; :obj; end }
o.extend(Over)
class << o; def he; :obj; end; end
o.singleton_class.send(:define_method, :hh) { :obj }
a = Class.new(Base) { def hf; :anon; end }.new
b = Class.new(Base) { define_method(:hg) { :anon } }.new
",
    )]);
    let calls: Vec<_> = resolved(&rows)
        .into_iter()
        .filter(|row| row.1 == Call)
        .collect();
    assert_eq!(
        calls,
        labelled(&[("Base#run", Call, 2, "Base#kept")])
            .into_iter()
            .collect::<Vec<_>>()
    );
    let names = ["ha", "hb", "hc", "hd", "he", "hf", "hg", "hh"];
    let sites: Vec<_> = names
        .iter()
        .map(|name| ("Base#run", Call, 2, *name))
        .collect();
    assert_unresolved(&rows, &sites);
}

/// `class Alias` for a constant `Alias = Foo`, assigned in another file,
/// reopens `Foo`: what its body defines and mixes in lands on a class the
/// rules cannot name, and nothing written there knows its `self` (R5-03).
/// Ruby 3.3: `Foo.new.run` is `[:alias_a, :foo]`, `Foo.new.go` `:base`,
/// `Mix#probe` run on `Foo.new` `:foo`.
#[test]
fn c10_ruby_dynamic_dispatch_a_class_reopened_through_a_constant_alias_proves_nothing() {
    // R5-03
    let rows = index(&[
        (
            "lib/base.rb",
            "class Base
  def a; :base; end
  def b; :base; end
  def helper; :base; end
end
module Mix; def b; :mix; end; def probe; b; end; end
",
        ),
        (
            "lib/foo.rb",
            "require_relative \"base\"
class Foo < Base
  def run; [a, b]; end
  def b; :foo; end
end
",
        ),
        ("lib/alias.rb", "require_relative \"foo\"\nAlias = Foo\n"),
        (
            "lib/reopen.rb",
            "require_relative \"alias\"
class Alias
  include Mix
  def a; :alias_a; end
  def go; helper; end
end
",
        ),
    ]);
    assert_unresolved(
        &rows,
        &[
            ("Foo#run", Call, 3, "a"),
            ("Mix#probe", Call, 6, "b"),
            ("Alias#go", Call, 5, "helper"),
        ],
    );
}

/// The same for a class reopened through an alias of its namespace
/// (`class F2::Bar` for `F2 = Foo`), and through an alias
/// `Object.const_set` makes (R7-11). Ruby 3.3: `Foo::Bar.new.run` and
/// `Baz.new.run` are `:alias_a`.
#[test]
fn c10_ruby_dynamic_dispatch_a_class_reopened_through_an_aliased_namespace_proves_nothing() {
    // R7-11
    let rows = index(&[
        (
            "lib/base.rb",
            "class Base; def a; :base; end; end
module Foo
  class Bar < Base
    def run; a; end
  end
end
class Baz < Base
  def run; a; end
end
",
        ),
        (
            "lib/alias.rb",
            "require_relative \"base\"
F2 = Foo
class F2::Bar
  def a; :alias_a; end
end
Object.const_set(:Qux, Baz)
class Qux
  def a; :alias_a; end
end
",
        ),
    ]);
    assert_unresolved(
        &rows,
        &[("Foo::Bar#run", Call, 4, "a"), ("Baz#run", Call, 8, "a")],
    );
}

/// `const_set` on any receiver, sent or not, and a scoped constant
/// assignment, of a value that may be a class, may make the name an alias
/// of any class: one reopened under it proves nothing; a literal value
/// aliases none (R8-09). Ruby 3.3: `Foo.new.run` is `:alias_a` with each,
/// and `Kept.new.run` `:kept`.
#[test]
fn c10_ruby_dynamic_dispatch_a_class_reopened_under_a_name_const_set_may_assign_proves_nothing() {
    // R8-09
    for assign in [
        "Object.send(:const_set, :Alias, Foo)",
        "self.class.const_set(:Alias, Foo)",
        "Object::Alias = Foo",
        "mod = Object\nmod.__send__(:const_set, \"Alias\", Foo)",
        "Object.method(:const_set).call(:Alias, Foo)",
    ] {
        let rows = index(&[
            (
                "lib/base.rb",
                "class Base; def a; :base; end; end
class Foo < Base
  def run; a; end
end
class Kept < Base
  def run; k; end
  def k; :kept; end
end
",
            ),
            (
                "lib/alias.rb",
                &format!(
                    "require_relative \"base\"
{assign}
module Ns; end
Ns::Kept = 1
class Alias; def a; :alias_a; end; end
"
                ),
            ),
        ]);
        assert_unresolved(&rows, &[("Foo#run", Call, 3, "a")]);
        assert!(
            resolved(&rows).contains(&("Kept#run".to_owned(), Call, 6, "Kept#k".to_owned())),
            "{assign}"
        );
    }
}

/// A method defined through a constant that names another constant
/// (`ALIAS = Foo`, `F = ::Foo`) lands on whatever class that constant
/// holds, which the rules do not read: no lookup of its name proves
/// anything (R5-03). Ruby 3.3: `Foo.go` is `[:alias_m, :alias_n,
/// :alias_o]`, `Foo.new.run` `[:fx, :fy]`.
#[test]
fn c10_ruby_dynamic_dispatch_a_method_defined_through_a_constant_alias_proves_nothing() {
    // R5-03
    let rows = index(&[(
        "lib/k.rb",
        "class Base; def self.m; :base; end; def self.n; :base; end; def self.o; :base; end; def x; :base; end; def y; :base; end; end
class Foo < Base
  def self.go; [m, n, o]; end
  def run; [x, y]; end
end
ALIAS = Foo
class << ALIAS
  def m; :alias_m; end
end
def ALIAS.n; :alias_n; end
ALIAS.class_eval do
  def self.o; :alias_o; end
end
module Outer
  F = ::Foo
  F.class_eval do
    def x; :fx; end
  end
  F.send(:define_method, :y) { :fy }
end
",
    )]);
    let calls: Vec<_> = resolved(&rows)
        .into_iter()
        .filter(|row| row.1 == Call)
        .collect();
    assert_eq!(calls, vec![]);
    assert_unresolved(
        &rows,
        &[
            ("Foo.go", Call, 3, "m"),
            ("Foo.go", Call, 3, "n"),
            ("Foo.go", Call, 3, "o"),
            ("Foo#run", Call, 4, "x"),
            ("Foo#run", Call, 4, "y"),
        ],
    );
}

/// Ruby skips a module the superclass's chain already holds. Past a step
/// of that chain the rules cannot name (a superclass written as a
/// constant they do not read, a gem's class), a module the class mixes
/// in may be held already, and then its own method is not the first:
/// a lookup that lands in it proves nothing when that step may hold it,
/// which a gem's class may for a module the application mixes into one
/// (R5-06). Ruby 3.3: `Child.new.run` and `Child.go` are `:base`; with
/// `Gemlib::Base` a gem's class, `Post.new.run` is `:app`.
#[test]
fn c10_ruby_ancestors_a_module_a_hidden_superclass_may_hold_proves_nothing() {
    // R5-06
    let opaque = index(&[(
        "lib/k.rb",
        "module M; def save; :m; end; end
module F; def find; :f; end; end
class Base
  include M
  extend F
  def save; :base; end
  def self.find; :base; end
end
B = Base
class Child < B
  include M
  extend F
  def run; save; end
  def self.go; find; end
end
",
    )]);
    let gem = index(&[
        (
            "config/initializers/mod.rb",
            "Gemlib::Base.include(Auditing)\n",
        ),
        (
            "app/models/auditing.rb",
            "module Auditing; def save; :auditing; end; end\n",
        ),
        (
            "app/models/application_record.rb",
            "class ApplicationRecord < Gemlib::Base\n  def save; :app; end\nend\n",
        ),
        (
            "app/models/post.rb",
            "class Post < ApplicationRecord\n  include Auditing\n  def run; save; end\nend\n",
        ),
        (
            "app/models/note.rb",
            "module Notes; def save; :notes; end; end\nclass Note < ApplicationRecord\n  include Notes\n  def run; save; end\nend\n",
        ),
    ]);
    let calls = |rows: &[pixel_model::FileRows]| -> Vec<_> {
        resolved(rows)
            .into_iter()
            .filter(|row| row.1 == Call)
            .collect()
    };
    assert_eq!(calls(&opaque), vec![]);
    assert_unresolved(
        &opaque,
        &[
            ("Child#run", Call, 13, "save"),
            ("Child.go", Call, 14, "find"),
        ],
    );
    assert_eq!(
        calls(&gem),
        labelled(&[("Note#run", Call, 4, "Notes#save")])
            .into_iter()
            .collect::<Vec<_>>()
    );
    assert_unresolved(&gem, &[("Post#run", Call, 3, "save")]);
}

/// A mixin computed from a name (`Object.const_get(:M)`, sent or not,
/// `Kernel.const_get("N")`) or written through `Object` (`Object::Q`) may
/// be any module of the graph, whatever the constant the expression is
/// rooted in: the superclass may hold the module already (R6-13). Ruby
/// 3.3: `Child.new.run` is `[:base, :base, :base]`.
#[test]
fn c10_ruby_ancestors_a_module_named_by_a_string_may_be_held_already() {
    // R6-13
    let rows = index(&[(
        "lib/k.rb",
        "module M; def save; :m; end; end
module N; def tag; :n; end; end
module Q; def size; :q; end; end
class Base
  include Object.const_get(:M)
  send(:include, Kernel.const_get(\"N\"))
  include Object::Q
  def save; :base; end; def tag; :base; end; def size; :base; end
end
class Child < Base
  include M
  include N
  include Q
  def run; [save, tag, size]; end
end
",
    )]);
    assert_unresolved(
        &rows,
        &[
            ("Child#run", Call, 14, "save"),
            ("Child#run", Call, 14, "tag"),
            ("Child#run", Call, 14, "size"),
        ],
    );
}

/// A mixin core code computes from what it is given (`Array(M).first`,
/// `Hash.new(N)[:x]`, `Struct.new(:m).new(Q).m`) may be that module: the
/// superclass may hold it already. `Struct.new(:a)`, given no constant,
/// makes no module of the graph (R7-06). Ruby 3.3: `Child.new.run` is
/// `[:base, :base, :base]`, `Pair.new.run` `:m2`.
#[test]
fn c10_ruby_ancestors_a_module_core_code_is_given_may_be_held_already() {
    // R7-06
    let rows = index(&[(
        "lib/k.rb",
        "module M; def save; :m; end; end
module N; def tag; :n; end; end
module Q; def size; :q; end; end
module M2; def save; :m2; end; end
class Base
  include Array(M).first
  include Hash.new(N)[:x]
  include Struct.new(:m).new(Q).m
  def save; :base; end; def tag; :base; end; def size; :base; end
end
class Child < Base
  include M
  include N
  include Q
  def run; [save, tag, size]; end
end
class Pair < Struct.new(:a)
  include M2
  def run; save; end
end
",
    )]);
    assert_unresolved(
        &rows,
        &[
            ("Child#run", Call, 15, "save"),
            ("Child#run", Call, 15, "tag"),
            ("Child#run", Call, 15, "size"),
        ],
    );
    assert!(resolved(&rows).contains(&("Pair#run".to_owned(), Call, 19, "M2#save".to_owned())));
}

/// A hook defined other than by a plain `def` (`define_singleton_method`,
/// `alias_method` in `class << self`) runs statements the rules do not
/// read: the includer's lists are unknown from the front, as for a hook
/// statement they cannot read (R5-08). Ruby 3.3: `Post.load` and
/// `Post2.load` are `:cm`.
#[test]
fn c10_ruby_ancestors_a_hook_defined_other_than_by_def_is_unknown() {
    // R5-08
    let rows = index(&[(
        "lib/k.rb",
        "class Base; def self.find; :base; end; end
module CM; def find; :cm; end; end
module Finder
  define_singleton_method(:included) { |base| CM.send(:extend_object, base) }
end
class Post < Base
  include Finder
  def self.load; find; end
end
module Finder2
  def self.setup(base); CM.send(:extend_object, base); end
  class << self
    alias_method :included, :setup
  end
end
class Post2 < Base
  include Finder2
  def self.load; find; end
end
",
    )]);
    assert_unresolved(
        &rows,
        &[
            ("Post.load", Call, 8, "find"),
            ("Post2.load", Call, 18, "find"),
        ],
    );
}

/// A hook defined on `Module` or `Class` itself (reopened, through a
/// module prepended to it, or in a block evaluated on it) runs for every
/// module or class that defines none of its own: what it does is unknown
/// (R6-16). Ruby 3.3: `Post.load` is `:cm` with each.
#[test]
fn c10_ruby_ancestors_a_hook_on_module_or_class_runs_for_every_one() {
    // R6-16
    let program = |hook: &str| {
        index(&[(
            "lib/k.rb",
            &format!(
                "module CM; def find; :cm; end; end
class Base; def self.find; :base; end; end
{hook}
module Finder; end
class Post < Base; include Finder; def self.load; find; end; end
"
            ),
        )])
    };
    for hook in [
        "class Module; def included(base); base.extend(CM) if base.is_a?(Class); end; end",
        "Module.class_eval { def included(base); base.extend(CM); end }",
        "class Class; def inherited(sub); sub.extend(CM); end; end",
    ] {
        assert_unresolved(&program(hook), &[("Post.load", Call, 5, "find")]);
    }
}

/// An `inherited` defined on the own side of `Object` or `BasicObject`
/// (reopened, through `class << Object`, `def Object.inherited`, or a
/// module `Object` extends from outside its body) runs for every class,
/// whatever its superclass: a class's singleton chain reaches it before
/// `Class` (R7-07). Ruby 3.3: `Post.load` and `Post2.load` are `:cm` with
/// each; `K3.new.probe` is `:pm`.
#[test]
fn c10_ruby_ancestors_an_inherited_on_object_runs_for_every_class() {
    // R7-07
    let program = |hook: &str| {
        index(&[(
            "lib/k.rb",
            &format!(
                "module CM; def find; :cm; end; end
class Base; def self.find; :base; end; end
{hook}
class Post < Base; def self.load; find; end; end
class Post2 < (Base); def self.load; find; end; end
"
            ),
        )])
    };
    for hook in [
        "class Object; def self.inherited(sub); super; sub.extend(CM); end; end",
        "class << Object; def inherited(sub); super; sub.extend(CM); end; end",
        "def Object.inherited(sub); super; sub.extend(CM); end",
        "module InhMod; def inherited(sub); super; sub.extend(CM); end; end\nObject.extend(InhMod)",
        "class BasicObject; def self.inherited(sub); super; sub.extend(::CM); end; end",
    ] {
        let rows = program(hook);
        let line = 3 + hook.lines().count() as u32;
        assert_unresolved(
            &rows,
            &[
                ("Post.load", Call, line, "find"),
                ("Post2.load", Call, line + 1, "find"),
            ],
        );
    }
    let rows = index(&[(
        "lib/k.rb",
        "module PM; def c; :pm; end; end
class Base; end
class Object; def self.inherited(sub); super; sub.prepend(PM); end; end
class K3 < (Base); def c; :k3; end; def probe; c; end; end
",
    )]);
    assert_unresolved(&rows, &[("K3#probe", Call, 4, "c")]);
}

/// A method body that passes a block held in a variable, or its own
/// block argument, to a method that runs it with another `self`
/// (`instance_exec`, `instance_eval` and their family) may be passing any
/// block, lambda or proc written in it: none of them runs on the method's
/// object for sure, while a body that passes none, or a symbol's proc,
/// keeps it (R5-11). Ruby 3.3: `render`, `render2` and `render3` give
/// `:pdf`; `kept` gives `[:report]`.
#[test]
fn c10_ruby_dynamic_dispatch_a_block_passed_on_to_rebind_self_runs_elsewhere() {
    // R5-11
    let rows = index(&[(
        "lib/k.rb",
        "class Pdf; def title; :pdf; end; end
class Report
  def title; :report; end
  def render(pdf)
    header = -> { title }
    pdf.instance_exec(&header)
  end
  def render2(pdf)
    footer = proc { title }
    pdf.instance_eval(&footer)
  end
  def render3(pdf, &block)
    [1].each { title }
    pdf.instance_eval(&block)
  end
  def kept(pdf)
    rows = [1].map { title }
    pdf.instance_exec(pdf, &:title)
    rows
  end
end
",
    )]);
    let calls: Vec<_> = resolved(&rows)
        .into_iter()
        .filter(|row| row.1 == Call)
        .collect();
    assert_eq!(
        calls,
        labelled(&[("Report#kept", Call, 17, "Report#title")])
            .into_iter()
            .collect::<Vec<_>>()
    );
    assert_unresolved(
        &rows,
        &[
            ("Report#render", Call, 5, "title"),
            ("Report#render2", Call, 9, "title"),
            ("Report#render3", Call, 13, "title"),
        ],
    );
}

/// The same for a block written inline in the block argument (`&-> {}`,
/// `&proc {}`, `&lambda {}`, `&Proc.new {}`), one read from a collection
/// (`&parts[:h]`), and the rebinding method sent (`send(:instance_exec,
/// &header)`, `public_send`), a literal block given to it too
/// (`send(:instance_exec) { }`, `Pdf.send(:define_method, :t) { }`); a
/// method object's block keeps `self` (R6-12, R7-10). Ruby 3.3: `a` to
/// `j` give `:pdf`, `e` `:report`.
#[test]
fn c10_ruby_dynamic_dispatch_an_inline_block_passed_on_to_rebind_self_runs_elsewhere() {
    // R6-12, R7-10
    let rows = index(&[(
        "lib/report.rb",
        "class Pdf; def title; :pdf; end; end
class Report
  def title; :report; end
  def a; Pdf.new.instance_exec(&-> { title }); end
  def b; Pdf.new.instance_eval(&proc { title }); end
  def c; Pdf.new.instance_exec(&lambda { title }); end
  def d; Pdf.new.instance_exec(&Proc.new { title }); end
  def e; Pdf.new.instance_exec(&method(:helper)); end
  def helper; title; end
  def f; parts = { h: proc { title } }; Pdf.new.instance_exec(&parts[:h]); end
  def g; header = -> { title }; Pdf.new.send(:instance_exec, &header); end
  def h; Pdf.new.public_send(:instance_eval) { title }; end
  def i; Pdf.new.__send__(:instance_exec) { title }; end
  def j; Pdf.send(:define_method, :t2) { title }; Pdf.new.t2; end
end
",
    )]);
    let calls: Vec<_> = resolved(&rows)
        .into_iter()
        .filter(|row| row.0.starts_with("Report#") && row.3 == "Report#title")
        .collect();
    assert_eq!(
        calls,
        labelled(&[("Report#helper", Call, 9, "Report#title")])
            .into_iter()
            .collect::<Vec<_>>()
    );
    assert_unresolved(
        &rows,
        &[
            ("Report#a", Call, 4, "title"),
            ("Report#b", Call, 5, "title"),
            ("Report#c", Call, 6, "title"),
            ("Report#d", Call, 7, "title"),
            ("Report#f", Call, 10, "title"),
            ("Report#g", Call, 11, "title"),
            ("Report#h", Call, 12, "title"),
            ("Report#i", Call, 13, "title"),
            ("Report#j", Call, 14, "title"),
        ],
    );
}

/// A block, or a block argument, given to `send`, `public_send` or
/// `__send__` of a name the rules cannot read (a constant, a parameter, a
/// splat), or to `call` on a method object (`method(:instance_exec)`), may
/// run with another `self`: the method sent may be any (R8-07). Ruby 3.3:
/// each of `a` to `e` gives `:pdf`.
#[test]
fn c10_ruby_dynamic_dispatch_a_block_given_to_a_send_of_any_name_runs_elsewhere() {
    // R8-07
    let rows = index(&[(
        "lib/report.rb",
        "class Pdf; def title; :pdf; end; end
class Report
  MODE = :instance_exec
  def title; :report; end
  def a; Pdf.new.send(MODE) { title }; end
  def b; Pdf.new.method(:instance_exec).call { title }; end
  def c(m = :instance_eval); Pdf.new.public_send(m) { title }; end
  def d; Pdf.new.__send__(*[:instance_exec]) { title }; end
  def on(m, &b); Pdf.new.send(m, &b); end
  def e; on(:instance_exec) { title }; end
end
",
    )]);
    assert_unresolved(
        &rows,
        &[
            ("Report#a", Call, 5, "title"),
            ("Report#b", Call, 6, "title"),
            ("Report#c", Call, 7, "title"),
            ("Report#d", Call, 8, "title"),
            ("Report#e", Call, 10, "title"),
        ],
    );
}

/// A block given to a method of the file whose body passes its block on
/// to rebind `self` (`def on_pdf(&block); Pdf.new.instance_exec(&block);
/// end`, with `...` too), directly or through another such method of the
/// file, may run with another `self` too, whatever the receiver, and so
/// may a lambda given to one as an argument it passes on (R6-17, R7-12).
/// Ruby 3.3: `render`, `render2`, `r3`, `r5` and `r8` give `:pdf`, `kept`
/// `:report`.
#[test]
fn c10_ruby_dynamic_dispatch_a_block_given_to_a_rebinding_helper_runs_elsewhere() {
    // R6-17, R7-12
    let rows = index(&[(
        "lib/report.rb",
        "class Pdf; def title; :pdf; end; end
class Report
  def title; :report; end
  def render; on_pdf { title }; end
  def render2; self.on_pdf { title }; end
  def on_pdf(&block); Pdf.new.instance_exec(&block); end
  def on_pdf3(...) = Pdf.new.instance_exec(...)
  def r3; on_pdf3 { title }; end
  def outer(&b); on_pdf(&b); end
  def r5; outer { title }; end
  def on_pdf8(b); Pdf.new.instance_exec(&b); end
  def r8; on_pdf8(-> { title }); end
  def kept; [1].map { title }; end
end
",
    )]);
    assert_unresolved(
        &rows,
        &[
            ("Report#render", Call, 4, "title"),
            ("Report#render2", Call, 5, "title"),
            ("Report#r3", Call, 8, "title"),
            ("Report#r5", Call, 10, "title"),
            ("Report#r8", Call, 12, "title"),
        ],
    );
    assert!(resolved(&rows).contains(&(
        "Report#kept".to_owned(),
        Call,
        13,
        "Report#title".to_owned()
    )));
}

/// A lambda or proc literal passed as an argument at any depth (in an
/// array, a hash, parentheses, a splat, through another helper), stored
/// anywhere but in a local that is only called, or returned, may be
/// `instance_exec`ed by whatever is given it; a block given to a helper
/// `define_method` defines that rebinds its block runs elsewhere too. One
/// called right away, or from a local only called, keeps `self` (R8-08).
/// Ruby 3.3: `r1` to `r6` and `mk.call` evaluated on a `Pdf` give `:pdf`,
/// `kept` and `kept2` `:report`.
#[test]
fn c10_ruby_dynamic_dispatch_a_lambda_passed_stored_or_returned_runs_elsewhere() {
    // R8-08
    let rows = index(&[(
        "lib/report.rb",
        "class Pdf; def title; :pdf; end; end
class Report
  def title; :report; end
  define_method(:on_pdf) { |&b| Pdf.new.instance_exec(&b) }
  def r1; on_pdf { title }; end
  def on_pdf2(bs); bs.map { |b| Pdf.new.instance_exec(&b) }.first; end
  def r2; on_pdf2([-> { title }]); end
  def on_pdf3(b); Pdf.new.instance_exec(&b); end
  def r3; on_pdf3(Kernel.proc { title }); end
  def outer4(b); on_pdf3(b); end
  def r4; outer4(-> { title }); end
  def r5; on_pdf3(*[(-> { title })]); end
  def r6; @cb = lambda { title }; on_pdf3(@cb); end
  def mk; -> { title }; end
  def kept; -> { title }.call; end
  def kept2; b = Proc.new { title }; b.(); b.call; end
end
",
    )]);
    assert_unresolved(
        &rows,
        &[
            ("Report#r1", Call, 5, "title"),
            ("Report#r2", Call, 7, "title"),
            ("Report#r3", Call, 9, "title"),
            ("Report#r4", Call, 11, "title"),
            ("Report#r5", Call, 12, "title"),
            ("Report#r6", Call, 13, "title"),
            ("Report#mk", Call, 14, "title"),
        ],
    );
    let kept: Vec<_> = resolved(&rows)
        .into_iter()
        .filter(|row| row.3 == "Report#title")
        .collect();
    assert_eq!(
        kept,
        vec![
            (
                "Report#kept".to_owned(),
                Call,
                15,
                "Report#title".to_owned()
            ),
            (
                "Report#kept2".to_owned(),
                Call,
                16,
                "Report#title".to_owned()
            ),
        ]
    );
}

/// The lambda locals of one method are each read by their own name, from
/// one count of the method's reads (R9-01): `a`, only called, keeps
/// `self`; `b`, passed, and `c`, assigned again, do not. Ruby 3.3:
/// `Report.new.r` gives `[:report, :pdf, :report]`.
#[test]
fn c10_ruby_dynamic_dispatch_lambda_locals_of_one_method_each_read_by_their_own_name() {
    // R9-01
    let rows = index(&[(
        "lib/report.rb",
        "class Pdf; def title; :pdf; end; end
class Report
  def title; :report; end
  def on_pdf(b); Pdf.new.instance_exec(&b); end
  def r
    a = -> { title }
    b = -> { title }
    c = -> { title }
    c = b
    [a.call, on_pdf(b), c.()]
  end
end
",
    )]);
    assert_unresolved(
        &rows,
        &[
            ("Report#r", Call, 7, "title"),
            ("Report#r", Call, 8, "title"),
        ],
    );
    let kept: Vec<_> = resolved(&rows)
        .into_iter()
        .filter(|row| row.3 == "Report#title")
        .collect();
    assert_eq!(
        kept,
        vec![("Report#r".to_owned(), Call, 6, "Report#title".to_owned())]
    );
}

/// What may not be in a module's own list may not be in its includer's
/// either; a step that may not be there proves its module only where the
/// same module comes again at a step that is there; and a module
/// prepended where one that may not be there already is goes in front,
/// or not; so does what a module mixed in after one it may itself bring
/// (R5-07, found by differential fuzzing). Ruby 3.3: `K0.new.b` raises
/// NoMethodError, `K4.new.b` is `:k2`, `K3.new.b` `:m0`, `K5.b` and
/// `K6.new.b` `:m6`.
#[test]
fn c10_ruby_ancestors_a_step_that_may_not_be_there_is_carried_where_its_module_goes() {
    // R5-07
    let rows = index(&[(
        "lib/k.rb",
        "module M0; def b; :m0; end; end
module M2; include M0 unless true; end
module M3; def b; :m3; end; include M0 unless true; end
module M4; def b; :m4; end; end
module M5; include M4; end
class K0; prepend M2; end
class K1; def b; :k1; end; end
class K2 < K1; prepend M5 if false; def b; :k2; end; end
class K4 < K2; prepend M4 if false; end
class K3 < K1; prepend M0, M3; end
K0.new.b
K4.new.b
K3.new.b
module M6; def c; :m6; end; prepend M0 unless true; def b; :m6; end; end
class K5; extend M6, M0; end
class K6; prepend M6, M0; end
K5.b
K6.new.b
",
    )]);
    assert_unresolved(
        &rows,
        &[
            ("lib/k.rb", Call, 11, "K0.new.b"),
            ("lib/k.rb", Call, 12, "K4.new.b"),
            ("lib/k.rb", Call, 13, "K3.new.b"),
            ("lib/k.rb", Call, 17, "K5.b"),
            ("lib/k.rb", Call, 18, "K6.new.b"),
        ],
    );
}
