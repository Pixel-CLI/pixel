// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Ancestors and concerns: Ruby's method lookup through `prepend`,
//! `include`, `extend`, a superclass, and `ActiveSupport::Concern`'s
//! `included do` and `class_methods do`. The fixtures are v1's
//! `tests/all/ruby_ancestors.rs` tree, split by behaviour.

use pixel_model::EdgeKind::{Call, Import, Reference};

use super::{Row, assert_unresolved, index, labelled, resolved};

/// An instance method is looked up in the prepended modules, the class,
/// the included modules (a concern's `included do` includes on the
/// includer, so after the concern), then the superclass; a name only the
/// class side defines is not reached, and a same-tail module of another
/// namespace never is. v1 `ruby_self_calls_should_follow_the_declared_ancestor_chain`
/// (`Admin::Order#checkout`, `Admin::Trackable#stamp`, `Billing::Trackable`).
#[test]
fn c10_ruby_ancestors_an_instance_call_follows_prepend_class_include_superclass() {
    let rows = index(&[
        (
            "app/models/concerns/admin/trackable.rb",
            "module Admin
  module Trackable
    extend ActiveSupport::Concern

    included do
      include Admin::Auditable
      before_save :stamp
    end

    class_methods do
      def tracked_since
      end
    end

    def track
    end

    def stamp
    end
  end
end
",
        ),
        (
            "app/models/concerns/admin/auditable.rb",
            "module Admin\n  module Auditable\n    def audit\n    end\n  end\nend\n",
        ),
        (
            "app/models/concerns/billing/trackable.rb",
            "module Billing\n  module Trackable\n    def track\n    end\n\n    def audit\n    end\n\n    def tracked_since\n    end\n  end\nend\n",
        ),
        (
            "app/models/concerns/loud.rb",
            "module Loud\n  def save\n  end\nend\n",
        ),
        (
            "app/models/concerns/sortable.rb",
            "module Sortable\n  def sort_key\n  end\nend\n",
        ),
        (
            "app/models/application_record.rb",
            "class ApplicationRecord < ActiveRecord::Base\n  def persisted_label\n  end\nend\n",
        ),
        (
            "app/models/admin/order.rb",
            "module Admin
  class Order < ApplicationRecord
    include Trackable
    prepend Loud
    extend Sortable

    def save
    end

    def checkout
      track
      audit
      save
      persisted_label
      tracked_since
      self.track
    end
  end
end
",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Admin::Trackable", Reference, 6, "Admin::Auditable"),
            ("Admin::Trackable", Reference, 7, "Admin::Trackable#stamp"),
            ("Admin::Order", Reference, 2, "ApplicationRecord"),
            ("Admin::Order", Reference, 3, "Admin::Trackable"),
            ("Admin::Order", Reference, 4, "Loud"),
            ("Admin::Order", Reference, 5, "Sortable"),
            ("Admin::Order#checkout", Call, 11, "Admin::Trackable#track"),
            ("Admin::Order#checkout", Call, 12, "Admin::Auditable#audit"),
            ("Admin::Order#checkout", Call, 13, "Loud#save"),
            (
                "Admin::Order#checkout",
                Call,
                14,
                "ApplicationRecord#persisted_label"
            ),
            ("Admin::Order#checkout", Call, 16, "Admin::Trackable#track"),
        ])
    );
    assert_unresolved(
        &rows,
        &[("Admin::Order#checkout", Call, 15, "tracked_since")],
    );
}

/// A method of the class itself is looked up in its own class methods, the
/// modules it extends, the `ClassMethods` of the concerns it includes, then
/// the superclass's class side; an instance method is not reached from
/// there. `Admin::Order.sort_key` from another class follows the same
/// chain. v1 `ruby_self_calls_should_follow_the_declared_ancestor_chain`
/// (`Admin::Order.report`, `SalesReport#build`).
#[test]
fn c10_ruby_ancestors_a_class_level_call_follows_extends_and_class_methods() {
    let rows = index(&[
        (
            "app/models/concerns/admin/trackable.rb",
            "module Admin
  module Trackable
    extend ActiveSupport::Concern

    included do
      include Admin::Auditable
      before_save :stamp
    end

    class_methods do
      def tracked_since
      end
    end

    def track
    end

    def stamp
    end
  end
end
",
        ),
        (
            "app/models/concerns/admin/auditable.rb",
            "module Admin\n  module Auditable\n    def audit\n    end\n  end\nend\n",
        ),
        (
            "app/models/concerns/loud.rb",
            "module Loud\n  def save\n  end\nend\n",
        ),
        (
            "app/models/concerns/sortable.rb",
            "module Sortable\n  def sort_key\n  end\nend\n",
        ),
        (
            "app/models/application_record.rb",
            "class ApplicationRecord < ActiveRecord::Base\n  def persisted_label\n  end\nend\n",
        ),
        (
            "app/models/admin/order.rb",
            "module Admin
  class Order < ApplicationRecord
    include Trackable
    prepend Loud
    extend Sortable

    def self.report
      tracked_since
      sort_key
      track
    end
  end
end
",
        ),
        (
            "app/reports/sales.rb",
            "class SalesReport\n  def build\n    Admin::Order.sort_key\n  end\nend\n",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Admin::Trackable", Reference, 6, "Admin::Auditable"),
            ("Admin::Trackable", Reference, 7, "Admin::Trackable#stamp"),
            ("Admin::Order", Reference, 2, "ApplicationRecord"),
            ("Admin::Order", Reference, 3, "Admin::Trackable"),
            ("Admin::Order", Reference, 4, "Loud"),
            ("Admin::Order", Reference, 5, "Sortable"),
            (
                "Admin::Order.report",
                Call,
                8,
                "Admin::Trackable::ClassMethods#tracked_since",
            ),
            ("Admin::Order.report", Call, 9, "Sortable#sort_key"),
            ("SalesReport#build", Call, 3, "Sortable#sort_key"),
        ])
    );
    assert_unresolved(&rows, &[("Admin::Order.report", Call, 10, "track")]);
}

