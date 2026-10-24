// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! RSpec's example groups read as classes (ADR-0041): a call in an
//! example follows the groups from its own outwards to a `def`; `let`,
//! `subject` and `described_class` type the receivers they make. Each
//! behaviour was checked with rspec-core 3.13.6 on Ruby 4.0.7.

use std::collections::BTreeSet;

use pixel_model::EdgeKind::Call;
use pixel_model::FileRows;

use super::{Row, assert_unresolved, index, labelled, resolved};

pub(super) const SPEC: &str = "spec/models/purchase_spec.rb";

const HELPER: (&str, &str) = ("spec/spec_helper.rb", "RSpec.configure do |config|\nend\n");

const RECORD: (&str, &str) = (
    "app/models/application_record.rb",
    "class ApplicationRecord < ActiveRecord::Base\nend\n",
);

const PURCHASE: (&str, &str) = (
    "app/models/purchase.rb",
    "class Purchase < ApplicationRecord\n  def refund!\n  end\n\n  def self.build_for\n  end\nend\n",
);

const PRODUCT: (&str, &str) = (
    "app/models/product.rb",
    "class Product < ApplicationRecord\n  def refund!\n  end\n\n  def self.build_for\n  end\nend\n",
);

/// The program: the spec helper, the models, `spec` at [`SPEC`], and
/// `more` besides.
pub(super) fn program(spec: &str, more: &[(&'static str, &'static str)]) -> Vec<FileRows> {
    let mut files = vec![HELPER, RECORD, PURCHASE, PRODUCT, (SPEC, spec)];
    files.extend(more);
    index(&files)
}

/// The resolved calls of the spec file.
pub(super) fn calls(rows: &[FileRows]) -> BTreeSet<Row> {
    resolved(rows)
        .into_iter()
        .filter(|row| row.1 == Call && (row.0 == SPEC || !row.0.contains('/')))
        .filter(|row| !matches!(row.0.as_str(), "Purchase" | "Product"))
        .collect()
}

/// A `let`, read through another, a `def` of the group, the implicit
/// subject (`Purchase.new`), `described_class` and `described_class.new`;
/// in a nested group, its own `let` first, its parent's `def` next.
#[test]
fn c10_ruby_rspec_an_example_reaches_its_groups_lets_defs_and_subject() {
    let rows = program(
        "describe Purchase do
  let(:purchase) { Purchase.create! }
  let(:other) { purchase }

  def helper
  end

  it \"refunds\" do
    purchase.refund!
    other.refund!
    helper
    subject.refund!
    described_class.build_for
    described_class.new.refund!
  end

  describe \"nested\" do
    let(:purchase) { Product.create! }

    it \"refunds\" do
      purchase.refund!
      helper
    end
  end
end
",
        &[],
    );
    assert_eq!(
        calls(&rows),
        labelled(&[
            (SPEC, Call, 9, "Purchase#refund!"),
            (SPEC, Call, 10, "Purchase#refund!"),
            (SPEC, Call, 11, "helper"),
            (SPEC, Call, 12, "Purchase#refund!"),
            (SPEC, Call, 13, "Purchase.build_for"),
            (SPEC, Call, 14, "Purchase#refund!"),
            (SPEC, Call, 21, "Product#refund!"),
            (SPEC, Call, 22, "helper"),
        ])
    );
}

