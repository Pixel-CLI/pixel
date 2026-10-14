// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! A method defined on one object's singleton class reaches the objects
//! that may be that object, and no other (#106): lookups from them prove
//! nothing of its name, every other lookup still resolves (C-10).

use mvp_model::EdgeKind::Call;

use super::{assert_unresolved, index, resolved};

/// The class of #106's reproduction: an instance method of `Maker`
/// defines a method of a computed name on the singleton class of the
/// `Maker` it runs on.
const MAKER: &str = "class Maker
  def build(n)
    define_singleton_method(n) { 1 }
  end
end
";

/// #106: one `define_singleton_method(n)` in an instance method of
/// `Maker` took every Ruby edge of the repository (0 call edges on
/// gumroad). It lands on a `Maker`, whose lookup a `Greeter` never makes:
/// `Greeter#hello` still calls `Greeter#helper`. Ruby 3.3:
/// `Maker.new.build(:helper)` leaves `Greeter.new.hello` at `Greeter#helper`.
#[test]
fn c10_ruby_dynamic_dispatch_a_singleton_method_of_an_instance_leaves_other_classes_resolved() {
    // #106
    let rows = index(&[
        (
            "app/a.rb",
            "class Greeter
  def hello
    helper
  end

  def helper
    1
  end
end
",
        ),
        ("app/b.rb", MAKER),
    ]);
    assert!(
        resolved(&rows).contains(&(
            "Greeter#hello".to_owned(),
            Call,
            3,
            "Greeter#helper".to_owned()
        )),
        "{:#?}",
        resolved(&rows)
    );
}

/// The same with a literal name: only the objects that may be a `Maker`
/// lose `ping`; a `Greeter` keeps its own.
#[test]
fn c10_ruby_dynamic_dispatch_a_named_singleton_method_of_an_instance_leaves_other_classes_resolved()
{
    // #106
    let rows = index(&[
        (
            "app/a.rb",
            "class Greeter
  def hello; ping; end
  def ping; end
end
",
        ),
        (
            "app/b.rb",
            "class Maker
  def build; define_singleton_method(:ping) { 1 }; end
  def run; ping; end
  def ping; end
end
",
        ),
    ]);
    let edges = resolved(&rows);
    assert!(
        edges.contains(&(
            "Greeter#hello".to_owned(),
            Call,
            2,
            "Greeter#ping".to_owned()
        )),
        "{edges:#?}"
    );
    assert_unresolved(&rows, &[("Maker#run", Call, 3, "ping")]);
}

/// What the singleton method may shadow still proves nothing: a call on
/// `self` that may run on a `Maker`, in `Maker` itself, in a subclass,
/// in a superclass or a module it includes (whose methods run on a
/// `Maker` too), in a class including a module whose method defines it,
/// on a class extending such a module (and through a constant receiver),
/// and in a top-level function, which runs on whatever object calls it.
/// Ruby 3.3: after `m = Sub.new; m.build(:h1)` (and so on for each name),
/// each of these calls reaches the singleton method, not the `def`.
#[test]
fn c10_ruby_dynamic_dispatch_what_a_singleton_method_of_an_instance_may_shadow_proves_nothing() {
    // #106
    let rows = index(&[
        (
            "app/maker.rb",
            "module Mix
  def mixed; h4; end
  def h4; end
end
class Base
  def up; h3; end
  def h3; end
end
class Maker < Base
  include Mix
  def build(n)
    define_singleton_method(n) { 1 }
  end
  def run; h1; end
  def h1; end
end
class Sub < Maker
  def down; h2; end
  def h2; end
end
",
        ),
        (
            "app/builder.rb",
            "module Builder
  def build(n)
    define_singleton_method(n) { 1 }
  end
end
class Kit
  include Builder
  def a; h5; end
  def h5; end
end
class Ext
  extend Builder
  def self.a; h6; end
  def self.h6; end
end
class Caller
  def call; Ext.h6; end
end
",
        ),
        (
            "app/helpers.rb",
            "def tool; h7; end
def h7; end
",
        ),
    ]);
    assert_unresolved(
        &rows,
        &[
            ("Maker#run", Call, 14, "h1"),
            ("Sub#down", Call, 18, "h2"),
            ("Base#up", Call, 6, "h3"),
            ("Mix#mixed", Call, 2, "h4"),
            ("Kit#a", Call, 8, "h5"),
            ("Ext.a", Call, 13, "h6"),
            ("Caller#call", Call, 17, "Ext.h6"),
            ("tool", Call, 1, "h7"),
        ],
    );
}