/// The class's own definition comes before an included one, and an alias
/// reaches the inherited method it copies. v1
/// `ruby_self_calls_should_follow_the_declared_ancestor_chain`
/// (`Admin::Invoice#close`, `Admin::Invoice#label`).
#[test]
fn c10_ruby_aliasing_an_own_method_overrides_and_an_alias_reaches_an_inherited_one() {
    let rows = index(&[
        (
            "app/models/concerns/admin/trackable.rb",
            "module Admin
  module Trackable
    extend ActiveSupport::Concern

    included do
      include Admin::Auditable
      before_save :stamp
    end

    class_methods do
      def tracked_since
      end
    end

    def track
    end

    def stamp
    end
  end
end
",
        ),
        (
            "app/models/concerns/admin/auditable.rb",
            "module Admin\n  module Auditable\n    def audit\n    end\n  end\nend\n",
        ),
        (
            "app/models/application_record.rb",
            "class ApplicationRecord < ActiveRecord::Base\n  def persisted_label\n  end\nend\n",
        ),
        (
            "app/models/admin/invoice.rb",
            "module Admin
  class Invoice < ApplicationRecord
    include Trackable
    alias_method :label, :persisted_label

    def track
    end

    def close
      track
    end
  end
end
",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Admin::Trackable", Reference, 6, "Admin::Auditable"),
            ("Admin::Trackable", Reference, 7, "Admin::Trackable#stamp"),
            ("Admin::Invoice", Reference, 2, "ApplicationRecord"),
            ("Admin::Invoice", Reference, 3, "Admin::Trackable"),
            (
                "Admin::Invoice#label",
                Reference,
                4,
                "ApplicationRecord#persisted_label",
            ),
            ("Admin::Invoice#close", Call, 10, "Admin::Invoice#track"),
        ])
    );
}

/// Mutual includes end the walk without losing what precedes; a superclass
/// cycle makes the rest unknown; a module the graph cannot name
/// (`include(PLUGINS.first)`), included after a known one, is looked up
/// first, so it may define the name. v1
/// `ruby_self_calls_should_follow_the_declared_ancestor_chain` (`Cycler`,
/// `Alpha`, `Dynamic`).
#[test]
fn c10_ruby_ancestors_cycles_end_the_chain_and_an_unknown_module_abstains() {
    let rows = index(&[
        (
            "app/models/concerns/sortable.rb",
            "module Sortable\n  def sort_key\n  end\nend\n",
        ),
        (
            "app/models/cycle.rb",
            "module Ping
  include Pong
  def ping
  end
end
module Pong
  include Ping
  def pong
  end
end
class Cycler
  include Ping
  def run
    pong
    ping
    never_defined
  end
end
",
        ),
        (
            "app/models/superclass_cycle.rb",
            "class Alpha < Beta\n  def run\n    never_defined\n  end\nend\nclass Beta < Alpha\nend\n",
        ),
        (
            "app/models/dynamic.rb",
            "class Dynamic\n  include Sortable\n  include(PLUGINS.first)\n\n  def run\n    sort_key\n  end\nend\n",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Ping", Reference, 2, "Pong"),
            ("Pong", Reference, 7, "Ping"),
            ("Cycler", Reference, 12, "Ping"),
            ("Cycler#run", Call, 14, "Pong#pong"),
            ("Cycler#run", Call, 15, "Ping#ping"),
            ("Alpha", Reference, 1, "Beta"),
            ("Beta", Reference, 6, "Alpha"),
            ("Dynamic", Reference, 2, "Sortable"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("Cycler#run", Call, 16, "never_defined"),
            ("Alpha#run", Call, 3, "never_defined"),
            ("Dynamic", Reference, 3, "PLUGINS.first"),
            ("Dynamic#run", Call, 6, "sort_key"),
        ],
    );
}

/// The includes of one class spread over two files come in a load order
/// the graph cannot tell: two of them defining a name leave it unresolved
/// (`track`, `audit`); a name one ancestor alone defines still resolves,
/// and so does one a module prepended in either file defines first
/// (`save`): a prepended module comes before the class whatever the load
/// order (Ruby 3.3, both orders run).
/// v1 `ruby_ancestor_edges_should_follow_incremental_changes` (its
/// `order_billing.rb` step, full-build form).
#[test]
fn c10_ruby_ancestors_includes_declared_in_two_files_abstain_on_a_shared_name() {
    let rows = index(&[
        (
            "app/models/concerns/admin/trackable.rb",
            "module Admin
  module Trackable
    extend ActiveSupport::Concern

    included do
      include Admin::Auditable
      before_save :stamp
    end

    class_methods do
      def tracked_since
      end
    end

    def track
    end

    def stamp
    end
  end
end
",
        ),
        (
            "app/models/concerns/admin/auditable.rb",
            "module Admin\n  module Auditable\n    def audit\n    end\n  end\nend\n",
        ),
        (
            "app/models/concerns/billing/trackable.rb",
            "module Billing\n  module Trackable\n    def track\n    end\n\n    def audit\n    end\n  end\nend\n",
        ),
        (
            "app/models/concerns/loud.rb",
            "module Loud\n  def save\n  end\nend\n",
        ),
        (
            "app/models/application_record.rb",
            "class ApplicationRecord < ActiveRecord::Base\n  def persisted_label\n  end\nend\n",
        ),
        (
            "app/models/admin/order.rb",
            "module Admin
  class Order < ApplicationRecord
    include Trackable
    prepend Loud

    def save
    end

    def checkout
      track
      audit
      save
      persisted_label
    end
  end
end
",
        ),
        (
            "app/models/admin/order_billing.rb",
            "module Admin\n  class Order\n    include Billing::Trackable\n  end\nend\n",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Admin::Trackable", Reference, 6, "Admin::Auditable"),
            ("Admin::Trackable", Reference, 7, "Admin::Trackable#stamp"),
            ("Admin::Order", Reference, 2, "ApplicationRecord"),
            ("Admin::Order", Reference, 3, "Admin::Trackable"),
            ("Admin::Order", Reference, 4, "Loud"),
            ("Admin::Order", Reference, 3, "Billing::Trackable"),
            ("Admin::Order#checkout", Call, 12, "Loud#save"),
            (
                "Admin::Order#checkout",
                Call,
                13,
                "ApplicationRecord#persisted_label"
            ),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("Admin::Order#checkout", Call, 10, "track"),
            ("Admin::Order#checkout", Call, 11, "audit"),
        ],
    );
}

/// `include Trackable` in `Admin::Order` names `Admin::Order::Trackable`
/// once it exists, never the concern a level out nor `Billing::Trackable`:
/// the edge and the calls follow the nearer module. v1
/// `ruby_ancestor_edges_should_follow_incremental_changes` (its
/// `admin/order/trackable.rb` step, full-build form).
#[test]
fn c10_ruby_shadowing_an_include_names_the_nearest_module_of_that_name() {
    let rows = index(&[
        (
            "app/models/concerns/admin/trackable.rb",
            "module Admin\n  module Trackable\n    def track\n    end\n  end\nend\n",
        ),
        (
            "app/models/concerns/billing/trackable.rb",
            "module Billing\n  module Trackable\n    def track\n    end\n  end\nend\n",
        ),
        (
            "app/models/admin/order.rb",
            "module Admin
  class Order
    include Trackable

    def checkout
      track
    end
  end
end
",
        ),
        (
            "app/models/admin/order/trackable.rb",
            "module Admin\n  class Order\n    module Trackable\n      def track\n      end\n    end\n  end\nend\n",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Admin::Order", Reference, 3, "Admin::Order::Trackable"),
            (
                "Admin::Order#checkout",
                Call,
                6,
                "Admin::Order::Trackable#track"
            ),
        ])
    );
}

/// A module the graph cannot name, included after a concern, may be a
/// gem's concern whose `ClassMethods` the class then has before the
/// concern's: a class method found only past it is unresolved. Included
/// before, it comes after the concern's. v1 skipped such modules on the
/// class side. A gem's module included by a module the class includes
/// (once-campfire's `User::Mentionable`, which includes
/// `ActionText::Attachable`) extends that module, not the class: Ruby 3.3
/// with Active Support 7.1 gives `User.create_bot!` from `Bot`.
#[test]
fn c10_ruby_ancestors_an_unknown_module_may_bring_class_methods() {
    let rows = index(&[
        (
            "app/models/bot.rb",
            "module Bot\n  extend ActiveSupport::Concern\n  class_methods do\n    def create_bot!; end\n  end\nend\n",
        ),
        (
            "app/models/user.rb",
            "class User\n  include Bot\n  include ActionText::Attachable\nend\n",
        ),
        (
            "app/models/account.rb",
            "class Account\n  include ActionText::Attachable\n  include Bot\nend\n",
        ),
        ("app/caller.rb", "User.create_bot!\nAccount.create_bot!\n"),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("User", Reference, 2, "Bot"),
            ("Account", Reference, 3, "Bot"),
            ("app/caller.rb", Call, 2, "Bot::ClassMethods#create_bot!"),
        ])
    );
    assert_unresolved(&rows, &[("app/caller.rb", Call, 1, "User.create_bot!")]);
}

/// The arguments of one `include`, `prepend` or `extend` are inserted
/// last first, so the first is looked up first; two calls look the later
/// one up first. Ruby 3.3: `Multi.ancestors` is `[Multi, First, Second]`,
/// and `run`, `run`, `go`, `run` reach First, First, First, Second.
#[test]
fn c10_ruby_ancestors_the_first_argument_of_one_call_is_looked_up_first() {
    // D10
    let rows = index(&[(
        "lib/mods.rb",
        "module First
  def name; end
  def save; end
end
module Second
  def name; end
  def save; end
end
class Multi
  include First, Second
  def run; name; end
end
class Pre
  prepend First, Second
  def save; end
  def run; save; end
end
class Ext
  extend First, Second
  def self.go; name; end
end
class Seq
  include First
  include Second
  def run; name; end
end
",
    )]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Multi", Reference, 10, "First"),
            ("Multi", Reference, 10, "Second"),
            ("Multi#run", Call, 11, "First#name"),
            ("Pre", Reference, 14, "First"),
            ("Pre", Reference, 14, "Second"),
            ("Pre#run", Call, 16, "First#save"),
            ("Ext", Reference, 19, "First"),
            ("Ext", Reference, 19, "Second"),
            ("Ext.go", Call, 20, "First#name"),
            ("Seq", Reference, 23, "First"),
            ("Seq", Reference, 24, "Second"),
            ("Seq#run", Call, 25, "Second#name"),
        ])
    );
}

