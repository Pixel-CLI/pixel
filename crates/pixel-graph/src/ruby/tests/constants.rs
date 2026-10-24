// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Calls on a constant: Ruby's lexical constant lookup (shadowing by a
//! nearer scope), constructors, Active Job and Action Mailer dispatch.

use pixel_model::EdgeKind::{Call, Import, Reference};
use pixel_model::SymbolKind;

use super::{assert_unresolved, index, labelled, resolved};

/// `B` inside `module A; class C` is `A::B`, `::B` the top-level one;
/// `B.new` reaches `initialize`, `B.new(1).run` the instance method. A
/// same-name class in an unrelated module changes nothing. v1
/// `ruby_constants_should_resolve_class_constructor_and_instance_calls_through_every_update`
/// (full-build form, with its `rival.rb`).
#[test]
fn c10_ruby_shadowing_a_constant_receiver_is_found_through_the_lexical_nesting() {
    let rows = index(&[
        (
            "targets.rb",
            "module A
  class B
    def self.run; end
    def initialize(value = nil); end
    def run; end
  end
end
class B
  def self.run; end
  def initialize; end
  def run; end
end
",
        ),
        (
            "caller.rb",
            "module A
  class C
    def invoke
      B.run
      ::B.run
      A::B.run
      B.new(1)
      B.new(1).run
    end
  end
end
",
        ),
        (
            "rival.rb",
            "module Unreachable\n class B\n def self.run; end\n def run; end\n end\nend\n",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("A::C#invoke", Call, 4, "A::B.run"),
            ("A::C#invoke", Call, 5, "B.run"),
            ("A::C#invoke", Call, 6, "A::B.run"),
            ("A::C#invoke", Call, 7, "A::B#initialize"),
            ("A::C#invoke", Call, 8, "A::B#initialize"),
            ("A::C#invoke", Call, 8, "A::B#run"),
        ])
    );
}

/// `class A::D` opens `A::D` alone: Ruby's nesting there is `[A::D]`, so
/// `B` is the top-level class, not `A::B`; `module A; class C` sees both.
/// v1 `ruby_constants_should_use_lexical_nesting_not_qualified_name_prefixes`.
#[test]
fn c10_ruby_shadowing_class_a_colon_d_does_not_see_the_constants_of_a() {
    let rows = index(&[
        (
            "targets.rb",
            "module A
  class B
    def self.run; end
  end
end
class B
  def self.run; end
end
",
        ),
        (
            "caller.rb",
            "module A\n class C\n def invoke\n B.run\n end\n end\nend\nclass A::D\n def invoke\n B.run\n end\nend\n",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("A::C#invoke", Call, 4, "A::B.run"),
            ("A::D#invoke", Call, 10, "B.run"),
        ])
    );
}

