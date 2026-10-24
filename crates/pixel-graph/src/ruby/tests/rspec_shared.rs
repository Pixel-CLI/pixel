// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! What RSpec groups take from outside their body (ADR-0041 points 6
//! and 7): factory_bot's factories, shared contexts the repository
//! declares, and modules a group includes.

use pixel_model::EdgeKind::Call;

use super::rspec::{SPEC, calls, program};
use super::{assert_unresolved, labelled};

const SYNTAX: (&str, &str) = (
    "spec/support/factory_bot.rb",
    "RSpec.configure do |config|\n  config.include FactoryBot::Syntax::Methods\nend\n",
);

const FACTORIES: (&str, &str) = (
    "spec/factories.rb",
    "FactoryBot.define do
  factory :purchase do
    factory :gift_purchase
  end

  factory :item, class: \"Product\"
  factory :item_copy, parent: :item, aliases: [:copy]

  factory :built, class: Purchase do
    initialize_with { Purchase.new }
  end
  factory :built_child, parent: :built

  factory :dup, class: Purchase
  factory :dup, class: Product

  factory :sub_item, parent: :item, class: \"Purchase\"
end
",
);

const FACTORY_SPEC: &str = "describe Purchase do
  let(:purchase) { create(:purchase) }

  it do
    purchase.refund!
    create(:gift_purchase).refund!
    build(:item).refund!
    create(:copy).refund!
    create(:item_copy).refund!
    create(:built).refund!
    create(:built_child).refund!
    create(:dup).refund!
  end

  context \"own create\" do
    def create(*)
    end
    it { create(:purchase).refund! }
  end

  it { create(:sub_item).refund! }
end
";

