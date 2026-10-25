// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The Rails DSL rules' rows, one test per class of input. The fixtures
//! named after a v1 test are its sources byte for byte, and the rows they
//! expect are v1's (`crates/pixel-graph/src/extract/ruby_*.rs` and
//! `tests/all/ruby_*.rs`, and the dumps of v1 on the same bytes), mapped to
//! the MVP: the `File` symbol for v1's `script`, associations and ancestors
//! as their own sites, the cref added.

use super::CALLBACK_METHODS;
use crate::mvp_index::ruby::extract::testing::{
    ancestors, associations, calls, references, rows, symbols,
};

/// Each method of the closed callback list references the symbols it is
/// given in a class or module body, and nowhere else: not at the top level,
/// in a method, or in `class << self`. v1
/// `ruby_callbacks_should_cover_the_documented_dsl_and_only_class_or_module_bodies`.
#[test]
fn every_callback_of_the_list_is_read_in_a_class_or_module_body_only() {
    let list = [
        "before_action",
        "after_action",
        "around_action",
        "prepend_before_action",
        "prepend_after_action",
        "prepend_around_action",
        "append_before_action",
        "append_after_action",
        "append_around_action",
        "skip_before_action",
        "skip_after_action",
        "skip_around_action",
        "validate",
        "before_validation",
        "after_validation",
        "before_save",
        "around_save",
        "after_save",
        "before_create",
        "around_create",
        "after_create",
        "before_update",
        "around_update",
        "after_update",
        "before_destroy",
        "around_destroy",
        "after_destroy",
        "after_initialize",
        "after_find",
        "after_touch",
        "before_commit",
        "after_commit",
        "after_rollback",
        "after_create_commit",
        "after_update_commit",
        "after_destroy_commit",
        "after_save_commit",
        "helper_method",
        "before_enqueue",
        "around_enqueue",
        "after_enqueue",
        "before_perform",
        "around_perform",
        "after_perform",
    ];
    assert_eq!(CALLBACK_METHODS, list);
    for method in list {
        let rows = rows(
            "record.rb",
            &format!(
                "{method} :top\nmodule Rules\n  {method} :check\nend\nclass Record\n  def run\n    {method} :inside\n  end\n  def self.configure\n    {method} :singleton_method\n  end\n  class << self\n    {method} :singleton\n  end\nend\n"
            ),
        );
        assert_eq!(
            references(&rows),
            [format!("3 Rules: check ({method}) [Rules]")],
            "{method}"
        );
    }
}

/// Interpolated names, method bodies and modules reference nothing. v1
/// `associations_should_record_their_class_path_from_plain_and_quoted_names`.
#[test]
fn associations_name_their_model_class_from_plain_and_quoted_names() {
    let rows = rows(
        "app/models/order.rb",
        r#"class Order
  has_many :"line_items"
  belongs_to :customer
  has_one :invoice, class_name: :"Billing::Invoice"
  has_many :"x#{y}"
  def build
    has_many :nested
  end
end
module Concern
  has_many :in_module
end
"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file app/models/order.rb 1-13",
            "class Order 1-9 | class Order",
            "method Order#build 6-8 | def build",
            "module Concern 10-12 | module Concern",
        ]
    );
    assert_eq!(
        calls(&rows),
        [
            "2 Order: has_many [Order]",
            "3 Order: belongs_to [Order]",
            "4 Order: has_one [Order]",
            "5 Order: has_many [Order]",
            "5 Order: y [Order]",
            "7 Order#build: has_many [Order]",
            "11 Concern: has_many [Concern]",
        ]
    );
    assert_eq!(
        associations(&rows),
        [
            "2 Order: has_many LineItem [Order]",
            "3 Order: belongs_to Customer [Order]",
            "4 Order: has_one Billing::Invoice [Order]",
        ]
    );
}