/// A class `A::C::B` makes `B` inside `A::C` name it, and it has no `run`
/// nor `initialize`: the calls are unresolved, never `A::B`'s. v1
/// `ruby_constants_should_invalidate_edges_when_a_new_constant_shadows_the_receiver`
/// (full-build form).
#[test]
fn c10_ruby_shadowing_a_nearer_constant_hides_an_outer_one_of_the_same_name() {
    let rows = index(&[
        (
            "targets.rb",
            "module A
  class B
    def self.run; end
    def initialize(value = nil); end
    def run; end
  end
end
class B
  def self.run; end
  def initialize; end
  def run; end
end
",
        ),
        (
            "caller.rb",
            "module A
  class C
    def invoke
      B.run
      ::B.run
      A::B.run
      B.new(1)
      B.new(1).run
    end
  end
end
",
        ),
        ("shadow.rb", "class A::C::B\nend\n"),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("A::C#invoke", Call, 5, "B.run"),
            ("A::C#invoke", Call, 6, "A::B.run"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("A::C#invoke", Call, 4, "B.run"),
            ("A::C#invoke", Call, 7, "B.new"),
            ("A::C#invoke", Call, 8, "B.new"),
            ("A::C#invoke", Call, 8, "B.new.run"),
        ],
    );
}

/// A nearer first segment owns the whole path even when the rest is
/// absent there: in `A::C`, `B::Child` is `A::C::B::Child`, which does not
/// exist, so it is not `A::B::Child`; in `A`'s own body it is. v1
/// `constant_lookup_should_honor_scope_boundaries_and_shadow_the_first_segment`.
#[test]
fn c10_ruby_shadowing_a_nearer_first_segment_owns_the_rest_of_the_path() {
    let rows = index(&[
        (
            "remote.rb",
            "class B; end
module A
  class B
    class Child
      def self.run; end
    end
  end
  class C
    class B; end
  end
end
",
        ),
        (
            "local.rb",
            "module A
  class C
    def go; B::Child.run; end
  end
  def self.go; B::Child.run; end
end
",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[("A.go", Call, 5, "A::B::Child.run")])
    );
    assert_unresolved(&rows, &[("A::C#go", Call, 3, "B::Child.run")]);
}

/// A constant names a class only where Ruby would find it: `Lost` lives in
/// two modules and not at the top level, `Missing` nowhere, and `Known`
/// has an instance method, not a class method. v1
/// `ruby_constants_should_not_fall_back_to_an_unrelated_method_or_reopened_definition`
/// (first part).
#[test]
fn c10_ruby_shadowing_a_constant_never_falls_back_to_a_same_tail_class() {
    let rows = index(&[
        (
            "caller.rb",
            "def invoke\n Lost.unique\n Missing.unique\n Known.unique\nend\n",
        ),
        (
            "targets.rb",
            "module X\n class Lost; end\nend\nmodule Y\n class Lost; end\nend\nclass Known\n def unique; end\nend\n",
        ),
    ]);
    assert_eq!(resolved(&rows), labelled(&[]));
    assert_unresolved(
        &rows,
        &[
            ("invoke", Call, 2, "Lost.unique"),
            ("invoke", Call, 3, "Missing.unique"),
            ("invoke", Call, 4, "Known.unique"),
        ],
    );
}

/// `A::B.run` defined in two files has no one target (decision 5); the
/// other calls keep theirs. v1
/// `ruby_constants_should_not_fall_back_to_an_unrelated_method_or_reopened_definition`
/// (second part).
#[test]
fn c10_ruby_dynamic_dispatch_a_class_method_defined_in_two_files_is_unresolved() {
    let rows = index(&[
        (
            "targets.rb",
            "module A
  class B
    def self.run; end
    def initialize(value = nil); end
    def run; end
  end
end
class B
  def self.run; end
  def initialize; end
  def run; end
end
",
        ),
        (
            "caller.rb",
            "module A
  class C
    def invoke
      B.run
      ::B.run
      A::B.run
      B.new(1)
      B.new(1).run
    end
  end
end
",
        ),
        ("reopened.rb", "class A::B\n def self.run; end\nend\n"),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("A::C#invoke", Call, 5, "B.run"),
            ("A::C#invoke", Call, 7, "A::B#initialize"),
            ("A::C#invoke", Call, 8, "A::B#initialize"),
            ("A::C#invoke", Call, 8, "A::B#run"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("A::C#invoke", Call, 4, "B.run"),
            ("A::C#invoke", Call, 6, "A::B.run"),
        ],
    );
}

/// Active Job's class methods run the job's `perform`, `set(...)` first or
/// not; a mailer's class method its instance method. A chain past them
/// (`deliver_later`) proves nothing. v1
/// `ruby_rails_dispatch_should_be_probable_and_explicit_class_methods_should_win`
/// (first part; v1 marked these `probable`, which the MVP has no column
/// for: a rule proves them, decision 1).
#[test]
fn c10_ruby_dynamic_dispatch_active_job_and_mailers_reach_their_instance_method() {
    let rows = index(&[
        (
            "caller.rb",
            "def invoke
 Job.perform_later(1)
 Job.perform_now(1)
 Job.perform_async(1)
 Job.perform_in(1, 2)
 Job.perform_at(1, 2)
 Job.set(wait: 1).perform_later(1)
 NoticeMailer.welcome(1).deliver_later
 Job.set(wait: 1).perform_now(1)
 Job.set(wait: 1).perform_async(1)
 Job.set(wait: 1).run
end
",
        ),
        (
            "targets.rb",
            "class Job\n def perform; end\nend\nclass NoticeMailer\n def welcome; end\nend\n",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("invoke", Call, 2, "Job#perform"),
            ("invoke", Call, 3, "Job#perform"),
            ("invoke", Call, 4, "Job#perform"),
            ("invoke", Call, 5, "Job#perform"),
            ("invoke", Call, 6, "Job#perform"),
            ("invoke", Call, 7, "Job#perform"),
            ("invoke", Call, 8, "NoticeMailer#welcome"),
            ("invoke", Call, 9, "Job#perform"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("invoke", Call, 7, "Job.set"),
            ("invoke", Call, 8, "NoticeMailer.welcome(1).deliver_later"),
            ("invoke", Call, 10, "Job.set.perform_async"),
            ("invoke", Call, 11, "Job.set.run"),
        ],
    );
}

/// A class method the class defines itself wins over the dispatch rules,
/// and makes `Job.set(...).perform_later` unprovable. v1
/// `ruby_rails_dispatch_should_be_probable_and_explicit_class_methods_should_win`
/// (with `overrides.rb`).
#[test]
fn c10_ruby_dynamic_dispatch_an_explicit_class_method_wins_over_dispatch() {
    let rows = index(&[
        (
            "caller.rb",
            "def invoke
 Job.perform_later(1)
 Job.perform_now(1)
 Job.perform_async(1)
 Job.perform_in(1, 2)
 Job.perform_at(1, 2)
 Job.set(wait: 1).perform_later(1)
 NoticeMailer.welcome(1).deliver_later
 Job.set(wait: 1).perform_now(1)
 Job.set(wait: 1).perform_async(1)
 Job.set(wait: 1).run
end
",
        ),
        (
            "targets.rb",
            "class Job\n def perform; end\nend\nclass NoticeMailer\n def welcome; end\nend\n",
        ),
        (
            "overrides.rb",
            "class Job\n def self.perform_later; end\nend\nclass NoticeMailer\n def self.welcome; end\nend\n",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("invoke", Call, 2, "Job.perform_later"),
            ("invoke", Call, 3, "Job#perform"),
            ("invoke", Call, 4, "Job#perform"),
            ("invoke", Call, 5, "Job#perform"),
            ("invoke", Call, 6, "Job#perform"),
            ("invoke", Call, 8, "NoticeMailer.welcome"),
            ("invoke", Call, 9, "Job#perform"),
        ])
    );
    assert_unresolved(&rows, &[("invoke", Call, 7, "Job.set.perform_later")]);
}

/// A custom `self.new` is the target of `new`, and its return value is of
/// no known class: `B.new(1).run` is unresolved. v1
/// `ruby_constructor_should_respect_an_explicit_new_override`.
#[test]
fn c10_ruby_dynamic_dispatch_a_custom_new_proves_no_instance_class() {
    let rows = index(&[
        (
            "targets.rb",
            "module A
  class B
    def self.run; end
    def initialize(value = nil); end
    def run; end
  end
end
class B
  def self.run; end
  def initialize; end
  def run; end
end
",
        ),
        (
            "caller.rb",
            "module A
  class C
    def invoke
      B.run
      ::B.run
      A::B.run
      B.new(1)
      B.new(1).run
    end
  end
end
",
        ),
        (
            "factory.rb",
            "class A::B\n def self.new(value); Object.new; end\nend\n",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("A::C#invoke", Call, 4, "A::B.run"),
            ("A::C#invoke", Call, 5, "B.run"),
            ("A::C#invoke", Call, 6, "A::B.run"),
            ("A::C#invoke", Call, 7, "A::B.new"),
            ("A::C#invoke", Call, 8, "A::B.new"),
        ])
    );
    assert_unresolved(&rows, &[("A::C#invoke", Call, 8, "B.new.run")]);
}

/// A module has no `new` and no dispatch; a class that defines `self.set`
/// configures nothing the rules know. Only the explicit `Job.set` calls
/// resolve. v1 `ruby_dispatch_should_reject_non_classes_and_overridden_configurators`.
#[test]
fn c10_ruby_dynamic_dispatch_modules_and_custom_configurators_get_no_dispatch() {
    let rows = index(&[
        (
            "caller.rb",
            "def invoke
 Helpers.new.run
 Helpers.new
 Helpers.perform_later
 HelpersMailer.welcome
 Job.set(wait: 1).perform_now
 Job.set(wait: 1).perform_async
 Job.set(wait: 1).run
 Helpers.set(wait: 1).perform_now
end
",
        ),
        (
            "targets.rb",
            "module Helpers
 def initialize; end
 def run; end
 def perform; end
end
module HelpersMailer
 def welcome; end
end
class Job
 def self.set(options); Object.new; end
 def perform; end
end
",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("invoke", Call, 6, "Job.set"),
            ("invoke", Call, 7, "Job.set"),
            ("invoke", Call, 8, "Job.set"),
        ])
    );
}

/// Ruby reads a superclass expression outside its class, a statement after
/// `end` outside the class it closes, and code in a one-line class inside
/// it. Decision 4 reads that nesting from the tree; v1 placed sites by
/// line and left all but `::B.run` unresolved
/// (`ruby_constants_should_not_guess_lexical_nesting_when_line_spans_are_indistinguishable`,
/// `ruby_constants_should_not_treat_class_headers_or_trailing_statements_as_class_body_calls`).
#[test]
fn c10_ruby_shadowing_class_boundaries_on_one_line_are_read_from_the_tree() {
    let rows = index(&[
        (
            "targets.rb",
            "module A\n  class B\n    def self.run; end\n  end\nend\nclass C\n  class B\n    def self.run; String; end\n  end\nend\nclass B\n  def self.run; Object; end\nend\n",
        ),
        (
            "same_lines.rb",
            "module A; class D\n def invoke\n B.run\n end\nend; end\n",
        ),
        ("header.rb", "class C < B.run\nend\n"),
        ("trailing.rb", "class C\nend; B.run\n"),
        ("one_line.rb", "class C; def invoke; B.run; end; end\n"),
        ("rooted.rb", "class C < ::B.run\nend\n"),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("A::D#invoke", Call, 3, "A::B.run"),
            ("header.rb", Call, 1, "B.run"),
            ("trailing.rb", Call, 2, "B.run"),
            ("C#invoke", Call, 1, "C::B.run"),
            ("rooted.rb", Call, 1, "B.run"),
        ])
    );
}

/// A file required later in the run is read like any other: the
/// constructor and the instance method resolve, and so does the
/// `require_relative`. v1
/// `ruby_constants_should_resolve_a_late_required_file_and_preserve_rename_spelling`
/// (full-build form).
#[test]
fn c10_ruby_imports_a_required_class_is_constructed_and_called() {
    let rows = index(&[
        (
            "caller.rb",
            "require_relative 'later'\ndef invoke\n Widget.new(1).run\nend\n",
        ),
        ("targets.rb", ""),
        (
            "later.rb",
            "class Widget\n def initialize(value); end\n def run; end\nend\n",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("caller.rb", Import, 1, "later.rb"),
            ("invoke", Call, 3, "Widget#initialize"),
            ("invoke", Call, 3, "Widget#run"),
        ])
    );
}

/// A constant assignment defines a constant of the scope it is written
/// in, so `Client` in `Billing` is `Billing::Client`, never the top-level
/// `Client`: `Struct.new` makes it a class whose superclass the graph
/// lacks, with no `connect` and no `initialize`; `Gateway = Client` and
/// `LIMIT = 10` are constants whose value the rules do not read. Ruby 3.3:
/// `Billing.charge` builds a `Billing::Client`, then raises
/// `NoMethodError` on `connect`.
#[test]
fn c10_ruby_shadowing_an_assigned_constant_hides_a_top_level_class() {
    // D11
    let rows = index(&[
        (
            "lib/billing.rb",
            "module Billing
  Client = Struct.new(:key)
  Gateway = Client
  LIMIT = 10
  def self.charge
    Client.new(1)
    Client.connect
    Gateway.connect
    LIMIT.to_s
  end
end
",
        ),
        (
            "lib/client.rb",
            "class Client\n  def initialize(a); end\n  def self.connect; end\nend\n",
        ),
        (
            "lib/gateway.rb",
            "class Gateway\n  def self.connect; end\nend\n",
        ),
    ]);
    assert_eq!(resolved(&rows), labelled(&[]));
    assert_unresolved(
        &rows,
        &[
            ("Billing.charge", Call, 6, "Client.new"),
            ("Billing.charge", Call, 7, "Client.connect"),
            ("Billing.charge", Call, 8, "Gateway.connect"),
            ("Billing.charge", Call, 9, "LIMIT.to_s"),
        ],
    );
}

/// `X ||= v` assigns only when Ruby's lookup finds no `X`, here the
/// top-level `Client` loaded first, and `X &&= v` only when it finds one:
/// whether either defines a constant depends on what another file loads
/// first, so neither is recorded, and a constant path through its name
/// proves nothing (R5-12). Ruby 3.3: `Other.charge` is `:top`, and
/// `defined?(Other::Client)` is `nil`.
#[test]
fn c10_ruby_shadowing_an_or_assigned_constant_may_define_nothing() {
    // R5-12
    let rows = index(&[(
        "lib/k.rb",
        "class Client; def self.connect; :top; end; end
class Gate; def self.open; end; end
module Other
  Client ||= Class.new do
    def self.connect; :other; end
  end
  Gate &&= 1
  def self.charge; Client.connect; end
  def self.go; Gate.open; end
end
",
    )]);
    let names: Vec<&str> = rows
        .iter()
        .flat_map(|r| &r.symbols)
        .map(|s| s.name.as_str())
        .collect();
    assert!(!names.contains(&"Other::Client") && !names.contains(&"Other::Gate"));
    assert_unresolved(
        &rows,
        &[
            ("Other.charge", Call, 8, "Client.connect"),
            ("Other.go", Call, 9, "Gate.open"),
        ],
    );
}

/// A constant assigned where the assignment may not run (`unless
/// defined?(X)`, `if false`, `defined?(X) || X = v`, a block, `const_set`
/// under a condition) is no more recorded than `X ||= v`: Ruby may never
/// assign it, and a constant path through its name proves nothing
/// (R6-05). Ruby 3.3: `defined?(Client)` in `Other` finds the top-level
/// `Client`, so `Other::Helper.new.run` is `:top`, and `Other::Client`,
/// `Other::Store` and `Other::Limit` raise NameError.
#[test]
fn c10_ruby_shadowing_a_constant_assigned_under_a_condition_may_define_nothing() {
    // R6-05
    let rows = index(&[(
        "lib/k.rb",
        "class Client
  def charge; :top; end
end
module Other
  Client = Class.new { def charge; :inner; end } unless defined?(Client)
  Store = Class.new { def charge; :inner; end } if false
  defined?(Pay) || Pay = Struct.new(:a)
  const_set(:Limit, 1) if false
  class Helper
    def run; Client.new.charge; end
  end
end
class User
  def go; Other::Client.new.charge; end
end
",
    )]);
    let names: Vec<&str> = rows
        .iter()
        .flat_map(|r| &r.symbols)
        .map(|s| s.name.as_str())
        .collect();
    for name in [
        "Other::Client",
        "Other::Client#charge",
        "Other::Store",
        "Other::Pay",
        "Other::Limit",
    ] {
        assert!(!names.contains(&name), "no symbol {name}");
    }
    assert_unresolved(
        &rows,
        &[
            ("Other::Helper#run", Call, 10, "Client.new.charge"),
            ("User#go", Call, 14, "Other::Client.new.charge"),
        ],
    );
}

/// `class Foo::Bar` in `module Outer` reopens the `Foo::Bar` whose `Foo`
/// Ruby's lexical lookup finds: the top-level one when `Outer` has none,
/// so its body sees `Foo::Bar::Baz`; `Inner::Foo::Bar` when the file gave
/// `Inner` a `Foo` before. When another file gives the module a `Foo`,
/// which one Ruby reopens depends on load order: nothing proves a site in
/// it, nor a call into it. Ruby 3.3: `Foo::Bar.new.y` calls
/// `Foo::Bar::Baz.go`, and `defined?(Outer::Foo)` is `nil`.
#[test]
fn c10_ruby_shadowing_a_compact_class_path_is_named_where_ruby_finds_its_head() {
    // D09
    let foo = (
        "lib/foo.rb",
        "class Baz
  def self.go; end
end
module Foo
  class Bar
    class Baz
      def self.go; end
    end
  end
end
",
    );
    let top = index(&[
        foo,
        (
            "lib/outer.rb",
            "module Outer\n  class Foo::Bar\n    def y; Baz.go; end\n  end\nend\n",
        ),
    ]);
    assert_eq!(
        resolved(&top),
        labelled(&[("Foo::Bar#y", Call, 3, "Foo::Bar::Baz.go")])
    );
    let inner = index(&[(
        "lib/inner.rb",
        "module Inner
  module Foo
  end
  class Foo::Bar
    def z; end
  end
  def self.go; Foo::Bar.new.z; end
end
",
    )]);
    assert_eq!(
        resolved(&inner),
        labelled(&[("Inner.go", Call, 7, "Inner::Foo::Bar#z")])
    );
    let ambiguous = index(&[
        foo,
        (
            "lib/amb.rb",
            "module Amb\n  class Foo::Bar\n    def w; Baz.go; end\n  end\nend\n",
        ),
        ("lib/amb_foo.rb", "module Amb\n  module Foo\n  end\nend\n"),
        (
            "lib/caller.rb",
            "class Caller\n  def run; Foo::Bar.new.w; end\nend\n",
        ),
    ]);
    assert_eq!(resolved(&ambiguous), labelled(&[]));
    assert_unresolved(
        &ambiguous,
        &[
            ("Foo::Bar#w", Call, 3, "Baz.go"),
            ("Caller#run", Call, 2, "Foo::Bar.new.w"),
        ],
    );
}

/// A compact `class Foo::Bar` that the nesting does not resolve finds
/// `Foo` through the ancestors of the innermost class, as this file
/// declares them: a module it includes, then its superclass. Ruby 3.3:
/// `Base::Foo::Bar.instance_methods(false)` is `[:w]` and the top-level
/// `Foo::Bar`'s `[]`; `K.go` is `:w`; `Inc::Foo::Bar` holds `v`.
#[test]
fn c10_ruby_shadowing_a_compact_class_path_is_found_through_the_ancestors() {
    // B10 (the review's `b03_compact_inherited`)
    let symbols = |rows: &[pixel_model::FileRows]| -> Vec<String> {
        rows.iter()
            .flat_map(|r| &r.symbols)
            .map(|s| s.name.clone())
            .collect()
    };
    let rows = index(&[(
        "lib/a.rb",
        "class Base; module Foo; class Bar; end; end; end
module Foo; class Bar; end; end
class K < Base
  class Foo::Bar
    def w; :w; end
  end
  def self.go; Foo::Bar.new.w; end
end
",
    )]);
    let names = symbols(&rows);
    assert!(names.contains(&"Base::Foo::Bar#w".to_owned()), "{names:?}");
    assert!(!names.contains(&"Foo::Bar#w".to_owned()), "{names:?}");
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("K", Reference, 3, "Base"),
            ("K.go", Call, 7, "Base::Foo::Bar#w"),
        ])
    );
    let included = index(&[(
        "lib/b.rb",
        "class Base; module Foo; class Bar; end; end; end
module Inc; module Foo; class Bar; end; end; end
class K2 < Base
  include Inc
  class Foo::Bar
    def v; :v; end
  end
end
",
    )]);
    let names = symbols(&included);
    assert!(names.contains(&"Inc::Foo::Bar#v".to_owned()), "{names:?}");
}

/// A region tree-sitter-ruby cannot parse (here an anonymous `,&` with no
/// blank before it, which #107's rewrite leaves) still defines what
/// Ruby reads there: `Admin::User` with `find`, a reopened `Post` with `save`. A
/// constant path through a name such a region may define, a lookup
/// through a class it may reopen, and a top-level function of a name it
/// may define prove nothing; a class it cannot touch keeps its edges
/// (`Other#initialize`, although the region defines an `initialize`).
/// Ruby 3.3: `ruby -c` accepts both broken files, and `Admin::Panel#x`
/// calls `Admin::User.find`.
#[test]
fn c10_ruby_shadowing_an_unread_region_proves_nothing_it_may_define() {
    // D12
    let rows = index(&[
        (
            "lib/admin_user.rb",
            "module Admin
  class User
    def initialize; end
    def self.find(&)
      lookup 1,&
    end
    def self.lookup(*); end
  end
end
",
        ),
        ("lib/user.rb", "class User\n  def self.find; end\nend\n"),
        ("lib/other.rb", "class Other\n  def initialize; end\nend\n"),
        (
            "lib/panel.rb",
            "module Admin
  class Panel
    def x
      User.find
      Other.new
    end
  end
end
",
        ),
        ("lib/helpers.rb", "def save; end\n"),
        (
            "lib/post.rb",
            "require_relative \"helpers\"\nclass Post\n  def run; save; end\nend\n",
        ),
        (
            "lib/post_ext.rb",
            "class Post
  def save(&)
    store 1,&
  end
  def self.store(*); end
end
",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Admin::Panel#x", Call, 5, "Other#initialize"),
            ("lib/post.rb", Import, 1, "lib/helpers.rb"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("Admin::Panel#x", Call, 4, "User.find"),
            ("Post#run", Call, 3, "save"),
        ],
    );
}

/// A region the grammar cannot read may define on a class it does not
/// open: through a constant receiver (`Base.prepend(..)`, which may add
/// any name to `Base`), in a block run elsewhere (`Module.new { def hook
/// }`), or on another object (`klass.define_method(:tag)`). A lookup that
/// passes `Base` proves nothing, and no class's `hook` or `tag` is a proven
/// target; a name the region defines in its own class body stays its
/// class's only. `ruby -c` accepts lib/patch.rb; Ruby 3.3: `main.rb`
/// prints `:patched` twice.
#[test]
fn c10_ruby_shadowing_an_unread_region_may_define_on_a_class_it_does_not_open() {
    // B9 (the review's `m05_unread_prepend`)
    let rows = index(&[
        (
            "lib/base.rb",
            "class Base; def hook; :base; end; end
",
        ),
        (
            "lib/k.rb",
            "require_relative \"base\"\nclass K < Base\n  def run; hook; end\nend\n",
        ),
        (
            "lib/patch.rb",
            "require_relative \"k\"
def link_to(*) = nil
def wrap(x, **opts, &)
  link_to x, **opts, data: {
    a: 1
  }.merge(opts),&
end
Base.prepend(Module.new { def hook; :patched; end })
def tagged(klass) = klass.define_method(:tag) { :t }
",
        ),
        (
            "lib/other.rb",
            "class Other; def hook; end; def tag; end; def label; end; end
class Mine; def run; [hook, tag, label]; end; def hook; end; def tag; end; def label; end; end
",
        ),
        (
            "lib/c.rb",
            "class C; def r; K.new.hook; Base.new.hook; end; end\n",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("lib/k.rb", Import, 1, "lib/base.rb"),
            ("lib/patch.rb", Import, 1, "lib/k.rb"),
            ("K", Reference, 2, "Base"),
            ("Mine#run", Call, 2, "Mine#label"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("K#run", Call, 3, "hook"),
            ("C#r", Call, 1, "K.new.hook"),
            ("C#r", Call, 1, "Base.new.hook"),
            ("Mine#run", Call, 2, "hook"),
            ("Mine#run", Call, 2, "tag"),
        ],
    );
}

/// `Foo.new.m` is an instance of `Foo` unless a `new` on Foo's singleton
/// chain, its own or a superclass's, returns something else; a `new` the
/// graph cannot see (a gem superclass's: Active Record builds the subclass a
/// `type` attribute names) may return an instance of a subclass, so every
/// subclass must agree. Ruby 3.3: `Widget.new.run` calls `Proxy#run`.
#[test]
fn c10_ruby_dynamic_dispatch_a_new_on_the_singleton_chain_proves_no_class() {
    // D07
    let rows = index(&[(
        "lib/factory.rb",
        "class Model
  def self.new(*args); Proxy.allocate; end
end
class Widget < Model
  def run; end
end
class Proxy
  def run; end
end
class Plain
  def run; end
end
class Shape < Plain
  def run; end
end
class Record < ActiveRecord::Base
  def label; end
end
class Special < Record
  def label; end
end
Widget.new.run
Plain.new.run
Record.new.label
Special.new.label
",
    )]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Widget", Reference, 4, "Model"),
            ("Shape", Reference, 13, "Plain"),
            ("Special", Reference, 19, "Record"),
            ("lib/factory.rb", Call, 22, "Model.new"),
            ("lib/factory.rb", Call, 23, "Plain#run"),
            ("lib/factory.rb", Call, 25, "Special#label"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("lib/factory.rb", Call, 22, "Widget.new.run"),
            ("lib/factory.rb", Call, 24, "Record.new.label"),
        ],
    );
}

/// The body of `class << self` is the singleton class's constant scope: a
/// class or constant assigned or opened there is its, never the owner's
/// (`Outer::X` raises NameError), and Ruby looks it up first from the
/// methods written there; past the nesting, constants come from the
/// singleton class's ancestors (the modules it extends), not from the
/// owner's. Ruby 3.3: `Outer.make` gives `:sing` and `:inner`,
/// `Outer.new.run` `:top`; `Job.new.a` `:base`, `Job.c` `:top`, `Job2.d`
/// `:ext`.
#[test]
fn c10_ruby_shadowing_a_constant_of_class_self_is_the_singleton_classs() {
    // R2
    let rows = index(&[
        (
            "lib/outer.rb",
            "class X; def m; end; end
class Outer
  class << self
    X = Class.new do
      def m; end
    end
    LIMIT = 1
    class Inner; def n; end; end
    def make; X.new.m; Inner.new.n; LIMIT.to_s; end
  end
  def run; X.new.m; end
end
class C; def r; Outer::X.new.m; Outer::Inner.new.n; end; end
",
        ),
        (
            "lib/job.rb",
            "class Retry; def self.go; end; end
class Base; class Retry; def self.go; end; end; end
module Ext; class Retry; def self.go; end; end; end
class Job < Base
  def a; Retry.go; end
  class << self
    def c; Retry.go; end
  end
end
class Job2 < Base
  extend Ext
  class << self
    def d; Retry.go; end
  end
end
",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Outer.make", Call, 9, "#<Class:Outer>::Inner#n"),
            ("Outer#run", Call, 11, "X#m"),
            ("Job", Reference, 4, "Base"),
            ("Job#a", Call, 5, "Base::Retry.go"),
            ("Job.c", Call, 7, "Retry.go"),
            ("Job2", Reference, 10, "Base"),
            ("Job2", Reference, 11, "Ext"),
            ("Job2.d", Call, 13, "Ext::Retry.go"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("C#r", Call, 13, "Outer::X.new.m"),
            ("C#r", Call, 13, "Outer::Inner.new.n"),
            ("Outer.make", Call, 9, "LIMIT.to_s"),
            // A `Class.new` (R12-03).
            ("Outer.make", Call, 9, "X.new.m"),
        ],
    );
}

/// `self::X = v` and `const_set(:X, v)` in a class body assign the class's
/// own constant, which shadows the top-level class of that name. Ruby 3.3:
/// both calls in `Billing.charge` raise NoMethodError on the Struct
/// classes.
#[test]
fn c10_ruby_shadowing_a_constant_set_on_self_shadows_the_top_level() {
    // R8
    let rows = index(&[
        (
            "lib/client.rb",
            "class Client; def self.connect; end; end\nclass Gw; def self.connect; end; end\n",
        ),
        (
            "lib/billing.rb",
            "class Billing
  self::Client = Struct.new(:k)
  const_set(:Gw, Struct.new(:k))
  def self.charge; Client.connect; Gw.connect; end
end
",
        ),
    ]);
    assert_eq!(resolved(&rows), labelled(&[]));
    assert_unresolved(
        &rows,
        &[
            ("Billing.charge", Call, 4, "Client.connect"),
            ("Billing.charge", Call, 4, "Gw.connect"),
        ],
    );
}

/// In the block of `Module.new`, `Struct.new` or `Data.define`, `self` is
/// the new class or module: a constant set on it, by `self::X = v` or a
/// called or sent `const_set`, is that class's, never the scope around's,
/// and shadows nothing there (R9-02). Ruby 3.3: `[Billing.charge,
/// Gw.connect]` is `[[:client, :gw], :gw]`, and `Billing.constants` is
/// `[:Box, :Row]`.
#[test]
fn c10_ruby_shadowing_a_constant_set_on_a_new_class_stays_in_it() {
    // R9-02
    let rows = index(&[
        (
            "lib/client.rb",
            "class Client; def self.connect; end; end\nclass Gw; def self.connect; end; end\n",
        ),
        (
            "lib/billing.rb",
            "class Billing
  Box = Module.new do
    self::Client = 1
    const_set(:Gw, 2)
  end
  Row = Struct.new(:k) do
    send(:const_set, :Client, 3)
  end
  def self.charge; [Client.connect, Gw.connect]; end
end
P = Data.define(:a) do
  self::Gw = 4
end
Gw.connect
",
        ),
    ]);
    let constants: Vec<&str> = rows
        .iter()
        .flat_map(|file| &file.symbols)
        .filter(|symbol| symbol.kind == SymbolKind::Const)
        .map(|symbol| symbol.name.as_str())
        .collect();
    assert_eq!(constants, Vec::<&str>::new());
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Billing.charge", Call, 9, "Client.connect"),
            ("Billing.charge", Call, 9, "Gw.connect"),
            ("lib/billing.rb", Call, 14, "Gw.connect"),
        ])
    );
}

/// A method a bare `private` or `protected` precedes, or one `private` or
/// `private_class_method` names, is no target of a call on a constant
/// receiver, whatever finds it: `extend self`, an `attr_reader *NAMES`, a
/// `define_method`, a Struct member, a plain `def`, `class << self`. A
/// call without receiver still reaches it (R9-03). Ruby 3.3: every call of
/// `C#bad` raises NoMethodError, every one of `C#ok` runs; those through
/// `extend self` or a `Struct.new` abstain as on main (R12-03).
#[test]
fn c10_ruby_a_private_method_is_no_target_of_a_constant_receiver() {
    // R9-03
    let rows = index(&[(
        "lib/k.rb",
        "module G
  extend self
  def pub; r; end
  def self.spub; r; end
  private
  def r; end
end
class K
  ATTRS = %i[a]
  def ia; [a, dm, priv]; end
  def self.s; end
  private
  attr_reader *ATTRS
  define_method(:dm) { }
  def priv; end
  class << self
    private
    def hid; end
  end
end
P = Struct.new(:z) do
  def iz; z; end
  private :z
end
class C
  def bad; [G.r, K.new.a, K.new.dm, P.new(1).z, K.new.priv, K.hid]; end
  def ok; [G.pub, G.spub, K.new.ia, K.s, P.new(1).iz]; end
end
",
    )]);
    assert_unresolved(
        &rows,
        &[
            ("C#bad", Call, 26, "G.r"),
            ("C#bad", Call, 26, "K.new.a"),
            ("C#bad", Call, 26, "K.new.dm"),
            ("C#bad", Call, 26, "P.new.z"),
            ("C#bad", Call, 26, "K.new.priv"),
            ("C#bad", Call, 26, "K.hid"),
        ],
    );
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("G", Reference, 2, "G"),
            ("G#pub", Call, 3, "G#r"),
            ("G.spub", Call, 4, "G#r"),
            ("K#ia", Call, 10, "K#a"),
            ("K#ia", Call, 10, "K#dm"),
            ("K#ia", Call, 10, "K#priv"),
            ("P#iz", Call, 22, "P#z"),
            ("C#ok", Call, 27, "G.spub"),
            ("C#ok", Call, 27, "K#ia"),
            ("C#ok", Call, 27, "K.s"),
        ])
    );
    assert_unresolved(
        &rows,
        &[("C#ok", Call, 27, "G.pub"), ("C#ok", Call, 27, "P.new.iz")],
    );
}