/// A hook, a `let` and a `def` run on the examples of nested groups too:
/// what they call must be the same from each. Here `purchase` is a
/// `Purchase` in the outer group and a `Product` in the nested one, and
/// the nested group defines its own `helper`, so the hooks' and the outer
/// `helper`'s calls prove nothing; the example's do.
#[test]
fn c10_ruby_rspec_a_hook_or_def_abstains_where_a_nested_group_overrides() {
    let rows = program(
        "describe Purchase do
  let(:purchase) { Purchase.create! }
  before { purchase.refund! }
  after { helper }

  def helper
    purchase.refund!
  end

  describe \"nested\" do
    let(:purchase) { Product.create! }

    def helper
    end

    it \"refunds\" do
      purchase.refund!
      helper
    end
  end
end
",
        &[],
    );
    assert_eq!(
        calls(&rows),
        labelled(&[
            (SPEC, Call, 17, "Product#refund!"),
            (SPEC, Call, 18, "helper"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            (SPEC, Call, 3, "purchase.refund!"),
            (SPEC, Call, 4, "helper"),
            ("helper", Call, 7, "purchase.refund!"),
        ],
    );
}

/// Past a group with metadata of its own (`:js`), a module the
/// configuration filters on it may come before the parent's `def`
/// (Ruby 4.0.7: `config.include Nested, :deep` answers a `def` of the
/// parent's name); past a statement the rules do not read
/// (`render_views`, a gem's `let_it_be`), any name may be defined; an
/// `it_behaves_like` makes a
/// nested group whose own methods are unknown, which the hook of its
/// parent runs on; a nested `describe Product` describes `Product`.
#[test]
fn c10_ruby_rspec_metadata_unread_statements_and_shared_groups_hide_names() {
    let rows = program(
        "describe Purchase do
  def helper
  end

  context \"with metadata\", :js do
    it { helper }
  end

  context \"unread\" do
    render_views
    it { helper }
  end

  context \"a gem's macro\" do
    let_it_be(:helper) { Product.create! }
    it { helper.refund! }
  end

  context \"shared\" do
    let(:purchase) { Purchase.create! }
    before { purchase.refund! }
    it_behaves_like \"refundable\"
  end

  describe Product do
    it { described_class.build_for }
    it { subject.refund! }
  end
end
",
        &[],
    );
    assert_eq!(
        calls(&rows),
        labelled(&[
            (SPEC, Call, 26, "Product.build_for"),
            (SPEC, Call, 27, "Product#refund!"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            (SPEC, Call, 6, "helper"),
            (SPEC, Call, 11, "helper"),
            (SPEC, Call, 16, "helper.refund!"),
            (SPEC, Call, 21, "purchase.refund!"),
        ],
    );
}

/// Ruby keeps the last of a group's definitions of a name, a `let` or a
/// `def` (here the `def`, whose value is no `let`'s); a `let` under an
/// iterator may run any number of times, which
/// the rules do not read; a `let` block that may leave early returns what
/// it left with.
#[test]
fn c10_ruby_rspec_the_last_definition_wins_and_unread_lets_type_nothing() {
    let rows = program(
        "describe Purchase do
  let(:item) { Purchase.create! }
  def item
    Product.create!
  end
  let(:early) { next Product.create! if ENV[\"X\"]; Purchase.create! }

  it do
    item.refund!
    early.refund!
  end

  context \"looped\" do
    [1, 2].each do |n|
      let(:purchase) { Purchase.create! }
    end
    it { purchase.refund! }
  end
end
",
        &[],
    );
    assert_eq!(calls(&rows), labelled(&[(SPEC, Call, 9, "item")]));
    assert_unresolved(
        &rows,
        &[
            (SPEC, Call, 9, "item.refund!"),
            (SPEC, Call, 10, "early.refund!"),
            (SPEC, Call, 17, "purchase.refund!"),
        ],
    );
}

/// RSpec's DSL is read only for a program that uses RSpec and leaves its
/// DSL alone: without `RSpec.configure` (or `RSpec.describe`), or with a
/// method named as the DSL in spec code outside a group, or with
/// `minitest/spec` loaded, nothing is proven and a group's `def` still
/// taints its name for test callers.
#[test]
fn c10_ruby_rspec_the_model_is_off_unless_the_program_uses_rspec_alone() {
    let spec = "describe Purchase do
  let(:purchase) { Purchase.create! }

  def helper
  end

  it do
    purchase.refund!
    helper
  end
end
";
    let sites = [
        (SPEC, Call, 8, "purchase.refund!"),
        (SPEC, Call, 9, "helper"),
    ];
    let without = index(&[RECORD, PURCHASE, (SPEC, spec)]);
    assert_unresolved(&without, &sites);
    let redefined = program(
        spec,
        &[(
            "spec/support/dsl.rb",
            "module Dsl\n  def let(name)\n  end\nend\n",
        )],
    );
    assert_unresolved(&redefined, &sites);
    let minitest = program(
        spec,
        &[("spec/minitest_helper.rb", "require \"minitest/spec\"\n")],
    );
    assert_unresolved(&minitest, &sites);
    // The same program with RSpec alone resolves both.
    assert_eq!(
        calls(&program(spec, &[])),
        labelled(&[
            (SPEC, Call, 8, "Purchase#refund!"),
            (SPEC, Call, 9, "helper"),
        ])
    );
}

/// A module the configuration mixes in (`config.include`) may answer
/// `subject` or `described_class` where no group defines it; a stub the
/// file writes on an object the rules cannot name may answer the `let`'s
/// name. (A module of spec code defining them turns the model off, since
/// `config.extend` may make them the groups' own macros.)
#[test]
fn c10_ruby_rspec_config_modules_and_stubs_may_answer_rspecs_names() {
    let spec = "describe Purchase do
  let(:purchase) { Purchase.create! }

  it do
    subject.refund!
    described_class.build_for
    purchase.refund!
  end
end
";
    let configured = |defined: &'static str| {
        program(
            spec,
            &[
                ("lib/subjects.rb", defined),
                (
                    "spec/support/subjects.rb",
                    "RSpec.configure do |config|\n  config.include Subjects\nend\n",
                ),
            ],
        )
    };
    // `subject` alone: `described_class` is still RSpec's.
    assert_eq!(
        calls(&configured("module Subjects\n  def subject\n  end\nend\n")),
        labelled(&[
            (SPEC, Call, 6, "Purchase.build_for"),
            (SPEC, Call, 7, "Purchase#refund!"),
        ])
    );
    // `described_class`: the implicit subject, which calls it, too.
    assert_eq!(
        calls(&configured(
            "module Subjects\n  def described_class\n  end\nend\n"
        )),
        labelled(&[(SPEC, Call, 7, "Purchase#refund!")])
    );
    let stubbed = format!("{spec}\nobject.define_singleton_method(:purchase) {{ nil }}\n");
    let rows = program(&stubbed, &[]);
    assert_unresolved(&rows, &[(SPEC, Call, 7, "purchase.refund!")]);
    // Through a `let`'s value: `item`'s calls the stubbed `reload`.
    let through = "describe Purchase do
  let(:purchase) { Purchase.create! }
  let(:item) { purchase.reload }

  it { item.refund! }
end

object.define_singleton_method(:reload) { nil }
";
    let rows = program(through, &[]);
    assert_unresolved(&rows, &[(SPEC, Call, 5, "item.refund!")]);
}

/// `described_class` is RSpec's only where no group defines it and the
/// same constant is described from every group a hook runs on; the
/// implicit subject is an instance only of a described class, not of a
/// module or an expression's value.
#[test]
fn c10_ruby_rspec_described_class_is_the_same_constant_or_nothing() {
    let rows = program(
        "describe Purchase do
  before { described_class.build_for }

  describe Product do
    it { described_class.build_for }
  end
end

describe Purchase do
  context \"overridden\" do
    let(:described_class) { Product }
    it { described_class.build_for }
  end

  describe Refundable do
    it { subject.refund! }
  end

  describe klass do
    it { described_class.build_for }
  end
end
",
        &[(
            "app/models/refundable.rb",
            "module Refundable\n  def refund!\n  end\nend\n",
        )],
    );
    assert_eq!(
        calls(&rows),
        labelled(&[(SPEC, Call, 5, "Product.build_for")])
    );
    assert_unresolved(
        &rows,
        &[
            (SPEC, Call, 2, "described_class.build_for"),
            (SPEC, Call, 12, "described_class.build_for"),
            (SPEC, Call, 16, "subject.refund!"),
            (SPEC, Call, 20, "described_class.build_for"),
        ],
    );
}

/// Where RSpec's model reads the program, a group's `def` is the group's
/// method and leaves the name of a class's method alone; where it does not
/// (no `RSpec.configure`), the `def` is one on an object the rules cannot
/// name, which a test caller of the same file may meet (#106).
#[test]
fn c10_ruby_rspec_a_groups_def_taints_its_name_only_without_the_model() {
    let spec = "class Widget
  def helper
  end

  def run
    helper
  end
end

describe Widget do
  def helper
  end
end
";
    let site = [("Widget#run", Call, 6, "Widget#helper")];
    assert!(labelled(&site).is_subset(&resolved(&program(spec, &[]))));
    let without = index(&[RECORD, (SPEC, spec)]);
    assert_unresolved(&without, &[("Widget#run", Call, 6, "helper")]);
}

/// A block given to a constructor runs with what it builds as `self`
/// (Ruby 4.0.7: `helper` in `Class.new do` raises `NameError`), not on the
/// example; one given to another method (`expect { }`) keeps the example.
#[test]
fn c10_ruby_rspec_a_constructors_block_is_not_the_example() {
    let rows = program(
        "describe Purchase do
  def helper
  end

  it do
    expect { helper }.not_to raise_error
    Class.new do
      helper
    end
  end
end
",
        &[],
    );
    assert_eq!(calls(&rows), labelled(&[(SPEC, Call, 6, "helper")]));
    assert_unresolved(&rows, &[(SPEC, Call, 8, "helper")]);
}