/// A class beside them, whose chain holds none of their steps, keeps every
/// edge, its superclass's and a module's included as well: a `Base` the
/// `Maker` shares with it only loses the calls that may run on a `Maker`.
/// Ruby 3.3: `Other.new.go` is `[:other, :base]` after any `build`.
#[test]
fn c10_ruby_dynamic_dispatch_a_singleton_method_of_an_instance_leaves_a_sibling_resolved() {
    // #106
    let rows = index(&[(
        "app/k.rb",
        "class Base
  def shared; end
end
module Kept
  def kept; end
end
class Maker < Base
  def build(n)
    define_singleton_method(n) { 1 }
  end
end
class Other < Base
  include Kept
  def go; [own, shared, kept]; end
  def own; end
end
",
    )]);
    let edges = resolved(&rows);
    for target in ["Other#own", "Base#shared", "Kept#kept"] {
        assert!(
            edges.contains(&("Other#go".to_owned(), Call, 14, target.to_owned())),
            "{target}: {edges:#?}"
        );
    }
}

/// Subclasses that share their superclass's chain are read together
/// (R10-01): one of them whose own list holds a module defining on the
/// singleton class of its instances is still read alone, so the call in
/// the superclass that may run on it proves nothing, while a superclass
/// none of whose objects can run such a method keeps its edge. Ruby 3.3:
/// `a = A.new; a.build(:foo); a.go` is the singleton method's value.
#[test]
fn c10_ruby_dynamic_dispatch_a_subclass_sharing_a_chain_still_reaches_the_singleton_method() {
    // #106
    let rows = index(&[(
        "app/k.rb",
        "module Builder
  def build(n)
    define_singleton_method(n) { 1 }
  end
end
class Base
  def go; foo; end
  def foo; end
end
class A < Base
  include Builder
end
class B < Base; end
class C < Base; end
class Plain
  def go; foo; end
  def foo; end
end
class P1 < Plain; end
class P2 < Plain; end
",
    )]);
    assert_unresolved(&rows, &[("Base#go", Call, 7, "foo")]);
    let edges = resolved(&rows);
    assert!(
        edges.contains(&("Plain#go".to_owned(), Call, 16, "Plain#foo".to_owned())),
        "{edges:#?}"
    );
}