/// A module already among a class's ancestors, its superclass's included,
/// is not inserted again, so it keeps its place; a module prepended twice
/// keeps its first place. Ruby 3.3: `C.ancestors` starts `[C, B, A]`,
/// `K`'s `[K, B, Sup, A]`, `P`'s `[B, A, P]`; each `run` reaches `B#tag`.
#[test]
fn c10_ruby_ancestors_a_module_already_included_keeps_its_place() {
    // F3 (Astra's `repeated_include`)
    let rows = index(&[(
        "lib/tags.rb",
        "module A
  def tag; end
end
module B
  def tag; end
end
class C
  include A
  include B
  include A
  def run; tag; end
end
class Sup
  include A
end
class K < Sup
  include B
  include A
  def run; tag; end
end
class P
  prepend A
  prepend B
  prepend A
  def tag; end
  def run; tag; end
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
            ("C#run".to_owned(), Call, 11, "B#tag".to_owned()),
            ("K#run".to_owned(), Call, 19, "B#tag".to_owned()),
            ("P#run".to_owned(), Call, 26, "B#tag".to_owned()),
        ]
    );
}

/// `include` and `prepend` in `class << self` act on the singleton class:
/// the module's methods come before the superclass's class methods, and a
/// prepended one before the singleton's own. Ruby 3.3: `Article.load` and
/// `Note.go` both reach `Overrides#find`.
#[test]
fn c10_ruby_ancestors_include_in_class_self_is_on_the_singleton_chain() {
    // D35
    let rows = index(&[(
        "lib/article.rb",
        "class Base
  def self.find; end
end
module Overrides
  def find; end
end
class Article < Base
  class << self
    include Overrides
  end
  def self.load; find; end
end
class Note < Base
  class << self
    prepend Overrides
    def find; end
  end
  def self.go; find; end
end
",
    )]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Article", Reference, 7, "Base"),
            ("Article", Reference, 9, "Overrides"),
            ("Article.load", Call, 11, "Overrides#find"),
            ("Note", Reference, 13, "Base"),
            ("Note", Reference, 15, "Overrides"),
            ("Note.go", Call, 18, "Overrides#find"),
        ])
    );
}

/// A concern's `included do; prepend Loud; end` prepends `Loud` to the
/// class including it, before the class's own method. Ruby 3.3 with
/// Active Support 7.1: `Post.ancestors` starts `[Loud, Post, Auditable]`
/// and `show` reaches `Loud#save`.
#[test]
fn c10_ruby_ancestors_a_concerns_included_prepend_lands_before_the_includer() {
    // D36
    let rows = index(&[
        (
            "lib/auditable.rb",
            "module Loud
  def save; end
end
module Auditable
  extend ActiveSupport::Concern
  included do
    prepend Loud
  end
end
",
        ),
        (
            "lib/post.rb",
            "class Post
  include Auditable
  def save; end
  def show; save; end
end
",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Auditable", Reference, 7, "Loud"),
            ("Post", Reference, 2, "Auditable"),
            ("Post#show", Call, 4, "Loud#save"),
        ])
    );
}