/// `class_name:` as written, else the singular of a collection; nothing
/// for `through:` without `class_name:`, `polymorphic: true` or a computed
/// class name.
#[test]
fn an_association_names_the_class_its_options_and_name_give() {
    let rows = rows(
        "app/a5.rb",
        r#"class Order < ApplicationRecord
  has_many :items, through: :x
  has_many :parts, class_name: "Foo::Part", through: :x
  belongs_to :owner, polymorphic: true
  has_many :people
  has_many :statuses
  has_many :categories
  has_many :boxes
  has_many :addresses
  has_many :aliases
  has_many :buses
  has_many :wishes
  has_many :matches
  has_one :address
  has_and_belongs_to_many :tags
  has_many :line_items, -> { order(:x) }
  belongs_to :author, class_name: "User"
  has_many :subs, :class_name => "Sub"
  has_many :dyn, class_name: name
  has_many "strs"
  has_many :admin_users, class_name: "::Admin::User"
  with_options dependent: :destroy do
    has_many :nested_items
  end
  has_one :thing, polymorphic: false
  belongs_to :user_profile
end
"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file app/a5.rb 1-28",
            "class Order 1-27 | class Order < ApplicationRecord",
        ]
    );
    assert_eq!(
        calls(&rows),
        [
            "2 Order: has_many [Order]",
            "3 Order: has_many [Order]",
            "4 Order: belongs_to [Order]",
            "5 Order: has_many [Order]",
            "6 Order: has_many [Order]",
            "7 Order: has_many [Order]",
            "8 Order: has_many [Order]",
            "9 Order: has_many [Order]",
            "10 Order: has_many [Order]",
            "11 Order: has_many [Order]",
            "12 Order: has_many [Order]",
            "13 Order: has_many [Order]",
            "14 Order: has_one [Order]",
            "15 Order: has_and_belongs_to_many [Order]",
            "16 Order: has_many [Order]",
            "16 Order: order [Order]",
            "17 Order: belongs_to [Order]",
            "18 Order: has_many [Order]",
            "19 Order: has_many [Order]",
            "19 Order: name [Order]",
            "20 Order: has_many [Order]",
            "21 Order: has_many [Order]",
            "22 Order: with_options [Order]",
            "23 Order: has_many [Order]",
            "25 Order: has_one [Order]",
            "26 Order: belongs_to [Order]",
        ]
    );
    assert_eq!(
        ancestors(&rows),
        ["1 Order: superclass ApplicationRecord []",]
    );
    assert_eq!(
        associations(&rows),
        [
            "3 Order: has_many Foo::Part [Order]",
            "5 Order: has_many People [Order]",
            "6 Order: has_many Status [Order]",
            "7 Order: has_many Category [Order]",
            "8 Order: has_many Box [Order]",
            "9 Order: has_many Address [Order]",
            "10 Order: has_many Alias [Order]",
            "11 Order: has_many Bus [Order]",
            "12 Order: has_many Wish [Order]",
            "13 Order: has_many Match [Order]",
            "14 Order: has_one Address [Order]",
            "15 Order: has_and_belongs_to_many Tag [Order]",
            "16 Order: has_many LineItem [Order]",
            "17 Order: belongs_to User [Order]",
            "18 Order: has_many Sub [Order]",
            "20 Order: has_many Str [Order]",
            "21 Order: has_many ::Admin::User [Order]",
            "23 Order: has_many NestedItem [Order]",
            "25 Order: has_one Thing [Order]",
            "26 Order: belongs_to UserProfile [Order]",
        ]
    );
}

/// No irregular or uncountable words (`news` gives `New`), and a
/// `class_name:` path is used as written.
#[test]
fn association_inflections_follow_the_regular_english_rules() {
    let rows = rows(
        "app/c2.rb",
        r#"class O
  has_many :user_statuses
  has_many :status
  has_many :html_parsers
  has_many :admin_users, class_name: "admin/user"
  has_many :glasses
  has_many :news
  has_many :data
end
"#,
    );
    assert_eq!(
        symbols(&rows),
        ["file app/c2.rb 1-10", "class O 1-9 | class O",]
    );
    assert_eq!(
        calls(&rows),
        [
            "2 O: has_many [O]",
            "3 O: has_many [O]",
            "4 O: has_many [O]",
            "5 O: has_many [O]",
            "6 O: has_many [O]",
            "7 O: has_many [O]",
            "8 O: has_many [O]",
        ]
    );
    assert_eq!(
        associations(&rows),
        [
            "2 O: has_many UserStatus [O]",
            "3 O: has_many Status [O]",
            "4 O: has_many HtmlParser [O]",
            "5 O: has_many admin/user [O]",
            "6 O: has_many Glass [O]",
            "7 O: has_many New [O]",
            "8 O: has_many Data [O]",
        ]
    );
}

