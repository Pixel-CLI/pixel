// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! How far a stub written in a test file reaches (ADR-0039): an example
//! file's stubs reach its own callers and those of shared test code;
//! shared test code's reach every test caller (#106).

use mvp_model::EdgeKind::Call;
use mvp_model::FileRows;

use super::{assert_unresolved, index, resolved};

const LINK: (&str, &str) = (
    "app/models/link.rb",
    "class Link
  def publish; save_it; end
  def save_it; end
  def help; end
end
",
);

/// An example file that stubs every name on a `Link` the rules cannot name
/// (gumroad's `link_test.rb`).
const STUBBING: (&str, &str) = (
    "test/models/link_test.rb",
    "class LinkTest < BaseTest
  def stub_all(product, names)
    names.each { |m| product.define_singleton_method(m) { true } }
  end
  def test_publish; Link.new.save_it; end
end
",
);

/// Another example file, which stubs nothing.
const OTHER: (&str, &str) = (
    "test/models/other_test.rb",
    "class OtherTest
  def test_save; Link.new.save_it; end
end
",
);

fn has(rows: &[FileRows], src: &str, line: u32, dst: &str) -> bool {
    resolved(rows).contains(&(src.to_owned(), Call, line, dst.to_owned()))
}

/// Another example file's stub never runs on the objects of this one: its
/// callers resolve, the stubbing file's own still prove nothing, and so do
/// the callers of shared test code, which may run on the stubbing file's
/// objects.
#[test]
fn c10_ruby_a_stub_in_an_example_file_reaches_that_file_and_shared_test_code() {
    let rows = index(&[
        LINK,
        STUBBING,
        OTHER,
        (
            "test/test_helper.rb",
            "class Helper\n  def check; Link.new.save_it; end\nend\n",
        ),
        ("test/support/base_test.rb", "class BaseTest\nend\n"),
    ]);
    assert!(has(&rows, "OtherTest#test_save", 2, "Link#save_it"));
    assert!(has(&rows, "Link#publish", 2, "Link#save_it"));
    // A stub changes which method a name finds, not which class a
    // constant names.
    let superclass = (
        "LinkTest".to_owned(),
        mvp_model::EdgeKind::Reference,
        1,
        "BaseTest".to_owned(),
    );
    assert!(resolved(&rows).contains(&superclass));
    assert_unresolved(
        &rows,
        &[
            ("LinkTest#test_publish", Call, 5, "Link.new.save_it"),
            ("Helper#check", Call, 2, "Link.new.save_it"),
        ],
    );
}

/// A stub reaches every test caller where the file holding it is not an
/// example file, or where other files' code may run on its objects: shared
/// test code (`test_helper.rb`), an example file another file requires,
/// one declaring RSpec shared examples other files' groups may include
/// (at the top level, sent to `RSpec` in a group, in a block evaluated on
/// an object the model does not name, or one beside another declared in
/// it on the same line), one
/// whose class another file subclasses (its `setup` runs for the
/// subclass).
#[test]
fn c10_ruby_a_stub_other_files_may_run_reaches_every_test_caller() {
    let shared = (
        "test/test_helper.rb",
        "def stub_all(product, names)\n  names.each { |m| product.define_singleton_method(m) { true } }\nend\n",
    );
    let required = (
        "test/models/sub_test.rb",
        "require_relative \"link_test\"\nclass SubTest\nend\n",
    );
    let stub = "it { |p| p.define_singleton_method(p.name) { 1 } }";
    let top =
        format!("shared_examples \"stubbed\" do\n  {stub}\nend\nRSpec.describe Link do\nend\n");
    let sent = format!(
        "RSpec.describe Link do\n  RSpec.shared_examples \"stubbed\" do\n    {stub}\n  end\nend\n"
    );
    let elsewhere =
        format!("Class.new do\n  shared_examples \"stubbed\" do\n    {stub}\n  end\nend\n");
    let one_line = format!("shared_examples \"a\" do shared_examples \"b\" do {stub} end end\n");
    let examples: Vec<(&str, &str)> = [&top, &sent, &elsewhere, &one_line]
        .into_iter()
        .map(|code| ("spec/models/link_spec.rb", code.as_str()))
        .collect();
    let subclassed = ("test/models/sub_test.rb", "class SubTest < LinkTest\nend\n");
    let mut cases = vec![
        vec![LINK, OTHER, shared],
        vec![LINK, OTHER, STUBBING, required],
        vec![LINK, OTHER, STUBBING, subclassed],
    ];
    cases.extend(
        examples
            .into_iter()
            .map(|examples| vec![LINK, OTHER, examples]),
    );
    for files in cases {
        let rows = index(&files);
        assert_unresolved(
            &rows,
            &[("OtherTest#test_save", Call, 2, "Link.new.save_it")],
        );
    }
}