/// The private copies a bare `module_function` makes, of a `def` or an
/// `attr_reader`, are no target of a call on a constant receiver through
/// an includer or an extender (R10-04). The module's own copy, and a call
/// without receiver, still reach theirs. Ruby 3.3: every call of `C#bad`
/// raises NoMethodError, `C#ok` is `[:util, :util]`.
#[test]
fn c10_ruby_a_private_copy_of_a_module_function_is_no_target() {
    // R10-04
    let rows = index(&[(
        "lib/k.rb",
        "module Util
  module_function
  def helper; :util; end
  attr_reader :at
end
class K
  include Util
  extend Util
  def inside; helper; end
end
class C
  def bad; [K.new.helper, K.helper, K.new.at]; end
  def ok; [Util.helper, K.new.inside]; end
end
",
    )]);
    assert_unresolved(
        &rows,
        &[
            ("C#bad", Call, 12, "K.new.helper"),
            ("C#bad", Call, 12, "K.helper"),
            ("C#bad", Call, 12, "K.new.at"),
        ],
    );
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("K", Reference, 7, "Util"),
            ("K", Reference, 8, "Util"),
            ("K#inside", Call, 9, "Util#helper"),
            ("C#ok", Call, 13, "Util.helper"),
            ("C#ok", Call, 13, "K#inside"),
        ])
    );
}