/// gumroad's `PlatformBlock`: `TYPES.each_value do |t| define_method("#{t}?")`
/// in a class body. A core collection's iterator yields to its block in
/// place, so the methods are the class's own, of names the rules cannot
/// read: a lookup through the class proves nothing of a name they may
/// take, and every other class keeps its edges (#106). On a receiver that
/// is neither a literal nor a constant (`list.each_value`, which may be an
/// object of the application evaluating the block elsewhere) the method
/// still lands anywhere. Ruby 3.3: `Block.new.check` is the generated
/// `ip?`, and `Other.new.go` `Other#ok?`.
#[test]
fn c10_ruby_dynamic_dispatch_a_definer_in_a_collection_iterator_is_the_classs() {
    // #106
    let rows = index(&[(
        "app/models/block.rb",
        "class Block
  TYPES = { ip: \"ip\", email: \"email\" }.freeze
  TYPES.each_value do |t|
    define_method(\"#{t}?\") { true }
  end
  def check; ip?; end
  def ip?; false; end
end
class Other
  def go; ok?; end
  def ok?; end
end
",
    )]);
    assert_unresolved(&rows, &[("Block#check", Call, 6, "ip?")]);
    let edges = resolved(&rows);
    assert!(
        edges.contains(&("Other#go".to_owned(), Call, 10, "Other#ok?".to_owned())),
        "{edges:#?}"
    );
    let rows = index(&[(
        "app/models/block.rb",
        "class Block
  def self.list; end
  list.each_value do |t|
    define_method(\"#{t}?\") { true }
  end
end
class Other
  def go; ok?; end
  def ok?; end
end
",
    )]);
    assert_unresolved(&rows, &[("Other#go", Call, 8, "ok?")]);
}

/// The application's `Link`, and a test that stubs a method of a computed
/// name on a `Link` the rules cannot name (gumroad's `link_test.rb`:
/// `product.define_singleton_method(m) { true }`).
const LINK: &str = "class Link
  def publish; save_it; end
  def save_it; end
end
";
const LINK_TEST: &str = "class LinkTest
  def stub_all(product, names)
    names.each { |m| product.define_singleton_method(m) { true } }
  end
  def test_publish; Link.new.save_it; end
end
";

/// A test file's definition on an object the rules cannot name is a stub,
/// as `allow(x).to receive(:m)` and `x.stubs(:m)` are, which the rules do
/// not read either: only a test runner loads the file, so the
/// application's callers keep their edges, and the test's own callers,
/// which may run on a stubbed object, prove nothing (#106). Ruby 3.3:
/// outside the test runner `Link.new.publish` calls `Link#save_it`.
#[test]
fn c10_ruby_dynamic_dispatch_a_stub_in_a_test_file_leaves_application_callers_resolved() {
    // #106
    for test in ["test/models/link_test.rb", "spec/models/link_spec.rb"] {
        let rows = index(&[("app/models/link.rb", LINK), (test, LINK_TEST)]);
        let edges = resolved(&rows);
        assert!(
            edges.contains(&(
                "Link#publish".to_owned(),
                Call,
                2,
                "Link#save_it".to_owned()
            )),
            "{test}: {edges:#?}"
        );
        assert_unresolved(
            &rows,
            &[("LinkTest#test_publish", Call, 5, "Link.new.save_it")],
        );
    }
}

/// The same definition in the application's own code reaches every
/// object, its callers' included: the boundary is the test file, not the
/// form (R5-04).
#[test]
fn c10_ruby_dynamic_dispatch_a_stub_in_application_code_still_proves_nothing() {
    // #106
    let rows = index(&[
        ("app/models/link.rb", LINK),
        ("app/lib/stubber.rb", LINK_TEST),
    ]);
    assert_unresolved(&rows, &[("Link#publish", Call, 2, "save_it")]);
}

/// A support file that reopens a named class of the application is no
/// stub: its definition is the class's, read as any other file's, so the
/// application's call, which two files' definitions may answer, proves
/// nothing.
#[test]
fn c10_ruby_dynamic_dispatch_a_test_file_reopening_a_named_class_is_read() {
    // #106
    let rows = index(&[
        ("app/models/link.rb", LINK),
        (
            "spec/support/link_patch.rb",
            "class Link\n  def save_it; :patched; end\nend\n",
        ),
    ]);
    assert_unresolved(&rows, &[("Link#publish", Call, 2, "save_it")]);
}

/// So is a module a test file mixes into an object the rules cannot name
/// (`report.extend(Fmt)`, as gumroad's specs do with their helpers): the
/// application's `Report#run` still calls `Fmt#fmt`, and the test's own
/// call proves nothing (#106).
#[test]
fn c10_ruby_dynamic_dispatch_a_module_a_test_file_mixes_in_leaves_the_application_resolved() {
    // #106
    let rows = index(&[
        (
            "app/models/report.rb",
            "module Fmt
  def fmt; end
end
class Report
  include Fmt
  def run; fmt; end
end
",
        ),
        (
            "spec/models/report_spec.rb",
            "class ReportSpec
  def stub(report); report.extend(Fmt); end
  def check; Report.new.fmt; end
end
",
        ),
    ]);
    let edges = resolved(&rows);
    assert!(
        edges.contains(&("Report#run".to_owned(), Call, 6, "Fmt#fmt".to_owned())),
        "{edges:#?}"
    );
    assert_unresolved(&rows, &[("ReportSpec#check", Call, 3, "Report.new.fmt")]);
}

/// What a test file is: one only a test runner loads.
#[test]
fn a_test_file_is_under_spec_or_test_or_named_so() {
    use crate::ruby::resolve::test_file;
    for path in [
        "spec/models/link_spec.rb",
        "spec/support/helpers.rb",
        "test/test_helper.rb",
        "components/billing/spec/x.rb",
        "lib/link_test.rb",
        "lib/link_spec.rb",
    ] {
        assert!(test_file(path), "{path}");
    }
    for path in [
        "app/models/test.rb",
        "lib/testing/x.rb",
        "app/specs/x.rb",
        "spec.rb",
    ] {
        assert!(!test_file(path), "{path}");
    }
}