/// Positional symbols and `if:`/`unless:` values; `only:`, `on:`, a
/// receiver and an empty call give nothing. v1
/// `ruby_callbacks_should_reference_methods_but_not_option_values_or_foreign_receivers`.
#[test]
fn callbacks_reference_methods_not_option_values_or_foreign_receivers() {
    let rows = rows(
        "record.rb",
        r#"class Record
  before_action :load, :authorize, only: :show
  validate :check, if: :ready?, unless: :blocked?
  after_commit :sync, on: :create
  foo.before_action :foreign
  self.before_action :explicit
  before_action()
  scope :active, -> { true }
end
"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file record.rb 1-10",
            "class Record 1-9 | class Record",
            "method Record.active 8-8 | scope :active, -> { true }",
        ]
    );
    assert_eq!(
        calls(&rows),
        [
            "2 Record: before_action [Record]",
            "3 Record: validate [Record]",
            "4 Record: after_commit [Record]",
            "5 Record: before_action on foo [Record]",
            "5 Record: foo [Record]",
            "6 Record: before_action on self [Record]",
            "7 Record: before_action [Record]",
        ]
    );
    assert_eq!(
        references(&rows),
        [
            "2 Record: load (before_action) [Record]",
            "2 Record: authorize (before_action) [Record]",
            "3 Record: check (validate) [Record]",
            "3 Record: ready? (validate) [Record]",
            "3 Record: blocked? (validate) [Record]",
            "4 Record: sync (after_commit) [Record]",
        ]
    );
}

/// The rows v1's
/// `ruby_callbacks_should_keep_their_owner_through_full_and_incremental_builds`
/// resolves.
#[test]
fn callbacks_reference_instance_and_class_methods_by_name() {
    let rows = rows(
        "record.rb",
        r#"class Record
  validate :check, if: :ready?
  before_action :check, only: :show
  after_commit :foreign
  after_save :class_only
  def check; end
  def ready?; true; end
  def show; end
  def self.class_only; end
end
"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file record.rb 1-11",
            "class Record 1-10 | class Record",
            "method Record#check 6-6 | def check; end",
            "method Record#ready? 7-7 | def ready?; true; end",
            "method Record#show 8-8 | def show; end",
            "method Record.class_only 9-9 | def self.class_only; end",
        ]
    );
    assert_eq!(
        calls(&rows),
        [
            "2 Record: validate [Record]",
            "3 Record: before_action [Record]",
            "4 Record: after_commit [Record]",
            "5 Record: after_save [Record]",
        ]
    );
    assert_eq!(
        references(&rows),
        [
            "2 Record: check (validate) [Record]",
            "2 Record: ready? (validate) [Record]",
            "3 Record: check (before_action) [Record]",
            "4 Record: foreign (after_commit) [Record]",
            "5 Record: class_only (after_save) [Record]",
        ]
    );
}

/// `:if =>` reads like `if:`; arrays, lambdas, strings and quoted symbols
/// name nothing; `with_options do` and a concern's `included do` keep the
/// class or module body for callbacks, but what `included do` declares is
/// the includer's, no method of the concern (B6). The `class_methods` call
/// is the concern's, not (by line, as in v1) the `ClassMethods` module's
/// it opens.
#[test]
fn a_callback_names_only_simple_symbols() {
    let rows = rows(
        "app/a4.rb",
        r#"class C
  before_action :a, :if => :b
  before_action :c, if: [:d, :e]
  before_action :f, if: -> { g }
  before_action "string_cb"
  before_action :"quoted_cb"
  after_action :h, unless: :i?, only: [:j]
  helper_method :k, :l
  with_options if: :m do
    before_save :n
  end
  around_action do |c, blk| blk.call end
end
module Concern
  included do
    before_action :in_included
    has_many :concern_items
    attr_reader :concern_attr
  end
  class_methods do
    attr_reader :cm_attr
    scope :cm_scope, -> {}
  end
end
"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file app/a4.rb 1-25",
            "class C 1-13 | class C",
            "module Concern 14-24 | module Concern",
            "module Concern::ClassMethods 20-23 | class_methods do",
            "method Concern::ClassMethods#cm_attr 21-21 | attr_reader :cm_attr",
            "method Concern::ClassMethods.cm_scope 22-22 | scope :cm_scope, -> {}",
        ]
    );
    assert_eq!(
        calls(&rows),
        [
            "2 C: before_action [C]",
            "3 C: before_action [C]",
            "4 C: before_action [C]",
            "4 C: g [C]",
            "5 C: before_action [C]",
            "6 C: before_action [C]",
            "7 C: after_action [C]",
            "8 C: helper_method [C]",
            "9 C: with_options [C]",
            "10 C: before_save [C]",
            "12 C: around_action [C]",
            "12 C: call on blk [C]",
            "15 Concern: included [Concern]",
            "16 Concern: before_action [Concern]",
            "17 Concern: has_many [Concern]",
            "18 Concern: attr_reader [Concern]",
            "20 Concern: class_methods [Concern]",
        ]
    );
    assert_eq!(
        references(&rows),
        [
            "2 C: a (before_action) [C]",
            "2 C: b (before_action) [C]",
            "3 C: c (before_action) [C]",
            "4 C: f (before_action) [C]",
            "7 C: h (after_action) [C]",
            "7 C: i? (after_action) [C]",
            "8 C: k (helper_method) [C]",
            "8 C: l (helper_method) [C]",
            "10 C: n (before_save) [C]",
            "16 Concern: in_included (before_action) [Concern]",
        ]
    );
}