/// An inherited method a step of the receiver's chain before its own
/// makes private or protected is no target of a call on a constant
/// receiver: `private :pub` in a class `Class.new(Base) do` or `class
/// Sub2 < Base` makes gives it a private copy there (R10-05). A call
/// without receiver still reaches it. Ruby 3.3: every call of `C#bad`
/// raises NoMethodError, `C#ok` is `[:pub, :pub]`.
#[test]
fn c10_ruby_an_inherited_method_made_private_is_no_target() {
    // R10-05
    let rows = index(&[(
        "lib/k.rb",
        "class Base
  def pub; :pub; end
  def prot; :prot; end
end
Sub = Class.new(Base) do
  private :pub
  protected :prot
end
class Sub2 < Base
  private :pub
  def mine; pub; end
end
class C
  def bad; [Sub.new.pub, Sub.new.prot, Sub2.new.pub]; end
  def ok; [Base.new.pub, Sub2.new.mine]; end
end
",
    )]);
    assert_unresolved(
        &rows,
        &[
            ("C#bad", Call, 14, "Sub.new.pub"),
            ("C#bad", Call, 14, "Sub.new.prot"),
            ("C#bad", Call, 14, "Sub2.new.pub"),
        ],
    );
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Sub", Reference, 5, "Base"),
            ("Sub2", Reference, 9, "Base"),
            ("Sub2#mine", Call, 11, "Base#pub"),
            ("C#ok", Call, 15, "Base#pub"),
            ("C#ok", Call, 15, "Sub2#mine"),
        ])
    );
}