/// A module an example file mixes into an object the rules cannot name
/// may answer that file's calls of the names it defines; the file's other
/// calls, and the other example files', resolve. A module a class of the
/// graph mixes in may change the chains themselves: every stub then,
/// another example file's included.
#[test]
fn c10_ruby_a_module_an_example_file_mixes_in_reaches_its_names_there() {
    let helper = (
        "spec/support/helpful.rb",
        "module Helpful\n  def help; end\nend\n",
    );
    let mixing = (
        "spec/models/mixing_spec.rb",
        "class MixingSpec
  def mix(x); x.extend(Helpful); end
  def save; Link.new.save_it; end
  def help; Link.new.help; end
end
",
    );
    let rows = index(&[LINK, OTHER, STUBBING, helper, mixing]);
    assert!(has(&rows, "MixingSpec#save", 3, "Link#save_it"));
    assert!(has(&rows, "OtherTest#test_save", 2, "Link#save_it"));
    assert_unresolved(&rows, &[("MixingSpec#help", Call, 4, "Link.new.help")]);
    let held = (
        "app/models/report.rb",
        "class Report\n  include Helpful\nend\n",
    );
    let rows = index(&[LINK, OTHER, STUBBING, helper, mixing, held]);
    assert_unresolved(&rows, &[("MixingSpec#save", Call, 3, "Link.new.save_it")]);
}

/// `expect(list).to include(Mod)` is RSpec's matcher, which mixes nothing
/// in: the module's names stay provable for every test caller, where
/// reading it as `Module#include` on an unnamed object would taint them.
#[test]
fn c10_ruby_rspecs_include_matcher_mixes_nothing_in() {
    let rows = index(&[
        (
            "app/models/report.rb",
            "module Fmt\n  def fmt; end\nend\nclass Report\n  include Fmt\nend\n",
        ),
        (
            "spec/support/fmt_matchers.rb",
            "RSpec.describe Report do
  it { expect(Report.ancestors).to include(Fmt) }
  it { expect(Report.ancestors).not_to include(Fmt).and include(Fmt) }
end
",
        ),
        (
            "spec/models/report_spec.rb",
            "class ReportSpec\n  def check; Report.new.fmt; end\nend\n",
        ),
    ]);
    assert!(has(&rows, "ReportSpec#check", 2, "Fmt#fmt"));
}

/// A shared group declared without a receiver in a group's body is that
/// group's and its nested groups' alone (checked with rspec-core 3.13.6:
/// another file's group cannot include it): the file holding it stays an
/// example file, whose stubs reach its own callers, not another example
/// file's (gumroad's `s3_retrievable_spec.rb`, whose `attr_accessor :user`
/// on a mock model left every test caller's `user` unproven).
#[test]
fn c10_ruby_a_shared_group_scoped_to_a_group_keeps_its_file_an_example_file() {
    let spec = (
        "spec/models/link_spec.rb",
        "RSpec.describe Link do
  shared_examples \"stubbed\" do
    it { |p| p.define_singleton_method(p.name) { 1 } }
  end
  context \"inner\" do
    shared_context(\"also\") { let(:x) { 1 } }
  end
  it { Link.new.save_it }
end
",
    );
    let rows = index(&[LINK, OTHER, spec]);
    assert!(has(&rows, "OtherTest#test_save", 2, "Link#save_it"));
    assert_unresolved(
        &rows,
        &[("spec/models/link_spec.rb", Call, 8, "Link.new.save_it")],
    );
}

/// A module an example group includes is, under RSpec's model, a step of
/// that group (ADR-0041), no object the rules cannot name: the file stays
/// an example file even where a class of the graph mixes the module in,
/// so another example file's stubs leave its calls alone. A call on
/// `self` in the module may still run on the group, whose own `def`s come
/// first: it proves nothing. Without the model (no `RSpec.describe` or
/// `RSpec.configure` in the program), the group is an object the rules
/// cannot name, as before.
#[test]
fn c10_ruby_a_module_a_group_includes_is_mixed_into_that_group_alone() {
    let helper = (
        "spec/support/helpful.rb",
        "module Helpful\n  def help; check; end\n  def check; end\nend\n",
    );
    let held = (
        "app/models/report.rb",
        "class Report\n  include Helpful\nend\n",
    );
    let spec = |describe: &str| {
        format!(
            "{describe} Link do
  include Helpful
  def check; end
  it {{ Link.new.save_it }}
end
"
        )
    };
    let modelled = spec("RSpec.describe");
    let mixing = ("spec/models/mixing_spec.rb", modelled.as_str());
    let rows = index(&[LINK, OTHER, STUBBING, helper, held, mixing]);
    assert!(has(&rows, "spec/models/mixing_spec.rb", 4, "Link#save_it"));
    assert_unresolved(&rows, &[("Helpful#help", Call, 2, "check")]);
    let rows = index(&[LINK, OTHER, helper, held, mixing]);
    assert_unresolved(&rows, &[("Helpful#help", Call, 2, "check")]);
    let unmodelled = spec("describe");
    let mixing = ("spec/models/mixing_spec.rb", unmodelled.as_str());
    let rows = index(&[LINK, OTHER, STUBBING, helper, held, mixing]);
    assert_unresolved(
        &rows,
        &[("spec/models/mixing_spec.rb", Call, 4, "Link.new.save_it")],
    );
}