/// A foreign or constant receiver, a class body's `class << self` and the
/// top level give nothing. One change from v1: `send(name, :dynamic)`
/// references nothing (MVP decision 6). v1
/// `ruby_send_should_reference_only_the_first_literal_symbol_on_self`.
#[test]
fn send_references_only_its_first_literal_symbol_on_self() {
    let rows = rows(
        "record.rb",
        r#"class Record
  def run
    send(:check, :data)
    self.public_send(:ready?, :other)
    object.send(:foreign)
    Record.send(:constant)
    send(name, :dynamic)
    send()
    send("string")
  end
  class << self
    send(:unknown_owner)
    def singleton_run
      send(:class_target)
    end
  end
end
send(:top_level)
"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file record.rb 1-19",
            "class Record 1-17 | class Record",
            "method Record#run 2-10 | def run",
            "method Record.singleton_run 13-15 | def singleton_run",
        ]
    );
    assert_eq!(
        calls(&rows),
        [
            "3 Record#run: send [Record]",
            "4 Record#run: public_send on self [Record]",
            "5 Record#run: send on object [Record]",
            "5 Record#run: object [Record]",
            "6 Record#run: send on Record [Record]",
            "7 Record#run: send [Record]",
            "7 Record#run: name [Record]",
            "8 Record#run: send [Record]",
            "9 Record#run: send [Record]",
            "12 Record: send [#<Class:Record>, Record]",
            "14 Record.singleton_run: send [#<Class:Record>, Record]",
            "18 record.rb: send []",
        ]
    );
    assert_eq!(
        references(&rows),
        [
            "3 Record#run: check (send) [Record]",
            "4 Record#run: ready? (public_send) [Record]",
            "14 Record.singleton_run: class_target (send) [#<Class:Record>, Record]",
        ]
    );
}

/// The rows v1's `ruby_send_should_follow_self_kind_without_linking_data_symbols`
/// resolves.
#[test]
fn send_references_from_a_class_body_and_from_methods() {
    let rows = rows(
        "record.rb",
        r#"class Record
  send(:check, :payload)
  def self.check; end
  def check; end
  def payload; end
  def run
    send(:check, :payload)
    self.public_send(:check)
    other.send(:payload)
  end
  def self.run
    public_send(:check)
  end
end
"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file record.rb 1-15",
            "class Record 1-14 | class Record",
            "method Record.check 3-3 | def self.check; end",
            "method Record#check 4-4 | def check; end",
            "method Record#payload 5-5 | def payload; end",
            "method Record#run 6-10 | def run",
            "method Record.run 11-13 | def self.run",
        ]
    );
    assert_eq!(
        calls(&rows),
        [
            "2 Record: send [Record]",
            "7 Record#run: send [Record]",
            "8 Record#run: public_send on self [Record]",
            "9 Record#run: send on other [Record]",
            "9 Record#run: other [Record]",
            "12 Record.run: public_send [Record]",
        ]
    );
    assert_eq!(
        references(&rows),
        [
            "2 Record: check (send) [Record]",
            "7 Record#run: check (send) [Record]",
            "8 Record#run: check (public_send) [Record]",
            "12 Record.run: check (public_send) [Record]",
        ]
    );
}