/// A constant assigned a constructor's class or module twice holds the
/// second (Ruby warns of an already initialized constant), which the
/// graph would merge with the first as if reopened: nothing is proven
/// through it, whether both are in one file or the two are test files
/// each with its own `Point` (R10-06). One assigned once and then
/// reopened keeps the methods of the body its keyword opens, which main
/// holds too; a call reaching what the constructor defines abstains as on
/// main (R12-03). Ruby 3.3, loading lib/k.rb:
/// `[:base, [:nomethod], 1, :dist]`.
#[test]
fn c10_ruby_shadowing_a_constant_assigned_a_new_class_twice_names_neither() {
    // R10-06
    let rows = index(&[
        (
            "lib/k.rb",
            "class Base; def a; :base; end; end
X = Class.new(Base) { def a; :first; end }
X = Class.new(Base)
M = Module.new { def m; :first; end }
M = Module.new
Q = Struct.new(:q)
class Q; def dist; :dist; end; end
class Host; include M; def run; [(m rescue :nomethod)]; end; end
class C
  def run; [X.new.a, Host.new.run, Q.new(1).q, Q.new(1).dist]; end
end
",
        ),
        ("test/a_test.rb", "Point = Struct.new(:x)\n"),
        (
            "test/b_test.rb",
            "Point = Struct.new(:lat)\nclass BTest; def t; Point.new(1).lat; end; end\n",
        ),
    ]);
    assert_unresolved(
        &rows,
        &[
            ("C#run", Call, 10, "X.new.a"),
            ("Host#run", Call, 8, "m"),
            ("BTest#t", Call, 2, "Point.new.lat"),
            ("C#run", Call, 10, "Q.new.q"),
        ],
    );
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("C#run", Call, 10, "Host#run"),
            ("C#run", Call, 10, "Q#dist"),
        ])
    );
}