/// `self.included(base)` runs on the class including the module:
/// `base.extend(ClassMethods)` and `base.send(:include, Helpers)` act on
/// it, so a class method and an instance method of the includer reach
/// them before the superclass's; a statement the rules do not read
/// (`register(base)`) leaves the includer's chains unknown. A
/// superclass's `self.inherited(sub)` acts on each subclass. Ruby 3.3:
/// `Post.load`, `Post#show` reach Finder's modules, `Widget#run`
/// `Tracking#track`; `Page`'s hook happens to do nothing.
#[test]
fn c10_ruby_ancestors_a_hook_acts_on_the_includer() {
    // D14
    let rows = index(&[(
        "lib/hooks.rb",
        "class Base
  def self.find; end
  def help; end
end
module Finder
  def self.included(base)
    super
    base.extend(ClassMethods)
    base.send(:include, Helpers)
  end
  module ClassMethods
    def find; end
  end
  module Helpers
    def help; end
  end
end
module Opaque
  def self.included(base)
    register(base)
  end
  def self.register(base); end
end
class Post < Base
  include Finder
  def self.load; find; end
  def show; help; end
end
class Page < Base
  include Opaque
  def self.load; find; end
  def show; help; end
end
module Tracking
  def track; end
end
class Model
  def self.inherited(sub)
    super
    sub.include(Tracking)
  end
  def track; end
end
class Widget < Model
  def run; track; end
end
",
    )]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Finder.included", Reference, 8, "Finder::ClassMethods"),
            ("Finder.included", Reference, 9, "Finder::Helpers"),
            ("Opaque.included", Call, 20, "Opaque.register"),
            ("Post", Reference, 24, "Base"),
            ("Post", Reference, 25, "Finder"),
            ("Post.load", Call, 26, "Finder::ClassMethods#find"),
            ("Post#show", Call, 27, "Finder::Helpers#help"),
            ("Page", Reference, 29, "Base"),
            ("Page", Reference, 30, "Opaque"),
            ("Model.inherited", Reference, 40, "Tracking"),
            ("Widget", Reference, 44, "Model"),
            ("Widget#run", Call, 45, "Tracking#track"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("Page.load", Call, 31, "find"),
            ("Page#show", Call, 32, "help"),
        ],
    );
}

/// The constant of an `include` is looked up where Ruby evaluates it: the
/// nesting, then the ancestors of the class so far (its superclass's
/// chain, the modules it included above), then the top level. Ruby 3.3:
/// `Job#run` reaches `Base::Retry#again`, `Mixed#run` `Foo::Bar#w`.
#[test]
fn c10_ruby_shadowing_a_mixin_constant_is_found_through_the_ancestors_first() {
    // D37
    let rows = index(&[(
        "lib/job.rb",
        "class Base
  module Retry
    def again; end
  end
end
module Retry
  def again; end
end
class Job < Base
  include Retry
  def run; again; end
end
module Foo
  module Bar
    def w; end
  end
end
module Bar
  def w; end
end
class Mixed
  include Foo
  include Bar
  def run; w; end
end
",
    )]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Job", Reference, 9, "Base"),
            ("Job", Reference, 10, "Base::Retry"),
            ("Job#run", Call, 11, "Base::Retry#again"),
            ("Mixed", Reference, 22, "Foo"),
            ("Mixed", Reference, 23, "Foo::Bar"),
            ("Mixed#run", Call, 24, "Foo::Bar#w"),
        ])
    );
}