/// Superclass, `include`, `prepend`, `extend` and `self.include` in a class
/// or module body, `included:` inside a concern's `included do`; a
/// non-constant ancestor has no name; `class_methods do` defines
/// `ClassMethods`. v1
/// `ruby_mixins_should_record_each_literal_ancestor_with_its_owner_and_line`;
/// one change from v1: the calls of a superclass expression and the
/// `class_methods` call belong to the body they are written in, not (by
/// line) to the class or module they open.
#[test]
fn each_literal_ancestor_is_a_reference_of_its_owner() {
    let rows = rows(
        "app/models/a.rb",
        r#"module Admin
  class Order < ::Base
    include Trackable, Admin::Auditable
    prepend Loud
    extend Sortable
    self.include Kernelish
    include(plugin)
    include Module.new
    other.include Ignored
    def run
      include NotAnAncestor
    end
  end
  class Point < Struct.new(:x)
  end
  module Tracked
    extend ActiveSupport::Concern
    included do
      include Audited
      extend Finders
    end
    class_methods do
      def since; end
    end
  end
end
include TopLevel
"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file app/models/a.rb 1-28",
            "module Admin 1-26 | module Admin",
            "class Admin::Order 2-13 | class Order < ::Base",
            "method Admin::Order#run 10-12 | def run",
            "class Admin::Point 14-15 | class Point < Struct.new(:x)",
            "module Admin::Tracked 16-25 | module Tracked",
            "module Admin::Tracked::ClassMethods 22-24 | class_methods do",
            "method Admin::Tracked::ClassMethods#since 23-23 | def since; end",
        ]
    );
    assert_eq!(
        calls(&rows),
        [
            "3 Admin::Order: include [Admin::Order, Admin]",
            "4 Admin::Order: prepend [Admin::Order, Admin]",
            "5 Admin::Order: extend [Admin::Order, Admin]",
            "6 Admin::Order: include on self [Admin::Order, Admin]",
            "7 Admin::Order: include [Admin::Order, Admin]",
            "7 Admin::Order: plugin [Admin::Order, Admin]",
            "8 Admin::Order: include [Admin::Order, Admin]",
            "8 Admin::Order: new on Module [Admin::Order, Admin]",
            "9 Admin::Order: include on other [Admin::Order, Admin]",
            "9 Admin::Order: other [Admin::Order, Admin]",
            "11 Admin::Order#run: include [Admin::Order, Admin]",
            "14 Admin: new on Struct [Admin]",
            "17 Admin::Tracked: extend [Admin::Tracked, Admin]",
            "18 Admin::Tracked: included [Admin::Tracked, Admin]",
            "19 Admin::Tracked: include [Admin::Tracked, Admin]",
            "20 Admin::Tracked: extend [Admin::Tracked, Admin]",
            "22 Admin::Tracked: class_methods [Admin::Tracked, Admin]",
            "27 app/models/a.rb: include []",
        ]
    );
    assert_eq!(
        ancestors(&rows),
        [
            "2 Admin::Order: superclass ::Base [Admin]",
            // Last argument first, as Ruby inserts them (D10).
            "3 Admin::Order: include Admin::Auditable [Admin::Order, Admin]",
            "3 Admin::Order: include Trackable [Admin::Order, Admin]",
            "4 Admin::Order: prepend Loud [Admin::Order, Admin]",
            "5 Admin::Order: extend Sortable [Admin::Order, Admin]",
            "6 Admin::Order: include Kernelish [Admin::Order, Admin]",
            "7 Admin::Order: include - [Admin::Order, Admin]",
            "8 Admin::Order: include - [Admin::Order, Admin]",
            "14 Admin::Point: superclass - [Admin]",
            "17 Admin::Tracked: extend ActiveSupport::Concern [Admin::Tracked, Admin]",
            "19 Admin::Tracked: included:include Audited [Admin::Tracked, Admin]",
            "20 Admin::Tracked: included:extend Finders [Admin::Tracked, Admin]",
        ]
    );
}