/// `module_function :m` makes the instance method `m` private too: no call
/// on a constant receiver reaches it through an includer, a plain class or
/// one `Class.new` or `Data.define` makes (R10-04). Ruby 3.3: every call
/// of `C#bad` raises NoMethodError, `C#ok` is `:util2`.
#[test]
fn c10_ruby_a_method_module_function_names_is_no_target_through_an_includer() {
    // R10-04
    let rows = index(&[(
        "lib/k.rb",
        "module Util2
  def helper2; :util2; end
  module_function :helper2
end
class PlainK
  include Util2
end
NewK = Class.new do
  include Util2
end
DK = Data.define(:z) do
  include Util2
end
class C
  def bad; [PlainK.new.helper2, NewK.new.helper2, DK.new(z: 1).helper2]; end
  def ok; Util2.helper2; end
end
",
    )]);
    assert_unresolved(
        &rows,
        &[
            ("C#bad", Call, 15, "PlainK.new.helper2"),
            ("C#bad", Call, 15, "NewK.new.helper2"),
            ("C#bad", Call, 15, "DK.new.helper2"),
        ],
    );
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("PlainK", Reference, 6, "Util2"),
            ("NewK", Reference, 9, "Util2"),
            ("DK", Reference, 12, "Util2"),
            ("C#ok", Call, 16, "Util2.helper2"),
        ])
    );
}