/// A factory builds its `class:` (a string is read at the top level),
/// else its parent's, nested or `parent:`, else its camelized name's; an
/// alias is the factory. `initialize_with` may build anything, and is
/// inherited; a factory declared twice is no one's; a group's own
/// `create` answers first.
#[test]
fn c10_ruby_rspec_a_factory_builds_a_record_of_its_class() {
    let rows = program(FACTORY_SPEC, &[SYNTAX, FACTORIES]);
    assert_eq!(
        calls(&rows),
        labelled(&[
            (SPEC, Call, 5, "Purchase#refund!"),
            (SPEC, Call, 6, "Purchase#refund!"),
            (SPEC, Call, 7, "Product#refund!"),
            (SPEC, Call, 8, "Product#refund!"),
            (SPEC, Call, 9, "Product#refund!"),
            (SPEC, Call, 18, "create"),
            (SPEC, Call, 21, "Purchase#refund!"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            (SPEC, Call, 10, "create(:built).refund!"),
            (SPEC, Call, 11, "create(:built_child).refund!"),
            (SPEC, Call, 12, "create(:dup).refund!"),
            (SPEC, Call, 18, "create(:purchase).refund!"),
        ],
    );
}

/// `create` is factory_bot's only where the configuration includes its
/// syntax methods.
#[test]
fn c10_ruby_rspec_create_is_factory_bots_only_where_the_configuration_says_so() {
    let rows = program(FACTORY_SPEC, &[FACTORIES]);
    assert_unresolved(
        &rows,
        &[
            (SPEC, Call, 5, "purchase.refund!"),
            (SPEC, Call, 6, "create(:gift_purchase).refund!"),
        ],
    );
}

const SHARED: (&str, &str) = (
    "spec/support/shared.rb",
    "RSpec.shared_context \"with a purchase\" do
  let(:purchase) { Purchase.create! }

  def helper
  end
end

RSpec.shared_context \"twice\" do
end

RSpec.shared_context \"twice\" do
end

RSpec.shared_context \"outer\" do
  include_context \"with a purchase\"
end

RSpec.shared_context \"scoped elsewhere\" do
end

describe Product do
  shared_context \"scoped elsewhere\" do
    let(:scoped) { Product.create! }
  end
end
",
);

/// `include_context` of a shared context the repository declares once at
/// a file's top level runs its block in the group: its `let`s and `def`s
/// are the group's, which a nested group may override. One a gem declares,
/// or one declared twice, may define any name.
#[test]
fn c10_ruby_rspec_an_included_shared_context_defines_in_the_group() {
    let rows = program(
        "describe Purchase do
  include_context \"with a purchase\"

  it do
    purchase.refund!
    helper
  end

  context \"from a gem\" do
    include_context \"with stripe stubs\"
    it { helper }
  end

  context \"declared twice\" do
    include_context \"twice\"
    it { helper }
  end

  context \"overridden\" do
    let(:purchase) { Product.create! }
    it { purchase.refund! }
  end

  context \"through another\" do
    include_context \"outer\"
    it { helper }
  end

  context \"scoped elsewhere\" do
    include_context \"scoped elsewhere\"
    it { helper }
  end
end
",
        &[SHARED],
    );
    assert_eq!(
        calls(&rows),
        labelled(&[
            (SPEC, Call, 5, "Purchase#refund!"),
            (SPEC, Call, 6, "helper"),
            (SPEC, Call, 21, "Product#refund!"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            (SPEC, Call, 11, "helper"),
            (SPEC, Call, 16, "helper"),
            (SPEC, Call, 26, "helper"),
            (SPEC, Call, 31, "helper"),
        ],
    );
}

/// A shared group whose name is no literal may be any: no
/// `include_context` is proven then.
#[test]
fn c10_ruby_rspec_a_shared_group_of_a_computed_name_may_be_any() {
    let spec = "describe Purchase do
  include_context \"with a purchase\"
  it { helper }
end
";
    let site = [(SPEC, Call, 3, "helper")];
    assert_eq!(calls(&program(spec, &[SHARED])), labelled(&site));
    let computed = (
        "spec/support/computed.rb",
        "RSpec.shared_context NAME do\nend\n",
    );
    assert_unresolved(&program(spec, &[SHARED, computed]), &site);
}

/// A shared context declared with metadata may be included in any group
/// whose metadata matches, derived ones too: past the top group nothing
/// is proven, so the implicit subject is not either.
#[test]
fn c10_ruby_rspec_a_shared_context_with_metadata_hides_the_top() {
    let spec = "describe Purchase do\n  it { subject.refund! }\nend\n";
    let site = [(SPEC, Call, 2, "Purchase#refund!")];
    assert_eq!(calls(&program(spec, &[])), labelled(&site));
    let meta = (
        "spec/support/meta.rb",
        "RSpec.shared_context \"meta\", type: :model do\n  subject { Product.new }\nend\n",
    );
    assert_unresolved(
        &program(spec, &[meta]),
        &[(SPEC, Call, 2, "subject.refund!")],
    );
}

/// A module a group includes comes after the group's own methods: one of
/// the graph whose chain the rules know and that has no hook leaves the
/// rest to the parent group; a gem's, or a concern (whose `included do`
/// may define anything on the group), may define any name.
#[test]
fn c10_ruby_rspec_a_module_a_group_includes_is_a_step_after_it() {
    let rows = program(
        "describe Purchase do
  let(:purchase) { Purchase.create! }

  context \"plain\" do
    include Plain
    it { purchase.refund! }
  end

  context \"a gem's\" do
    include Devise::Test::IntegrationHelpers
    it { purchase.refund! }
  end

  context \"a concern\" do
    include Tracked
    it { purchase.refund! }
  end

  context \"a module with a gem's\" do
    include Wrapped
    it { purchase.refund! }
  end
end
",
        &[
            (
                "spec/support/plain.rb",
                "module Plain\n  def other\n  end\nend\n",
            ),
            (
                "spec/support/wrapped.rb",
                "module Wrapped\n  include Devise::Test::IntegrationHelpers\nend\n",
            ),
            (
                "spec/support/tracked.rb",
                "module Tracked\n  extend ActiveSupport::Concern\nend\n",
            ),
        ],
    );
    assert_eq!(
        calls(&rows),
        labelled(&[(SPEC, Call, 6, "Purchase#refund!")])
    );
    assert_unresolved(
        &rows,
        &[
            (SPEC, Call, 11, "purchase.refund!"),
            (SPEC, Call, 16, "purchase.refund!"),
            (SPEC, Call, 21, "purchase.refund!"),
        ],
    );
}

/// A module the example file mixes into an object the rules cannot name
/// may be mixed into the example: one defining a name a `let`'s value
/// goes through answers it (ADR-0040).
#[test]
fn c10_ruby_rspec_a_module_the_file_mixes_in_may_answer_a_lets_chain() {
    let spec = "describe Purchase do
  let(:purchase) { Purchase.create! }
  let(:item) { purchase }

  it { item.refund! }
end
";
    let site = [(SPEC, Call, 5, "Purchase#refund!")];
    assert_eq!(calls(&program(spec, &[])), labelled(&site));
    let mixed = format!("{spec}\nobject.extend(Answers)\n");
    let answers = (
        "spec/support/answers.rb",
        "module Answers\n  def purchase\n  end\nend\n",
    );
    assert_unresolved(
        &program(&mixed, &[answers]),
        &[(SPEC, Call, 5, "item.refund!")],
    );
}
