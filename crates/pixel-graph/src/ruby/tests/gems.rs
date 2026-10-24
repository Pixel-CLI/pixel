// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! A gem's module mixed into a module of the graph (#120): a step of its
//! own, which may define any name, but which neither keeps the module out
//! of the classes including it nor puts anything ahead of it there, so the
//! modules a class mixes in beside it stay known steps. The shape is
//! once-campfire's `User::Mentionable`, which includes
//! `ActionText::Attachable`.

use pixel_model::EdgeKind::{Call, Reference};

use super::{assert_unresolved, index, labelled, resolved};

const RECORD: (&str, &str) = (
    "app/models/application_record.rb",
    "class ApplicationRecord < ActiveRecord::Base\nend\n",
);

const BANNABLE: (&str, &str) = (
    "app/models/user/bannable.rb",
    "module User::Bannable\n  extend ActiveSupport::Concern\n\n  def ban\n  end\nend\n",
);

const ROLE: (&str, &str) = (
    "app/models/user/role.rb",
    "module User::Role\n  extend ActiveSupport::Concern\n\n  def administrator?\n  end\nend\n",
);

const USER: (&str, &str) = (
    "app/models/user.rb",
    "class User < ApplicationRecord
  include Bannable, Mentionable, Role

  def go
    ban
    to_attachable_partial_path
    initials
    administrator?
    attachable_sgid
  end

  def initials
  end
end
",
);