/// A step that may not be there may hold the method first (`prepend M0 if
/// OFF` in `M1`, B4): the steps before its other place are checked too,
/// where `M1` makes it private (R10-05). Ruby 3.3: both calls raise
/// NoMethodError.
#[test]
fn c10_ruby_a_method_made_private_behind_a_step_that_may_not_be_there_is_no_target() {
    // R10-05
    let rows = index(&[(
        "lib/k.rb",
        "OFF = false
module M0; def a; :m0; end; end
module M1
  prepend M0 if OFF
  include M0
end
module M1; private :a; end
K = Class.new do
  include M0
  prepend M1
end
class P
  include M0
  prepend M1
end
class C; def run; [K.new.a, P.new.a]; end; end
",
    )]);
    assert_unresolved(
        &rows,
        &[
            ("C#run", Call, 16, "K.new.a"),
            ("C#run", Call, 16, "P.new.a"),
        ],
    );
}

/// An alias copies the visibility its original has when it runs: it is
/// proven public only for an original defined above it in the same body
/// that nothing makes private or protected, so a call on a constant
/// receiver landing on any other alias abstains, in a class `Class.new`
/// makes as in a plain one (R11-03, F11-01). `Early#c` is public in Ruby,
/// but `private :a` anywhere leaves it unproven. Ruby 3.3: `C#bad` is
/// `[:nm, :nm]`, `C#ok` `[:a, :a]`.
#[test]
fn c10_ruby_an_alias_of_a_method_not_proven_public_is_no_target() {
    // R11-03
    let rows = index(&[(
        "lib/k.rb",
        "K = Class.new do
  def a; :a; end
  private :a
  alias_method :c, :a
end
class Plain
  private
  def a; :a; end
  alias c a
end
class Early
  def a; :a; end
  alias_method :c, :a
  private :a
end
class Pub
  def a; :a; end
  alias_method :c, :a
end
class C
  def bad; [(K.new.c rescue :nm), (Plain.new.c rescue :nm)]; end
  def ok; [Early.new.c, Pub.new.c]; end
end
",
    )]);
    assert_unresolved(
        &rows,
        &[
            ("C#bad", Call, 21, "K.new.c"),
            ("C#bad", Call, 21, "Plain.new.c"),
            ("C#ok", Call, 22, "Early.new.c"),
        ],
    );
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("K#c", Reference, 4, "K#a"),
            ("Plain#c", Reference, 9, "Plain#a"),
            ("Early#c", Reference, 13, "Early#a"),
            ("Pub#c", Reference, 18, "Pub#a"),
            ("C#ok", Call, 22, "Pub#c"),
        ])
    );
}