/// A reopened core class holds methods and a superclass the graph does
/// not: a name its reopening does not define is not a top-level
/// function's, and a call on `self` there may run on a subclass the graph
/// does not hold either (a gem's, such as Active Support's `SafeBuffer`),
/// whose own method comes first. An instance of an application class
/// still reaches `Object`; the class itself reaches it only past `Module`
/// and `Class`. Ruby 3.3: `"x".shout` is `"X"` (`String#upcase`), with
/// `class Safe < String; def shout = :safe` `Safe.new.loud` is `:safe`,
/// `Report#run` the top-level `upcase`, `Report.label` `"Report"`
/// (`Module#name`).
#[test]
fn c10_ruby_dynamic_dispatch_a_reopened_core_class_reaches_no_function() {
    // D31, R5
    let rows = index(&[
        ("lib/helpers.rb", "def upcase; end\ndef name; end\n"),
        (
            "lib/core_ext.rb",
            "require_relative \"helpers\"
class String
  def shout; upcase; end
  def loud; shout; end
end
class Report
  def run; upcase; end
  def self.label; name; end
end
",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("lib/core_ext.rb", Import, 1, "lib/helpers.rb"),
            ("Report#run", Call, 7, "upcase"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("String#shout", Call, 3, "upcase"),
            ("String#loud", Call, 4, "shout"),
            ("Report.label", Call, 8, "name"),
        ],
    );
}

/// `class X < Object` proves no top-level function, as on main: code the
/// rules do not read, such as `send(:eval, "Object.prepend(P)")`, may have
/// changed `Object` (R8-01, which reverts R5-15). A reopened `Object`
/// defines its methods where a top-level `def` does, as a method of
/// `Object`: the one loaded last wins, which the graph cannot tell, so
/// neither is proven; `Kernel` comes after them. Ruby 3.3: `Obj2.new.run`
/// is `:top`, and `X4.new.run` `:p`; with `lib/obj.rb` loaded after the
/// helpers, `Plain.new.go2` is `:obj` and `Plain.new.k2` `:top`.
#[test]
fn c10_ruby_dynamic_dispatch_object_written_as_a_superclass_proves_no_function() {
    // R5-15, R8-01
    let rows = index(&[
        (
            "lib/helpers.rb",
            "def helper; :top; end\ndef go; :top; end\ndef k; :top; end\n",
        ),
        (
            "lib/obj.rb",
            "require_relative \"helpers\"
class Object
  def go; :obj; end
end
module Kernel
  def k; :kernel; end
end
",
        ),
        (
            "lib/k.rb",
            "require_relative \"obj\"
class Obj2 < Object
  def run; helper; end
end
class Plain
  def go2; go; end
  def k2; k; end
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
        vec![("Plain#k2".to_owned(), Call, 7, "k".to_owned())]
    );
    assert_unresolved(
        &rows,
        &[
            ("Obj2#run", Call, 3, "helper"),
            ("Plain#go2", Call, 6, "go"),
        ],
    );
    let rows = index(&[
        ("lib/helpers.rb", "def helper; :top; end\n"),
        (
            "lib/k.rb",
            "require_relative \"helpers\"
module P; def helper; :p; end; end
class X4 < Object
  def run; helper; end
end
send(:eval, \"Object.prepend(P)\")
",
        ),
    ]);
    assert_unresolved(&rows, &[("X4#run", Call, 4, "helper")]);
}

/// Code evaluated on `Object` from outside its body, which the graph does
/// not reopen, may define or remove what a top-level `def` does, or put a
/// module before it: no top-level function is proven for the names it may
/// touch, whatever the class (R6-01). Ruby 3.3 runs `P#helper` for
/// `X4.new.run` and `X5.new.run` once `Object.prepend(P)` has run, under
/// a condition or not, sent, in a `class_eval` block, or in a string a
/// binding evaluates (R7-03); a `remove_method :other` sent to `Object`
/// touches `other` only, and `X4`, written `< Object`, proves nothing
/// (R8-01).
#[test]
fn c10_ruby_dynamic_dispatch_object_changed_from_outside_proves_no_function() {
    // R6-01, R7-03
    let classes = "require_relative \"helpers\"
module P; def helper; :p; end; end
class X4 < Object
  def run; helper; end
end
class X5
  def run; helper; end
end
";
    for outside in [
        "Object.prepend(P) if ENV[\"NOPE\"].nil?",
        "::Object.send(:prepend, P)",
        "Object.class_eval { prepend P }",
        "Object.include(P)\nObject.send(:remove_method, :helper)",
        "binding.eval(\"Object.prepend(P)\")",
        "Kernel.eval(\"Object.prepend(P)\")",
        "TOPLEVEL_BINDING.eval(\"self.class.prepend(P)\")",
    ] {
        let rows = index(&[
            ("lib/helpers.rb", "def helper; :top; end\n"),
            ("lib/k.rb", classes),
            ("lib/z.rb", &format!("require_relative \"k\"\n{outside}\n")),
        ]);
        assert_unresolved(
            &rows,
            &[("X4#run", Call, 4, "helper"), ("X5#run", Call, 7, "helper")],
        );
    }
    let rows = index(&[
        ("lib/helpers.rb", "def helper; :top; end\n"),
        ("lib/k.rb", classes),
        ("lib/z.rb", "Object.send(:remove_method, :other)\n"),
    ]);
    let calls: Vec<Row> = resolved(&rows)
        .into_iter()
        .filter(|row| row.1 == Call)
        .collect();
    assert_eq!(
        calls,
        vec![("X5#run".to_owned(), Call, 7, "helper".to_owned())]
    );
}

/// A concern a concern prepends or includes is deferred to the class: a
/// `prepend` of the outer one prepends it to the class too, before the
/// class's own method. A concern with no `ClassMethods` of its own extends
/// the class with the one an ancestor holds (`const_get`). A class call
/// sees a module prepended to the singleton class before the class's own
/// method. Ruby 3.3 with Active Support 7.1: `Post#show` reaches
/// `Loud#save` (`Post.ancestors` starts `[Audited, Loud, Post]`),
/// `Page.load` `Finders::ClassMethods#find`, `Note.find` the same.
#[test]
fn c10_ruby_ancestors_a_concerns_dependencies_and_class_methods_reach_the_class() {
    // D36, D35 (found by a differential check against Ruby)
    let rows = index(&[(
        "lib/concerns.rb",
        "module Loud
  extend ActiveSupport::Concern
  def save; end
end
module Audited
  extend ActiveSupport::Concern
  prepend Loud
end
class Post
  prepend Audited
  def save; end
  def show; save; end
end
module Finders
  module ClassMethods
    def find; end
  end
end
module Searchable
  extend ActiveSupport::Concern
  include Finders
end
class Base
  def self.find; end
end
class Page < Base
  include Searchable
  def self.load; find; end
end
class Note
  class << self
    prepend Finders::ClassMethods
  end
  def self.find; end
end
Note.find
",
    )]);
    let calls: Vec<Row> = resolved(&rows)
        .into_iter()
        .filter(|row| row.1 == Call)
        .collect();
    assert_eq!(
        calls,
        vec![
            (
                "Page.load".to_owned(),
                Call,
                28,
                "Finders::ClassMethods#find".to_owned()
            ),
            ("Post#show".to_owned(), Call, 12, "Loud#save".to_owned()),
            (
                "lib/concerns.rb".to_owned(),
                Call,
                36,
                "Finders::ClassMethods#find".to_owned()
            ),
        ]
    );
}

/// A hook's `super` runs the hook the superclass (or an extended module)
/// defines; an `extend` of the singleton class (`class << self; include`
/// of a concern whose `included do` extends) runs the module's `extended`
/// hook on the singleton class; a concern's own `included` hook runs even
/// when Active Support defers the concern, and what it prepends to a
/// concern is deferred too. Ruby 3.3 with Active Support 7.1:
/// `Widget#run` reaches `Tracking#track`, `Page.go` `Shout#find`, `K#run`
/// `Outer#a` (`K.ancestors` starts `[K, Outer, Hooky, Base2]`).
#[test]
fn c10_ruby_ancestors_hooks_run_super_and_on_the_singleton_class() {
    // D14 (found by a differential check against Ruby)
    let rows = index(&[(
        "lib/hooks.rb",
        "module Tracking
  def track; end
end
class Model
  def self.inherited(sub)
    super
    sub.prepend(Tracking)
  end
end
class Record < Model
  def self.inherited(sub)
    super
    sub.extend(Tracking)
  end
end
class Widget < Record
  def track; end
  def run; track; end
end
module Shout
  def find; end
end
module Loud
  def self.extended(base)
    base.send(:prepend, Shout)
  end
end
module Lookups
  extend ActiveSupport::Concern
  included do
    extend Loud
  end
end
class Page
  class << self
    include Lookups
  end
  def self.find; end
  def self.go; find; end
end
module Base2
  extend ActiveSupport::Concern
  def a; end
end
module Hooky
  extend ActiveSupport::Concern
  def self.included(base)
    base.send(:prepend, Base2)
  end
end
module Outer
  extend ActiveSupport::Concern
  include Hooky
  def a; end
end
class K
  include Outer
  def run; a; end
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
            ("K#run".to_owned(), Call, 58, "Outer#a".to_owned()),
            ("Page.go".to_owned(), Call, 39, "Shout#find".to_owned()),
            (
                "Widget#run".to_owned(),
                Call,
                18,
                "Tracking#track".to_owned()
            ),
        ]
    );
}

/// In `class << self` the constants of the modules the class extends are
/// in scope, before the top level's. Ruby 3.3: `K.go` reaches `N0::X#w`.
#[test]
fn c10_ruby_shadowing_a_mixin_constant_in_class_self_is_found_through_the_extends() {
    // D37, D35 (found by a differential check against Ruby)
    let rows = index(&[(
        "lib/k.rb",
        "module X
  def w; end
end
module N0
  module X
    def w; end
  end
end
class K
  extend N0
  class << self
    include X
  end
  def self.go; w; end
end
",
    )]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("K", Reference, 10, "N0"),
            ("K", Reference, 12, "N0::X"),
            ("K.go", Call, 14, "N0::X#w"),
        ])
    );
}

/// `singleton_class.include M` and `singleton_class.prepend M` in a class
/// or module body act on its singleton class, as in `class << self`;
/// `send(:include, M)` and its family are the call they send. A module's
/// own methods (`module_function`'s copies) come after a module prepended
/// to its singleton class. Ruby 3.3: `[C1.find, C2.find, C3.new.save,
/// C4.new.save, C5.find, Util.esc]` is `[:sing, :sing, :loud, :loud,
/// :sing, :ext]`, and `Base#run` and `Base.go` run on those subclasses.
#[test]
fn c10_ruby_ancestors_singleton_class_and_sent_mixins_are_read() {
    // R3, R12
    let rows = index(&[(
        "lib/a.rb",
        "module Loud; def save; end; end
module Sing; def find; end; end
class Base; def save; end; def self.find; end; def run; save; end; def self.go; find; end; end
class C1 < Base; singleton_class.include Sing; end
class C2 < Base; singleton_class.send(:include, Sing); end
class C3 < Base; send(:include, Loud); end
class C4 < Base; self.send(:prepend, Loud); end
class C5 < Base; singleton_class.prepend Sing; end
module Esc; def esc; end; end
module Util
  include Esc
  module_function :esc
end
module Ext; def esc; end; end
module Util
  singleton_class.prepend Ext
end
class X
  def r; C1.find; C2.find; C3.new.save; C4.new.save; C5.find; Util.esc; end
end
",
    )]);
    let mut expected = labelled(&[
        ("Util", Reference, 11, "Esc"),
        ("Util", Reference, 16, "Ext"),
        ("X#r", Call, 19, "Sing#find"),
        ("X#r", Call, 19, "Loud#save"),
        ("X#r", Call, 19, "Ext#esc"),
    ]);
    for (class, line, module) in [
        ("C1", 4, "Sing"),
        ("C2", 5, "Sing"),
        ("C3", 6, "Loud"),
        ("C4", 7, "Loud"),
        ("C5", 8, "Sing"),
    ] {
        expected.insert((class.to_owned(), Reference, line, "Base".to_owned()));
        expected.insert((class.to_owned(), Reference, line, module.to_owned()));
    }
    assert_eq!(resolved(&rows), expected);
    assert_unresolved(
        &rows,
        &[("Base#run", Call, 3, "save"), ("Base.go", Call, 3, "find")],
    );
}

/// A module that extends one the graph cannot name may get its
/// `included` hook there (a gem's `Hooky`), which may act on the includer
/// as the rules cannot read: its lookups prove nothing. A module the graph
/// holds whole leaves the includer's chain known, and so does one that
/// includes a gem's module, whose `ClassMethods` are taken to define no
/// hook of the module's (#120). Ruby 3.3, with a `Hooky#included` that
/// extends `ClassMethods`: `Post.go` is `:plug`, `Clean.go` `:base`.
#[test]
fn c10_ruby_ancestors_a_hook_from_a_module_outside_the_graph_is_unknown() {
    // R7
    let rows = index(&[(
        "lib/a.rb",
        "class Base; def self.find; end; end
module Plug
  extend Gemmy::Hooky
  module ClassMethods; def find; end; end
end
class Post < Base
  include Plug
  def self.go; find; end
  def run; close; end
  def close; end
end
module Mentionable
  include Gemmy::Attachable
end
class User < Base
  include Mentionable
  def self.go; find; end
end
module Plain; end
class Clean < Base
  include Plain
  def self.go; find; end
  def run; close; end
  def close; end
end
",
    )]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Post", Reference, 6, "Base"),
            ("Post", Reference, 7, "Plug"),
            ("User", Reference, 15, "Base"),
            ("User", Reference, 16, "Mentionable"),
            ("User.go", Call, 17, "Base.find"),
            ("Clean", Reference, 20, "Base"),
            ("Clean", Reference, 21, "Plain"),
            ("Clean.go", Call, 22, "Base.find"),
            ("Clean#run", Call, 23, "Clean#close"),
        ])
    );
    assert_unresolved(
        &rows,
        &[("Post.go", Call, 8, "find"), ("Post#run", Call, 9, "close")],
    );
}

#[test]
fn c10_ruby_ancestors_a_concern_a_module_extends_runs_no_unknown_hook() {
    // R7: `ActiveSupport::Concern`, outside the graph, defines no
    // `extended` and no `extend_object` (Active Support 7.1, checked with
    // Ruby 3.3: `Post.new.show` is `Plain#label`, `Plain.go` is
    // `Tracked#track`).
    let rows = index(&[(
        "lib/a.rb",
        "module Tracked
  extend ActiveSupport::Concern
  def track; end
end
module Plain
  extend Tracked
  def label; end
  def self.go; track; end
end
class Post
  include Plain
  def show; label; end
end
",
    )]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Plain", Reference, 6, "Tracked"),
            ("Plain.go", Call, 8, "Tracked#track"),
            ("Post", Reference, 11, "Plain"),
            ("Post#show", Call, 12, "Plain#label"),
        ])
    );
}

