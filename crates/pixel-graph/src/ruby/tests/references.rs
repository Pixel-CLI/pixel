// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Methods named rather than called: aliases and delegators (generated
//! methods), callbacks, `send`.

use pixel_model::EdgeKind::{Call, Reference};

use super::{assert_unresolved, index, labelled, resolved};

/// Readers, writers, aliases and delegators are methods of their owner: a
/// call reaches them, an alias or a delegator references the method it
/// copies or forwards to on its own class, never another class's
/// (`Billing::Account`, `Other`). `self.token = v` calls `token=`, which
/// `attr_reader :token` does not define. v1
/// `ruby_generated_methods_should_be_navigable_after_full_and_incremental_builds`
/// (full-build form, with `other.rb`).
#[test]
fn c10_ruby_aliasing_generated_methods_are_called_and_reference_their_original() {
    let rows = index(&[
        (
            "app/models/admin/account.rb",
            r#"module Admin
  class Account
    attr_reader :token
    attr_writer :secret
    attr_accessor :name, "email"
    # A computed name, `attr_reader(*FIELDS)`, is in other.rb.
    # So is `attr_reader :"dyn_#{x}"`.
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
        ),
        (
            "app/models/billing/account.rb",
            "module Billing\n  class Account\n    attr_accessor :name\n    def owner; end\n  end\nend\n",
        ),
        (
            "app/models/other.rb",
            "class Other\n  attr_accessor :name\n  def owner; end\n  def address; end\n  attr_reader(*FIELDS)\n  attr_reader :\"dyn_#{x}\"\n  def run; owner; end\nend\n",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Admin::Account#label", Reference, 9, "Admin::Account#name"),
            ("Admin::Account#title", Reference, 10, "Admin::Account#name"),
            (
                "Admin::Account#street",
                Reference,
                12,
                "Admin::Account#address"
            ),
            (
                "Admin::Account#city",
                Reference,
                12,
                "Admin::Account#address"
            ),
            (
                "Admin::Account#owner_email",
                Reference,
                13,
                "Admin::Account#owner"
            ),
            (
                "Admin::Account#home_zip",
                Reference,
                14,
                "Admin::Account#address"
            ),
            ("Admin::Account#rename", Call, 32, "Admin::Account#name="),
            ("Admin::Account#rename", Call, 33, "Admin::Account#secret="),
            ("Admin::Account#rename", Call, 35, "Admin::Account#name"),
            ("Admin::Account#rename", Call, 36, "Admin::Account#label"),
            ("Admin::Account#bump", Call, 41, "Admin::Account#name="),
            ("Admin::Account#bump", Call, 41, "Admin::Account#name"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("Admin::Account#rename", Call, 34, "self.token="),
            ("Admin::Account#rename", Call, 37, "self.class.registry"),
            ("Admin::Account.active", Call, 18, "where"),
            // A computed name may be any name, `owner` too (B7).
            ("Other#run", Call, 7, "owner"),
        ],
    );
}

/// A callback names an instance method of the class declaring it: its
/// positional symbols and its `if:`, never an option value (`only:
/// :show`), another class's method (`foreign`) or a class method
/// (`class_only`). v1
/// `ruby_callbacks_should_keep_their_owner_through_full_and_incremental_builds`
/// (full-build form, with `third.rb`) and
/// `ruby_callback_references_should_resolve_only_the_declaring_owners_instance_method`.
#[test]
fn c10_ruby_dynamic_dispatch_a_callback_names_an_instance_method_of_its_class() {
    let rows = index(&[
        (
            "record.rb",
            "class Record
  validate :check, if: :ready?
  before_action :check, only: :show
  after_commit :foreign
  after_save :class_only
  def check; end
  def ready?; true; end
  def show; end
  def self.class_only; end
end
",
        ),
        (
            "other.rb",
            "class Other\n  def check; end\n  def foreign; end\nend\n",
        ),
        ("third.rb", "class Third\n  def ready?; end\nend\n"),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Record", Reference, 2, "Record#check"),
            ("Record", Reference, 2, "Record#ready?"),
            ("Record", Reference, 3, "Record#check"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("Record", Reference, 4, "foreign"),
            ("Record", Reference, 5, "class_only"),
        ],
    );
}

/// A callback's method may be defined by a reopening in another file: one
/// definition is its target; a second one, in a third file, leaves it
/// unresolved (decision 5). v1
/// `ruby_callbacks_should_relink_a_later_definition_and_a_rewritten_target_file`
/// and the `reopened.rb` step of
/// `ruby_callbacks_should_keep_their_owner_through_full_and_incremental_builds`.
#[test]
fn c10_ruby_dynamic_dispatch_a_callback_reaches_a_method_its_reopening_defines() {
    let declared = ("record.rb", "module Rules\n  validate :check\nend\n");
    let defined = ("rules.rb", "module Rules\n  def check; true; end\nend\n");
    let rows = index(&[declared, defined]);
    assert_eq!(
        resolved(&rows),
        labelled(&[("Rules", Reference, 2, "Rules#check")])
    );
    let redefined = ("patch.rb", "module Rules\n  def check; false; end\nend\n");
    let rows = index(&[declared, defined, redefined]);
    assert_eq!(resolved(&rows), labelled(&[]));
    assert_unresolved(&rows, &[("Rules", Reference, 2, "check")]);
}

/// `send(:m)` names a method of whatever `self` is where it is written: the
/// class in its body and in `def self.run`, an instance in `def run`; only
/// its first argument, and only on no receiver or `self`. A Python file's
/// `check` is no candidate. v1
/// `ruby_send_should_follow_self_kind_without_linking_data_symbols` (with
/// `other.py` and `other.rb`).
#[test]
fn c10_ruby_dynamic_dispatch_send_names_a_method_of_the_side_self_is_on() {
    let rows = index(&[
        (
            "record.rb",
            "class Record
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
",
        ),
        ("other.py", "class Record:\n    def check(self): pass\n"),
        ("other.rb", "class Other\n  def check; end\nend\n"),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Record", Reference, 2, "Record.check"),
            ("Record#run", Reference, 7, "Record#check"),
            ("Record#run", Reference, 8, "Record#check"),
            ("Record.run", Reference, 12, "Record.check"),
        ])
    );
    assert_unresolved(&rows, &[("Record#run", Call, 9, "other.send")]);
}

/// An alias copies the body its original has when the alias runs: the
/// definition above it in the file, not a later redefinition; with none
/// above, the one definition on the owner's lookup chain, or nothing when
/// it has several. Ruby 3.3: `Svc.new.save` is `:original`, through
/// `old_save`.
#[test]
fn c10_ruby_aliasing_an_alias_copies_the_definition_above_it() {
    // D05
    let rows = index(&[
        (
            "lib/svc.rb",
            "class Svc
  def save; :original; end
  alias_method :old_save, :save
  alias old_alias save
  def save; old_save; end
end
",
        ),
        (
            "lib/base.rb",
            "class Base\n  def run; end\n  def go; end\nend\n",
        ),
        (
            "lib/late.rb",
            "class Late < Base
  alias_method :old_run, :run
  def run; end
  alias_method :old_go, :go
end
",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Svc#old_save", Reference, 3, "Svc#save"),
            ("Svc#old_alias", Reference, 4, "Svc#save"),
            ("Svc#save", Call, 5, "Svc#old_save"),
            ("Late", Reference, 1, "Base"),
            ("Late#old_go", Reference, 4, "Base#go"),
        ])
    );
    let svc = rows.iter().find(|r| r.file.path == "lib/svc.rb").unwrap();
    for line in [3, 4] {
        let dst = svc
            .edges
            .iter()
            .find(|e| e.line == line && e.kind == Reference);
        assert_eq!(
            dst.and_then(|e| e.dst.as_deref()),
            Some("lib/svc.rb#Svc#save#method"),
            "line {line} copies line 2's `save`"
        );
    }
    assert_unresolved(&rows, &[("Late#old_run", Reference, 2, "run")]);
}

/// An alias copies what lookup from its owner finds when it runs: a module
/// prepended above it comes before the owner's own definition, one
/// prepended below it does not, and one prepended in another file may load
/// before or after. Ruby 3.3: `K.new.run` is `[:loud, :loud]`, `L.new.run`
/// `:l`; `K2.new.run` is `:k2` or `:loud` with the load order.
#[test]
fn c10_ruby_aliasing_an_alias_copies_a_prepended_method_above_it() {
    // B1
    let rows = index(&[
        (
            "lib/k.rb",
            "module Loud; def save; :loud; end; end
class K
  prepend Loud
  def save; :k; end
  alias_method :old_save, :save
  alias old2 save
  def run; [old_save, old2]; end
end
class L
  def save; :l; end
  alias_method :old_save, :save
  prepend Loud
  def run; old_save; end
end
",
        ),
        (
            "lib/k2.rb",
            "class K2
  def save; :k2; end
  alias_method :old_save, :save
end
",
        ),
        ("lib/k2_ext.rb", "class K2; prepend Loud; end\n"),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("K", Reference, 3, "Loud"),
            ("K#old_save", Reference, 5, "Loud#save"),
            ("K#old2", Reference, 6, "Loud#save"),
            ("K#run", Call, 7, "K#old_save"),
            ("K#run", Call, 7, "K#old2"),
            ("L#old_save", Reference, 11, "L#save"),
            ("L", Reference, 12, "Loud"),
            ("L#run", Call, 13, "L#old_save"),
            ("K2", Reference, 1, "Loud"),
        ])
    );
    assert_unresolved(&rows, &[("K2#old_save", Reference, 3, "save")]);
}