/// A visibility change the rules do not read (`send(:private, :a)` in the
/// body or from outside, `class_eval { private :a }`,
/// `private_class_method` from outside, `private :s` in `class << P5`)
/// leaves the methods it names unproven for a call on a constant receiver
/// on that class or module, a plain one as one `Class.new` makes or one
/// that extends itself (R11-03, F11-01). Ruby 3.3: `C#bad` is all
/// `:nm`, `C#ok` `[:a, :s]`.
#[test]
fn c10_ruby_a_visibility_change_the_rules_do_not_read_proves_no_target() {
    // R11-03
    let rows = index(&[(
        "lib/k.rb",
        "class P1
  def a; :a; end
  send(:private, :a)
end
class P2
  def a; :a; end
end
P2.send(:private, :a)
K3 = Class.new do
  def a; :a; end
end
K3.class_eval { private :a }
class P4
  def self.s; :s; end
end
P4.private_class_method(:s)
class P5
  def self.s; :s; end
end
class << P5
  private :s
end
module U6
  extend self
  def a; :a; end
  __send__(:private, :a)
end
class Q
  def a; :a; end
  def self.s; :s; end
end
class C
  def bad; [(P1.new.a rescue :nm), (P2.new.a rescue :nm), (K3.new.a rescue :nm), (P4.s rescue :nm), (P5.s rescue :nm), (U6.a rescue :nm)]; end
  def ok; [Q.new.a, Q.s]; end
end
",
    )]);
    assert_unresolved(
        &rows,
        &[
            ("C#bad", Call, 33, "P1.new.a"),
            ("C#bad", Call, 33, "P2.new.a"),
            ("C#bad", Call, 33, "K3.new.a"),
            ("C#bad", Call, 33, "P4.s"),
            ("C#bad", Call, 33, "P5.s"),
            ("C#bad", Call, 33, "U6.a"),
        ],
    );
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("U6", Reference, 24, "U6"),
            ("C#ok", Call, 34, "Q#a"),
            ("C#ok", Call, 34, "Q.s"),
        ])
    );
}

/// What main does not hold, a class or module a constructor made and an
/// `extend self` module's own methods on itself, is no target of a call on
/// a constant receiver: the ways to change visibility there are more than
/// the rules read (a computed or splatted `send`, `method(:private)`, a
/// constant aliasing the class, a bare word under a condition). The
/// module's `def self.s` and the method a keyword's body gives a
/// constructed constant (`Q#dist`) are main's too, and still found
/// (R12-03). Ruby 3.3: `C#bad` is all `:nm`, `C#ok` `[:s, :dist]`.
#[test]
fn c10_ruby_what_main_does_not_hold_is_no_target_of_a_constant_receiver() {
    // R12-03
    let rows = index(&[(
        "lib/k.rb",
        "VIS = :private
ON = true
K1 = Class.new do
  def a; :a; end
end
K1.send(VIS, :a)
K2 = Struct.new(:x) do
  def a; :a; end
end
K2.send(*[:private, :a])
K3 = Data.define(:x) do
  def a; :a; end
  method(:private).call(:a)
end
K4 = Class.new do
  def a; :a; end
end
K4A = K4
K4A.send(:private, :a)
K5 = Class.new do
  ON ? private : public
  def a; :a; end
end
module U
  extend self
  if ON then private else public end
  def a; :a; end
  def self.s; :s; end
end
Q = Struct.new(:q)
class Q
  def dist; :dist; end
end
class C
  def bad; [(K1.new.a rescue :nm), (K2.new(1).a rescue :nm), (K3.new(x: 1).a rescue :nm), (K4.new.a rescue :nm), (K5.new.a rescue :nm), (U.a rescue :nm)]; end
  def ok; [U.s, Q.new(1).dist]; end
end
",
    )]);
    assert_unresolved(
        &rows,
        &[
            ("C#bad", Call, 35, "K1.new.a"),
            ("C#bad", Call, 35, "K2.new.a"),
            ("C#bad", Call, 35, "K3.new.a"),
            ("C#bad", Call, 35, "K4.new.a"),
            ("C#bad", Call, 35, "K5.new.a"),
            ("C#bad", Call, 35, "U.a"),
        ],
    );
    let calls: std::collections::BTreeSet<_> = resolved(&rows)
        .into_iter()
        .filter(|row| row.1 == Call)
        .collect();
    assert_eq!(
        calls,
        labelled(&[("C#ok", Call, 36, "U.s"), ("C#ok", Call, 36, "Q#dist")])
    );
}