#[test]
fn c10_ruby_dynamic_dispatch_a_module_an_unread_hook_mixes_in_is_held_by_its_includer() {
    // R7: `Gemmy::Hooky`'s `included` (outside the graph) may include
    // `Plug` and extend the class with `Plug::ClassMethods`; checked with
    // Ruby 3.3, a hook that does both: `Post.go` is `Post.find`,
    // `Post.new.run` is `Post#close`.
    let rows = index(&[(
        "lib/a.rb",
        "module Plug
  extend Gemmy::Hooky
  module ClassMethods
    def go; find; end
    def find; end
  end
  def run; close; end
  def close; end
end
module Plain
  def run; close; end
  def close; end
end
class Post
  include Plug
  def close; end
  def self.find; end
end
class Page
  include Plain
end
",
    )]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Plain#run", Call, 11, "Plain#close"),
            ("Post", Reference, 15, "Plug"),
            ("Page", Reference, 20, "Plain"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("Plug::ClassMethods#go", Call, 4, "find"),
            ("Plug#run", Call, 7, "close"),
        ],
    );
}

#[test]
fn c10_ruby_dynamic_dispatch_extend_self_holds_the_module_on_its_singleton_class() {
    // R7: after `extend self` and a later `include`, the singleton class
    // holds the module's own list, which defines no hook; checked with
    // Ruby 3.3: `Ident.dom_id` is `:single`, `Form.new.dom_id` `:inst`.
    let rows = index(&[(
        "lib/a.rb",
        "module Naming
  def model_name; end
end
module Ident
  extend self
  include Naming
  def dom_id; dom_class; end
  def dom_class; end
  def self.dom_class; end
end
module Plain
  extend self
  def run; close; end
  def close; end
end
class Form
  include Ident
  include Plain
  def show; model_name; end
end
",
    )]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Ident", Reference, 5, "Ident"),
            ("Ident", Reference, 6, "Naming"),
            ("Plain", Reference, 12, "Plain"),
            ("Plain#run", Call, 13, "Plain#close"),
            ("Form", Reference, 17, "Ident"),
            ("Form", Reference, 18, "Plain"),
            ("Form#show", Call, 19, "Naming#model_name"),
        ])
    );
    assert_unresolved(&rows, &[("Ident#dom_id", Call, 7, "dom_class")]);
}