/// `attr_*`, `alias_method`, `alias`, `delegate` with its prefixes and
/// `scope`, in a class and in `class << self`; a later `def` overrides a
/// reader; an alias or delegator references the method it forwards to.
/// v1 `ruby_declarations_should_generate_methods_of_their_owner_at_the_declaring_line`.
#[test]
fn declarations_generate_methods_of_their_owner_at_the_declaring_line() {
    let rows = rows(
        "app/models/admin/account.rb",
        r#"module Admin
  class Account
    attr_reader :token
    attr_writer :secret
    attr_accessor :name, "email"
    attr_reader(*FIELDS)
    attr_reader :"dyn_#{x}"
    attr_reader :status
    alias_method :label, :name
    alias title name
    alias $old_global $new_global
    delegate :street, :city, to: :address
    delegate :email, to: :owner, prefix: true
    delegate :zip, to: :address, prefix: :home
    delegate :currency, to: :@config
    delegate :size, to: :class
    delegate :x, to: :address, prefix: some_prefix
    scope :active, -> { where(active: true) }

    class << self
      attr_accessor :registry
    end

    def status
      @status || :new
    end

    def address; end
    def owner; end

    def rename(value)
      self.name = value
      self.secret = value
      self.token = value
      name
      label
      self.class.registry
    end

    def bump
      self.name += "!"
    end
  end
end
"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file app/models/admin/account.rb 1-45",
            "module Admin 1-44 | module Admin",
            "class Admin::Account 2-43 | class Account",
            "method Admin::Account#token 3-3 | attr_reader :token",
            "method Admin::Account#secret= 4-4 | attr_writer :secret",
            "method Admin::Account#name 5-5 | attr_accessor :name, \"email\"",
            "method Admin::Account#name= 5-5 | attr_accessor :name, \"email\"",
            "method Admin::Account#email 5-5 | attr_accessor :name, \"email\"",
            "method Admin::Account#email= 5-5 | attr_accessor :name, \"email\"",
            "method Admin::Account#label 9-9 | alias_method :label, :name",
            "method Admin::Account#title 10-10 | alias title name",
            "method Admin::Account#street 12-12 | delegate :street, :city, to: :address",
            "method Admin::Account#city 12-12 | delegate :street, :city, to: :address",
            "method Admin::Account#owner_email 13-13 | delegate :email, to: :owner, prefix: true",
            "method Admin::Account#home_zip 14-14 | delegate :zip, to: :address, prefix: :home",
            "method Admin::Account#currency 15-15 | delegate :currency, to: :@config",
            "method Admin::Account#size 16-16 | delegate :size, to: :class",
            "method Admin::Account.active 18-18 | scope :active, -> { where(active: true) }",
            "method Admin::Account.registry 21-21 | attr_accessor :registry",
            "method Admin::Account.registry= 21-21 | attr_accessor :registry",
            "method Admin::Account#status 24-26 | def status",
            "method Admin::Account#address 28-28 | def address; end",
            "method Admin::Account#owner 29-29 | def owner; end",
            "method Admin::Account#rename 31-38 | def rename(value)",
            "method Admin::Account#bump 40-42 | def bump",
        ]
    );
    assert_eq!(
        calls(&rows),
        [
            "6 Admin::Account: attr_reader [Admin::Account, Admin]",
            "7 Admin::Account: attr_reader [Admin::Account, Admin]",
            "7 Admin::Account: x [Admin::Account, Admin]",
            "17 Admin::Account: delegate [Admin::Account, Admin]",
            "17 Admin::Account: some_prefix [Admin::Account, Admin]",
            "18 Admin::Account.active: where [Admin::Account, Admin]",
            "32 Admin::Account#rename: name= on self [Admin::Account, Admin]",
            "33 Admin::Account#rename: secret= on self [Admin::Account, Admin]",
            "34 Admin::Account#rename: token= on self [Admin::Account, Admin]",
            "35 Admin::Account#rename: name [Admin::Account, Admin]",
            "36 Admin::Account#rename: label [Admin::Account, Admin]",
            "37 Admin::Account#rename: registry on self.class [Admin::Account, Admin]",
            "37 Admin::Account#rename: class on self [Admin::Account, Admin]",
            "41 Admin::Account#bump: name= on self [Admin::Account, Admin]",
            "41 Admin::Account#bump: name on self [Admin::Account, Admin]",
        ]
    );
    assert_eq!(
        references(&rows),
        [
            "9 Admin::Account#label: name (:alias) [Admin::Account, Admin]",
            "10 Admin::Account#title: name (:alias) [Admin::Account, Admin]",
            "12 Admin::Account#street: address (:delegate) [Admin::Account, Admin]",
            "12 Admin::Account#city: address (:delegate) [Admin::Account, Admin]",
            "13 Admin::Account#owner_email: owner (:delegate) [Admin::Account, Admin]",
            "14 Admin::Account#home_zip: address (:delegate) [Admin::Account, Admin]",
        ]
    );
}

/// A declaration evaluated in a method or at the top level has no known
/// owner. v1 `ruby_declarations_outside_an_owner_body_should_generate_nothing`.
#[test]
fn declarations_outside_an_owner_body_generate_nothing() {
    let rows = rows(
        "app/models/admin/account.rb",
        r#"attr_reader :top
def helper
  attr_reader :inside_def
end
class Box
  def build
    delegate :a, to: :b
  end
end
"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file app/models/admin/account.rb 1-10",
            "function helper 2-4 | def helper",
            "class Box 5-9 | class Box",
            "method Box#build 6-8 | def build",
        ]
    );
    assert_eq!(
        calls(&rows),
        [
            "1 app/models/admin/account.rb: attr_reader []",
            "3 helper: attr_reader []",
            "7 Box#build: delegate [Box]",
        ]
    );
}