/// `include Bannable, Mentionable, Role` inserts `Role`, then `Mentionable`
/// with the gem's module after it, then `Bannable`, each right after
/// `User`: Ruby 4.0.7's ancestors are `[User, User::Bannable,
/// User::Mentionable, ActionText::Attachable, User::Role, ApplicationRecord,
/// ...]`. What the gem's module may define is behind `Mentionable`'s own
/// methods and `Bannable`'s, and ahead of `Role`'s: those abstain, as does
/// a name only the gem may define. Before #120, what the gem's module put
/// on `Mentionable`'s singleton class might have been an `included` hook
/// of `Mentionable`'s, and every call on `self` in `User` abstained.
#[test]
fn c10_ruby_gems_a_gem_module_in_a_module_leaves_the_steps_beside_it_known() {
    let rows = index(&[
        RECORD,
        BANNABLE,
        ROLE,
        USER,
        (
            "app/models/user/mentionable.rb",
            "module User::Mentionable\n  include ActionText::Attachable\n\n  def to_attachable_partial_path\n  end\nend\n",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("User", Reference, 1, "ApplicationRecord"),
            ("User", Reference, 2, "User::Bannable"),
            ("User", Reference, 2, "User::Mentionable"),
            ("User", Reference, 2, "User::Role"),
            ("User#go", Call, 5, "User::Bannable#ban"),
            (
                "User#go",
                Call,
                6,
                "User::Mentionable#to_attachable_partial_path"
            ),
            ("User#go", Call, 7, "User#initials"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("User#go", Call, 8, "administrator?"),
            ("User#go", Call, 9, "attachable_sgid"),
        ],
    );
}

/// Within the module, the gem's module is where its `include` puts it:
/// after the module, ahead of what the module included before it, behind
/// what it includes after. Ruby 4.0.7: `Late.ancestors` is `[Late, Helpers,
/// ActionText::Attachable]`, `Early.ancestors` `[Early,
/// ActionText::Attachable, Helpers]`, so only `Late`'s `helper` is proven.
#[test]
fn c10_ruby_gems_a_gem_module_hides_what_its_module_included_before_it() {
    let rows = index(&[
        (
            "lib/helpers.rb",
            "module Helpers\n  def helper\n  end\nend\n",
        ),
        (
            "lib/early.rb",
            "module Early\n  include Helpers\n  include ActionText::Attachable\nend\n",
        ),
        (
            "lib/late.rb",
            "module Late\n  include ActionText::Attachable\n  include Helpers\nend\n",
        ),
        (
            "lib/report.rb",
            "class Report\n  include Late\n\n  def run\n    helper\n  end\nend\n",
        ),
        (
            "lib/summary.rb",
            "class Summary\n  include Early\n\n  def run\n    helper\n  end\nend\n",
        ),
    ]);
    let calls: Vec<_> = resolved(&rows)
        .into_iter()
        .filter(|row| row.1 == Call)
        .collect();
    assert_eq!(
        calls,
        labelled(&[("Report#run", Call, 5, "Helpers#helper")])
            .into_iter()
            .collect::<Vec<_>>()
    );
    assert_unresolved(&rows, &[("Summary#run", Call, 5, "helper")]);
}

/// A gem's module prepended to a module comes before it wherever it is
/// included: `[User, ActionText::Attachable, User::Mentionable,
/// User::Role, ...]`. The module's own methods and those behind it
/// abstain; the class's own still resolve, since nothing the gem added to
/// `Mentionable` can put a step ahead of `User`.
#[test]
fn c10_ruby_gems_a_gem_module_prepended_to_a_module_stays_behind_the_class() {
    let rows = index(&[
        RECORD,
        ROLE,
        (
            "app/models/user/mentionable.rb",
            "module User::Mentionable\n  prepend ActionText::Attachable\n\n  def to_attachable_partial_path\n  end\nend\n",
        ),
        (
            "app/models/user.rb",
            "class User < ApplicationRecord
  include Mentionable, Role

  def go
    to_attachable_partial_path
    initials
    administrator?
  end

  def initials
  end
end
",
        ),
    ]);
    let calls: Vec<_> = resolved(&rows)
        .into_iter()
        .filter(|row| row.1 == Call)
        .collect();
    assert_eq!(
        calls,
        labelled(&[("User#go", Call, 6, "User#initials")])
            .into_iter()
            .collect::<Vec<_>>()
    );
    assert_unresolved(
        &rows,
        &[
            ("User#go", Call, 5, "to_attachable_partial_path"),
            ("User#go", Call, 7, "administrator?"),
        ],
    );
}

/// What the gem's module adds to the module's singleton class holds none
/// of the graph's modules: one the module extends after it is there.
/// Ruby 4.0.7: `Formatting.singleton_class.ancestors` starts `[#<Class:
/// Formatting>, Helpers, ...]`.
#[test]
fn c10_ruby_gems_a_module_extended_beside_a_gem_extension_is_there() {
    let rows = index(&[
        (
            "lib/helpers.rb",
            "module Helpers\n  def helper\n  end\nend\n",
        ),
        (
            "lib/formatting.rb",
            "module Formatting\n  include ActionText::Attachable\n  extend Helpers\n\n  def self.run\n    helper\n  end\nend\n",
        ),
    ]);
    let calls: Vec<_> = resolved(&rows)
        .into_iter()
        .filter(|row| row.1 == Call)
        .collect();
    assert_eq!(
        calls,
        labelled(&[("Formatting.run", Call, 6, "Helpers#helper")])
            .into_iter()
            .collect::<Vec<_>>()
    );
}

/// A module that `extend`s a gem's module may get any hook from it (R7):
/// `Hooky`'s `included(base)` may prepend to `User`, so no call on `self`
/// there is proven, not even `User`'s own.
#[test]
fn c10_ruby_gems_a_module_extending_a_gem_module_still_hides_its_includers() {
    let rows = index(&[
        RECORD,
        BANNABLE,
        ROLE,
        USER,
        (
            "app/models/user/mentionable.rb",
            "module User::Mentionable\n  extend Hooky\n\n  def to_attachable_partial_path\n  end\nend\n",
        ),
    ]);
    assert_unresolved(
        &rows,
        &[
            ("User#go", Call, 5, "ban"),
            ("User#go", Call, 6, "to_attachable_partial_path"),
            ("User#go", Call, 7, "initials"),
            ("User#go", Call, 8, "administrator?"),
        ],
    );
}