/// A hook runs for an `include` or a subclass only once it is defined:
/// below it in the same file it does not run; in another file it runs when
/// that file is the one defining the module, which the `include` names
/// only once loaded, and the load order decides otherwise. Ruby 3.3:
/// `Post.load` is `:base`, `Leaf.new.run` `:base`, `Item.load` `:finder`;
/// with main.rb's order, `Page.load` is `:base`.
#[test]
fn c10_ruby_ancestors_a_hook_runs_only_for_what_comes_after_it() {
    // B3
    let rows = index(&[
        (
            "lib/a.rb",
            "class Base; def self.find; :base; end; end
module Finder
  module ClassMethods; def find; :finder; end; end
end
class Post < Base
  include Finder
  def self.load; find; end
end
module Finder
  def self.included(base); base.extend(ClassMethods); end
end
module Tracking; def track; :tracking; end; end
class Base; def track; :base; end; end
class Leaf < Base; def run; track; end; end
class Base
  def self.inherited(sub); super; sub.include(Tracking); end
end
",
        ),
        (
            "lib/hooked.rb",
            "module Hooked
  module ClassMethods; def find; :finder; end; end
  def self.included(base); base.extend(ClassMethods); end
end
",
        ),
        (
            "lib/item.rb",
            "class Item < Base
  include Hooked
  def self.load; find; end
end
",
        ),
        (
            "lib/paged.rb",
            "module Paged\n  module ClassMethods; def find; :finder; end; end\nend\n",
        ),
        (
            "lib/paged_hook.rb",
            "module Paged\n  def self.included(base); base.extend(ClassMethods); end\nend\n",
        ),
        (
            "lib/page.rb",
            "require_relative \"paged\"
class Page < Base
  include Paged
  def self.load; find; end
end
",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Post", Reference, 5, "Base"),
            ("Post", Reference, 6, "Finder"),
            ("Post.load", Call, 7, "Base.find"),
            ("Finder.included", Reference, 10, "Finder::ClassMethods"),
            ("Leaf", Reference, 14, "Base"),
            ("Leaf#run", Call, 14, "Base#track"),
            ("Base.inherited", Reference, 16, "Tracking"),
            ("Hooked.included", Reference, 3, "Hooked::ClassMethods"),
            ("Item", Reference, 1, "Base"),
            ("Item", Reference, 2, "Hooked"),
            ("Item.load", Call, 3, "Hooked::ClassMethods#find"),
            ("Paged.included", Reference, 2, "Paged::ClassMethods"),
            ("lib/page.rb", Import, 1, "lib/paged.rb"),
            ("Page", Reference, 2, "Base"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("Page", Reference, 3, "Paged"),
            ("Page.load", Call, 4, "find"),
        ],
    );
}

/// A module the superclass includes is skipped by the subclass's own
/// `include` only when that superclass's `include` has run: above it in
/// the same file, in a file it requires, or in the one file defining the
/// superclass. Ruby 3.3: `Sub1.new.run` is `:a`, `Sub2.new.run` `:b` with
/// main.rb's order (`[Sub2, B, A, Base2, B]`), `:a` the other way round.
#[test]
fn c10_ruby_ancestors_a_superclass_include_counts_once_it_has_run() {
    // B4
    let rows = index(&[
        (
            "lib/mods.rb",
            "module A; def t; :a; end; end\nmodule B; def t; :b; end; end\n",
        ),
        (
            "lib/base1.rb",
            "require_relative \"mods\"\nclass Base1; include B; end\n",
        ),
        (
            "lib/sub1.rb",
            "require_relative \"base1\"
class Sub1 < Base1; include A; include B; def run; t; end; end
",
        ),
        (
            "lib/base2.rb",
            "require_relative \"mods\"\nclass Base2; end\n",
        ),
        (
            "lib/base2_ext.rb",
            "require_relative \"base2\"\nclass Base2; include B; end\n",
        ),
        (
            "lib/sub2.rb",
            "require_relative \"base2\"
class Sub2 < Base2; include A; include B; def run; t; end; end
",
        ),
        (
            "main.rb",
            "require_relative \"lib/sub2\"\nrequire_relative \"lib/base2_ext\"\n",
        ),
    ]);
    let sub1 = resolved(&rows)
        .into_iter()
        .filter(|row| row.0.starts_with("Sub"))
        .collect::<std::collections::BTreeSet<Row>>();
    assert_eq!(
        sub1,
        labelled(&[
            ("Sub1", Reference, 2, "Base1"),
            ("Sub1", Reference, 2, "A"),
            ("Sub1", Reference, 2, "B"),
            ("Sub1#run", Call, 2, "A#t"),
            ("Sub2", Reference, 2, "A"),
            ("Sub2", Reference, 2, "B"),
        ])
    );
    // `Base2` opens in two files: no one definition to point at.
    assert_unresolved(
        &rows,
        &[("Sub2", Reference, 2, "Base2"), ("Sub2#run", Call, 2, "t")],
    );
}

/// A module's `include` that runs after a class included the module is
/// carried to the class by Ruby 3.3 after the module, and the class's own
/// `include` of the same module in between is not skipped: `K0.ancestors`
/// is `[K0, M0, M2, M0]`, and `K0.new.probe` and `K1.new.probe1` are
/// `:m0`. Where the late step lands is not read, so a lookup through it
/// abstains, as it does when the module reopens in a file loaded in an
/// order the graph cannot tell (`N2`); one in the one file defining the
/// module has run when the class names it (`F2`), and one above the
/// class's in the same file has too (`E2`) (R12-02).
#[test]
fn c10_ruby_ancestors_a_module_include_counts_for_its_includers_once_it_has_run() {
    // R12-02
    let rows = index(&[
        (
            "lib/k.rb",
            "module M0; def a; :m0; end; end
module M2; def a; :m2; end; end
class K0
  include M2
  include M0
  def probe; a; end
end
class K1 < K0; def probe1; a; end; end
module M2; include M0; end
module E0; def e; end; end
module E2; include E0; end
class KE; include E2; def run; e; end; end
",
        ),
        (
            "lib/n.rb",
            "module N0; def a; :n0; end; end
module N2; def a; :n2; end; end
class KN; include N2; include N0; def run; a; end; end
",
        ),
        ("lib/n2.rb", "module N2; include N0; end\n"),
        ("lib/f0.rb", "module F0; def f; end; end\n"),
        ("lib/f2.rb", "module F2; include F0; end\n"),
        ("lib/kf.rb", "class KF; include F2; def run; f; end; end\n"),
    ]);
    let calls: std::collections::BTreeSet<Row> = resolved(&rows)
        .into_iter()
        .filter(|row| row.1 == Call)
        .collect();
    assert_eq!(
        calls,
        labelled(&[("KE#run", Call, 12, "E0#e"), ("KF#run", Call, 1, "F0#f")])
    );
    assert_unresolved(
        &rows,
        &[
            ("K0#probe", Call, 6, "a"),
            ("K1#probe1", Call, 8, "a"),
            ("KN#run", Call, 3, "a"),
        ],
    );
}

/// A hook that mixes its own module back in, two hooks that mix each other
/// in, and two concerns that include each other make Ruby 3.3 repeat the
/// `include` (or `extend`) until `SystemStackError`: what the includer
/// holds is unknown from there, and indexing goes on (R5-01). The cycles
/// that end on their own (mutual includes, a superclass cycle, `extend
/// self` with `include self`) still index.
#[test]
fn c10_ruby_ancestors_a_mixin_played_inside_itself_is_cut() {
    // R5-01
    let rows = index(&[
        (
            "lib/again.rb",
            "module Again
  def self.included(base)
    base.include(Again)
  end
  def ping; end
end
class K1
  include Again
  def run; ping; end
end
",
        ),
        (
            "lib/pair.rb",
            "module M1
  def self.included(base); base.include(M2); end
  def m1; end
end
module M2
  def self.included(base); base.include(M1); end
end
class K2
  include M1
  def run; m1; end
end
",
        ),
        (
            "lib/extended.rb",
            "module E1
  def self.extended(base); base.extend(E2); end
  def e1; end
end
module E2
  def self.extended(base); base.extend(E1); end
end
module J
  def self.extended(base); base.extend(J); end
  def j; end
end
class K3
  extend E1
  extend J
  def self.run; e1; j; end
end
",
        ),
        (
            "lib/concerns.rb",
            "module CA
  extend ActiveSupport::Concern
  def ca; end
end
module CB
  extend ActiveSupport::Concern
  include CA
end
module CA
  include CB
end
class K4
  include CA
  def run; ca; end
end
",
        ),
        (
            "lib/ends.rb",
            "module A; include B; def a; b; end; end
module B; include A; def b; a; end; end
module C; include C; extend C; prepend C; def c; c; end; end
class X < Y; def x; y; end; end
class Y < X; def y; x; end; end
module S; extend self; include S; def s; s; end; end
class I1; def self.inherited(s); super; end; end
class I2 < I1; def self.inherited(s); super; s.include(I2); end; end
class I3 < I2; def r; r; end; end
",
        ),
    ]);
    let calls: Vec<Row> = resolved(&rows)
        .into_iter()
        .filter(|row| row.1 == Call && row.0.starts_with('K'))
        .collect();
    assert_eq!(calls, vec![]);
    assert_unresolved(
        &rows,
        &[
            ("K1#run", Call, 9, "ping"),
            ("K2#run", Call, 10, "m1"),
            ("K3.run", Call, 15, "e1"),
            ("K3.run", Call, 15, "j"),
            ("K4#run", Call, 14, "ca"),
        ],
    );
}

/// A walk that meets the depth cap may hide past the cut a hook, a
/// definition or a constant of the graph: a hook found nowhere before it
/// is unknown, and no lookup through it proves anything (R6-04). Ruby
/// 3.3: `C1.inherited` prepends `M` to every subclass, so `C33.new.run`
/// is `:m`.
#[test]
fn c10_ruby_ancestors_a_walk_past_the_cap_hides_its_hooks() {
    // R6-04
    let mut text = "module M; def save; :m; end; end
class C0; end
class C1 < C0; def self.inherited(sub); super; sub.prepend(M); end; end
"
    .to_owned();
    for i in 2..34 {
        text.push_str(&format!("class C{i} < C{}; end\n", i - 1));
    }
    text.push_str("class C33; def save; :c33; end; def run; save; end; end\n");
    let rows = index(&[("lib/k.rb", text.as_str())]);
    assert_unresolved(&rows, &[("C33#run", Call, 36, "save")]);
}

/// Chains nested past the depth cap end in an unknown step, without
/// exhausting the stack, and a call on `self` in a class or module some
/// object holds past the cut proves nothing: Ruby 3.3 runs `C99#h` for
/// `C99.new.run` and `K#h` for `K.new.run` (R5-01).
#[test]
fn c10_ruby_ancestors_chains_past_the_cap_end_in_an_unknown_step() {
    // R5-01
    let deep = |count: usize, line: &dyn Fn(usize) -> String| -> String {
        (1..count).map(line).collect::<Vec<_>>().join("\n") + "\n"
    };
    let hooks = format!(
        "module H0; def h; end; end\n{}class KH; include H{}; def run; h; end; end\n",
        deep(2000, &|i| format!(
            "module H{i}; def self.included(b); b.include(H{}); end; end",
            i - 1
        )),
        1999
    );
    let includes = format!(
        "module N0; def h; end; end\n{}class KN; include N{}; def run; h; end; end\n",
        deep(2000, &|i| format!("module N{i}; include N{}; end", i - 1)),
        1999
    );
    let concerns = format!(
        "module Q0; extend ActiveSupport::Concern; def h; end; end\n{}class KQ; include Q{}; def run; h; end; end\n",
        deep(2000, &|i| format!(
            "module Q{i}; extend ActiveSupport::Concern; include Q{}; end",
            i - 1
        )),
        1999
    );
    let classes = format!(
        "class C0; def run; h; end; def h; end; end\n{}class C99; def h; end; end\n",
        deep(2000, &|i| format!("class C{i} < C{}; end", i - 1)),
    );
    let modules = format!(
        "module M0; def run; h; end; def h; end; end\n{}class K; include M99; def h; end; end\n",
        deep(100, &|i| format!("module M{i}; include M{}; end", i - 1)),
    );
    let rows = index(&[
        ("lib/hooks.rb", hooks.as_str()),
        ("lib/includes.rb", includes.as_str()),
        ("lib/concerns.rb", concerns.as_str()),
        ("lib/classes.rb", classes.as_str()),
        ("lib/modules.rb", modules.as_str()),
    ]);
    let calls: Vec<Row> = resolved(&rows)
        .into_iter()
        .filter(|row| row.1 == Call)
        .collect();
    assert_eq!(calls, vec![]);
    assert_unresolved(
        &rows,
        &[
            ("KH#run", Call, 2001, "h"),
            ("KN#run", Call, 2001, "h"),
            ("KQ#run", Call, 2001, "h"),
            ("C0#run", Call, 1, "h"),
            ("M0#run", Call, 1, "h"),
        ],
    );
}