/// The later `alias_method :title` and the `def heading` win: the first
/// `title` alias and the `heading` alias leave no reference behind. v1
/// `ruby_quoted_symbols_should_name_methods_and_overridden_declarations_forward_nothing`.
#[test]
fn quoted_names_generate_methods_and_an_overridden_declaration_forwards_nothing() {
    let rows = rows(
        "app/models/card.rb",
        r#"class Card
  attr_reader :"display_name"
  alias :"label" :"display_name"
  alias_method :title, :display_name
  alias_method :title, :label
  alias_method :heading, :display_name
  def heading
  end
  def footer
  end
  attr_reader :footer
  alias_method :"quoted_alias", :"display_name"
  delegate :"quoted_delegate", to: :"quoted_target", prefix: :"quoted_prefix"
  scope :"quoted_scope", -> { where(active: true) }
end
"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file app/models/card.rb 1-16",
            "class Card 1-15 | class Card",
            "method Card#display_name 2-2 | attr_reader :\"display_name\"",
            "method Card#label 3-3 | alias :\"label\" :\"display_name\"",
            "method Card#title 5-5 | alias_method :title, :label",
            "method Card#heading 7-8 | def heading",
            "method Card#footer 11-11 | attr_reader :footer",
            "method Card#quoted_alias 12-12 | alias_method :\"quoted_alias\", :\"display_name\"",
            "method Card#quoted_prefix_quoted_delegate 13-13 | delegate :\"quoted_delegate\", to: :\"quoted_target\", prefix: :\"quoted_prefix\"",
            "method Card.quoted_scope 14-14 | scope :\"quoted_scope\", -> { where(active: true) }",
        ]
    );
    assert_eq!(calls(&rows), ["14 Card.quoted_scope: where [Card]",]);
    assert_eq!(
        references(&rows),
        [
            "3 Card#label: display_name (:alias) [Card]",
            "5 Card#title: label (:alias) [Card]",
            "12 Card#quoted_alias: display_name (:alias) [Card]",
            "13 Card#quoted_prefix_quoted_delegate: quoted_target (:delegate) [Card]",
        ]
    );
}

/// A valueless `allow_nil:` is still an option, a valueless `to:` a dynamic
/// target and a valueless `prefix:` a dynamic prefix. v1
/// `delegate_options_written_as_shorthand_should_not_stop_generation`.
#[test]
fn delegate_options_written_as_shorthand_do_not_stop_generation() {
    let rows = rows(
        "app/models/box.rb",
        r#"class Box
  delegate(:width, to: :frame, allow_nil:)
  delegate(:height, to:)
  delegate(:depth, to: :frame, prefix:)
end
"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file app/models/box.rb 1-6",
            "class Box 1-5 | class Box",
            "method Box#width 2-2 | delegate(:width, to: :frame, allow_nil:)",
            "method Box#height 3-3 | delegate(:height, to:)",
        ]
    );
    assert_eq!(
        calls(&rows),
        [
            "2 Box#width: allow_nil [Box]",
            "3 Box#height: to [Box]",
            "4 Box: delegate [Box]",
            "4 Box: prefix [Box]",
        ]
    );
    assert_eq!(references(&rows), ["2 Box#width: frame (:delegate) [Box]",]);
}

/// Targets that are no method of the owner forward nothing; a dynamic name
/// or prefix, a wrong `alias_method` arity and a bare `attr_accessor`
/// generate nothing and stay calls; `module_function` copies a `def` as a
/// module method, not an accessor (D15; Ruby 3.3: `M.singleton_methods` is
/// `[:mf]`, and `mf_attr` a private instance method).
#[test]
fn delegate_alias_and_accessor_names_must_be_literal() {
    let rows = rows(
        "app/a6.rb",
        r#"class D
  delegate :a, to: "address"
  delegate :b, to: :b_q?
  delegate :c, to: :Foo
  delegate :d, to: :@@cv
  delegate :e, to: :address, prefix: false
  delegate :f, to: :address, prefix: nil
  delegate :g, to: :address, prefix: "pre"
  delegate :h
  delegate :i, :j, to: :x, prefix: true
  delegate :k, to: x, prefix: true
  alias_method :a1, :b1, :c1
  alias_method "a2", "b2"
  alias_method :a3, name
  alias_method :<<, :push
  alias + add
  alias :x? :y?
  attr_reader :"esc\tname"
  attr_reader :r1, *more, :r2
  attr_accessor
  scope :s1
  scope name, -> {}
  class << self
    scope :s2, -> {}
    alias_method :cm_alias, :cm
    delegate :cd, to: :thing
  end
end
module M
  module_function
  attr_reader :mf_attr
  def mf; end
end
"#,
    );
    assert_eq!(
        symbols(&rows),
        [
            "file app/a6.rb 1-34",
            "class D 1-28 | class D",
            "method D#a 2-2 | delegate :a, to: \"address\"",
            "method D#b 3-3 | delegate :b, to: :b_q?",
            "method D#c 4-4 | delegate :c, to: :Foo",
            "method D#d 5-5 | delegate :d, to: :@@cv",
            "method D#e 6-6 | delegate :e, to: :address, prefix: false",
            "method D#f 7-7 | delegate :f, to: :address, prefix: nil",
            "method D#pre_g 8-8 | delegate :g, to: :address, prefix: \"pre\"",
            "method D#x_i 10-10 | delegate :i, :j, to: :x, prefix: true",
            "method D#x_j 10-10 | delegate :i, :j, to: :x, prefix: true",
            "method D#a2 13-13 | alias_method \"a2\", \"b2\"",
            "method D#<< 15-15 | alias_method :<<, :push",
            "method D#+ 16-16 | alias + add",
            "method D#x? 17-17 | alias :x? :y?",
            "method D#r1 19-19 | attr_reader :r1, *more, :r2",
            "method D#r2 19-19 | attr_reader :r1, *more, :r2",
            "method D.s1 21-21 | scope :s1",
            "method D.s2 24-24 | scope :s2, -> {}",
            "method D.cm_alias 25-25 | alias_method :cm_alias, :cm",
            "method D.cd 26-26 | delegate :cd, to: :thing",
            "module M 29-33 | module M",
            "method M#mf_attr 31-31 | attr_reader :mf_attr",
            "method M.mf 32-32 | def mf; end",
            "method M#mf 32-32 | def mf; end",
        ]
    );
    assert_eq!(
        calls(&rows),
        [
            "9 D: delegate [D]",
            "11 D: delegate [D]",
            "11 D: x [D]",
            "12 D: alias_method [D]",
            "14 D: alias_method [D]",
            "14 D: name [D]",
            "18 D: attr_reader [D]",
            "19 D#r1: more [D]",
            "20 D: attr_accessor [D]",
            "22 D: scope [D]",
            "22 D: name [D]",
            "30 M: module_function [M]",
        ]
    );
    assert_eq!(
        references(&rows),
        [
            "2 D#a: address (:delegate) [D]",
            "3 D#b: b_q? (:delegate) [D]",
            "6 D#e: address (:delegate) [D]",
            "7 D#f: address (:delegate) [D]",
            "8 D#pre_g: address (:delegate) [D]",
            "10 D#x_i: x (:delegate) [D]",
            "10 D#x_j: x (:delegate) [D]",
            "13 D#a2: b2 (:alias) [D]",
            "15 D#<<: push (:alias) [D]",
            "16 D#+: add (:alias) [D]",
            "17 D#x?: y? (:alias) [D]",
            "25 D.cm_alias: cm (:alias) [#<Class:D>, D]",
            "26 D.cd: thing (:delegate) [#<Class:D>, D]",
        ]
    );
}

#[test]
fn audited_association_extensions_should_require_a_block_extend_option_or_extending_lambda() {
    use crate::mvp_index::ruby::{ASSOCIATION_EXTENDED, Context};
    for (declaration, extended) in [
        ("has_many :items", false),
        ("has_many :items, -> { where(open: true) }", false),
        ("has_many :items, -> { extending Extras }", true),
        ("has_many :items, extend: Extras", true),
        ("has_many :items, class_name: 'Item'", false),
        ("has_many :items do; def recent; end; end", true),
    ] {
        let found = rows("model.rb", &format!("class Box; {declaration}; end"));
        let association = found
            .edges
            .iter()
            .find(|e| e.kind == mvp_model::EdgeKind::Association)
            .unwrap();
        let context = Context::decode(&association.context);
        assert_eq!(
            context
                .extra
                .get(1)
                .is_some_and(|s| s.contains(ASSOCIATION_EXTENDED)),
            extended,
            "{declaration}: {context:?}"
        );
    }
}
