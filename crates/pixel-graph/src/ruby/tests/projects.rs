// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The Ruby project boundary: a `require` resolves to a file of its own
//! project's load roots, never an external gem's path, a sibling project or
//! a suffix match; and an incremental run decides every kept site again.

use pixel_model::EdgeKind::{Call, Import, Reference};

use super::{Row, assert_unresolved, index, labelled, resolved, update};

/// A `require` searches the load roots of the project around the file (its
/// gemspec's `require_paths`, `lib` by default, then its path gems'); a
/// name an external gem owns (`rails`, `thor` through the lockfile,
/// `my_client` through `require:`) is that gem's, a sibling project's
/// files are not on the path, `require "./x"` depends on the working
/// directory; `require_relative` is relative to the file. A shell binstub
/// is no Ruby file; the lockfile is one, of facts only (decision 8; v1
/// stored no row for it). v1 `ruby_requires_should_resolve_inside_their_project_only`.
#[test]
fn c10_ruby_imports_a_require_resolves_inside_its_own_project_only() {
    let rows = index(&[
        (
            "Gemfile",
            "source \"https://rubygems.org\"\ngem \"rails\", \"~> 7.1\"\ngem \"my-client\", require: \"my_client\"\ngem \"billing\", path: \"engines/billing\"\n",
        ),
        (
            "Gemfile.lock",
            "PATH\n  remote: engines/billing\n  specs:\n    billing (0.1.0)\n\nGEM\n  remote: https://rubygems.org/\n  specs:\n    rails (7.1.0)\n      railties (= 7.1.0)\n    railties (7.1.0)\n      thor (~> 1.0)\n    thor (1.3.0)\n\nDEPENDENCIES\n  billing!\n  rails\n",
        ),
        (
            "Rakefile",
            "require_relative \"config/application\"\nRails.application.load_tasks\n",
        ),
        (
            "config/application.rb",
            "module Shop\n  class Application\n  end\nend\n",
        ),
        (
            "config/boot.rb",
            "ENV[\"BUNDLE_GEMFILE\"] ||= \"Gemfile\"\n",
        ),
        (
            "bin/rails",
            "#!/usr/bin/env ruby\nrequire_relative \"../config/boot\"\nrequire \"rails/commands\"\n",
        ),
        (
            "bin/dev",
            "#!/usr/bin/env sh\nexec foreman start -f Procfile.dev\n",
        ),
        (
            "app/models/order.rb",
            "require \"rails/generators\"
require \"thor\"
require \"my_client/api\"
require \"reports/pdf\"
require \"billing/invoice\"
require \"missing/thing\"
require \"./cwd_relative\"
require_relative \"../services/checkout\"
require_relative \"absent\"
class Order
end
",
        ),
        ("app/services/checkout.rb", "class Checkout\nend\n"),
        ("cwd_relative.rb", "X = 1\n"),
        ("lib/generators.rb", "module Generators\nend\n"),
        (
            "lib/rails/generators.rb",
            "module Rails\n  module Generators\n  end\nend\n",
        ),
        ("lib/thor.rb", "class Thor\nend\n"),
        ("lib/my_client/api.rb", "module MyClient\nend\n"),
        ("lib/reports/pdf.rb", "module Reports\nend\n"),
        (
            "engines/billing/billing.gemspec",
            "Gem::Specification.new do |s|\n  s.name = \"billing\"\n  s.require_paths = [\"lib\"]\nend\n",
        ),
        (
            "engines/billing/lib/billing/invoice.rb",
            "module Billing\nend\n",
        ),
        (
            "engines/billing/lib/billing/charge.rb",
            "require \"billing/invoice\"\nrequire \"reports/pdf\"\nmodule Billing\nend\n",
        ),
        ("other_app/Gemfile", "source \"https://rubygems.org\"\n"),
        ("other_app/lib/missing/thing.rb", "module Missing\nend\n"),
    ]);
    let order = "app/models/order.rb";
    let charge = "engines/billing/lib/billing/charge.rb";
    assert_eq!(
        resolved(&rows),
        labelled(&[
            (order, Import, 4, "lib/reports/pdf.rb"),
            (order, Import, 5, "engines/billing/lib/billing/invoice.rb"),
            (order, Import, 8, "app/services/checkout.rb"),
            (charge, Import, 1, "engines/billing/lib/billing/invoice.rb"),
            ("bin/rails", Import, 2, "config/boot.rb"),
            ("Rakefile", Import, 1, "config/application.rb"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            (order, Import, 1, "rails/generators"),
            (order, Import, 2, "thor"),
            (order, Import, 3, "my_client/api"),
            (order, Import, 6, "missing/thing"),
            (order, Import, 7, "./cwd_relative"),
            (order, Import, 9, "absent"),
            (charge, Import, 2, "reports/pdf"),
            ("bin/rails", Import, 3, "rails/commands"),
        ],
    );
    let paths: Vec<&str> = rows.iter().map(|r| r.file.path.as_str()).collect();
    assert!(paths.contains(&"bin/rails") && paths.contains(&"Gemfile.lock"));
    assert!(!paths.contains(&"bin/dev"), "{paths:?}");
}

/// A path gem's `require_paths` join the project's load roots; a file both
/// roots hold is in an order the manifests do not fix, so unresolved.
/// Without the path gem, its directory is a project of its own, off the
/// path. v1 `ruby_requires_should_follow_manifest_edits_incrementally`
/// (full-build forms) and `a_load_root_listed_twice_should_not_make_a_require_ambiguous`.
#[test]
fn c10_ruby_imports_a_path_gem_adds_load_roots_and_two_roots_holding_a_file_conflict() {
    let gemspec = (
        "engines/billing/billing.gemspec",
        "Gem::Specification.new do |s|\n  s.name = \"billing\"\n  s.require_paths = [\"lib\"]\nend\n",
    );
    let invoice = (
        "engines/billing/lib/billing/invoice.rb",
        "module Billing\nend\n",
    );
    let app_pdf = ("lib/reports/pdf.rb", "module Reports\nend\n");
    let gem_pdf = (
        "engines/billing/lib/reports/pdf.rb",
        "module Reports\nend\n",
    );
    let order = (
        "app/models/order.rb",
        "require \"billing/invoice\"\nrequire \"reports/pdf\"\n",
    );
    let with_gem = (
        "Gemfile",
        "source \"https://rubygems.org\"\ngem \"billing\", path: \"engines/billing\"\n",
    );
    let rows = index(&[with_gem, gemspec, invoice, app_pdf, gem_pdf, order]);
    assert_eq!(
        resolved(&rows),
        labelled(&[(
            "app/models/order.rb",
            Import,
            1,
            "engines/billing/lib/billing/invoice.rb"
        )])
    );
    assert_unresolved(&rows, &[("app/models/order.rb", Import, 2, "reports/pdf")]);
    let without_gem = ("Gemfile", "source \"https://rubygems.org\"\n");
    let rows = index(&[without_gem, gemspec, invoice, app_pdf, gem_pdf, order]);
    assert_eq!(
        resolved(&rows),
        labelled(&[("app/models/order.rb", Import, 2, "lib/reports/pdf.rb")])
    );
}

/// A gem the lockfile lists, transitive ones included, owns its require
/// names: dropped from the lockfile, `thor` is the project's own
/// `lib/thor.rb`. v1
/// `a_lockfile_edit_should_make_the_graph_stale_and_its_delta_re_resolve`
/// (its resolution part).
#[test]
fn c10_ruby_imports_the_lockfile_names_the_external_gems() {
    let gemfile = (
        "Gemfile",
        "source \"https://rubygems.org\"\ngem \"rails\", \"~> 7.1\"\n",
    );
    let thor = ("lib/thor.rb", "class Thor\nend\n");
    let order = ("app/models/order.rb", "require \"thor\"\n");
    let locked = (
        "Gemfile.lock",
        "GEM\n  remote: https://rubygems.org/\n  specs:\n    rails (7.1.0)\n      thor (~> 1.0)\n    thor (1.3.0)\n\nDEPENDENCIES\n  rails\n",
    );
    let rows = index(&[gemfile, locked, thor, order]);
    assert_eq!(resolved(&rows), labelled(&[]));
    let unlocked = (
        "Gemfile.lock",
        "GEM\n  remote: https://rubygems.org/\n  specs:\n    rails (7.1.0)\n\nDEPENDENCIES\n  rails\n",
    );
    let rows = index(&[gemfile, unlocked, thor, order]);
    assert_eq!(
        resolved(&rows),
        labelled(&[("app/models/order.rb", Import, 1, "lib/thor.rb")])
    );
}

/// A run that extracts only the changed files decides every kept site
/// again, so it stores what a full run stores: a reopening that redefines
/// the method takes the kept call's edge away, and removing it gives the
/// edge back. v1 `ruby_bare_call_should_lose_its_exact_edge_when_a_reopened_class_redefines_it`
/// and `ruby_bare_call_should_keep_its_own_class_method_across_incremental_updates`.
#[test]
fn c10_ruby_dynamic_dispatch_an_incremental_run_decides_kept_sites_again() {
    let svc = (
        "a.rb",
        "class Svc\n  def run\n    access_logs(user: 1)\n  end\n  def access_logs(user:)\n    [user]\n  end\nend\n",
    );
    let other = (
        "other.rb",
        "class Other\n  def access_logs\n    []\n  end\nend\n",
    );
    let patch = (
        "patch.rb",
        "class Svc\n  def access_logs(user:)\n    []\n  end\nend\n",
    );
    let first = index(&[svc, other]);
    let edge = labelled(&[("Svc#run", Call, 3, "Svc#access_logs")]);
    assert_eq!(resolved(&first), edge);
    let patched = update(&first, &[svc, other, patch]);
    assert_eq!(resolved(&patched), labelled(&[]));
    assert_eq!(resolved(&patched), resolved(&index(&[svc, other, patch])));
    let unpatched = update(&patched, &[svc, other]);
    assert_eq!(resolved(&unpatched), edge);
}

/// Each Ruby project (the nearest `Gemfile` or `*.gemspec` above a file)
/// is its own program: a site resolves to definitions of its own project
/// and of the local path gems its `Gemfile` names, never a sibling's. Ruby
/// 3.3, loading only `apps/b`: `User.find_by_token` raises NoMethodError,
/// `User.new.name` is `apps/b`'s; the root app reaches its path gem
/// `Billing::Invoice.total`; the gem does not reach the app's `Order`.
#[test]
fn c10_ruby_imports_a_site_resolves_inside_its_own_project_only() {
    // D13
    let rows = index(&[
        ("apps/a/Gemfile", ""),
        (
            "apps/a/app/models/user.rb",
            "class User\n  def self.find_by_token(t); end\nend\n",
        ),
        ("apps/b/Gemfile", ""),
        (
            "apps/b/app/models/user.rb",
            "class User\n  def name; end\nend\n",
        ),
        (
            "apps/b/app/services/login.rb",
            "class Login\n  def run\n    User.find_by_token(1)\n    User.new.name\n  end\nend\n",
        ),
        ("Gemfile", "gem \"billing\", path: \"engines/billing\"\n"),
        (
            "engines/billing/billing.gemspec",
            "Gem::Specification.new do |spec|\n  spec.name = \"billing\"\nend\n",
        ),
        (
            "engines/billing/lib/billing/invoice.rb",
            "module Billing\n  class Invoice\n    def self.total; end\n    def run; Order.new.run; end\n  end\nend\n",
        ),
        (
            "app/models/order.rb",
            "class Order\n  def run; Billing::Invoice.total; end\nend\n",
        ),
    ]);
    let calls: Vec<Row> = resolved(&rows)
        .into_iter()
        .filter(|row| row.1 == Call)
        .collect();
    assert_eq!(
        calls,
        vec![
            ("Login#run".to_owned(), Call, 4, "User#name".to_owned()),
            (
                "Order#run".to_owned(),
                Call,
                2,
                "Billing::Invoice.total".to_owned()
            ),
        ]
    );
    assert_unresolved(
        &rows,
        &[
            ("Login#run", Call, 3, "User.find_by_token"),
            ("Billing::Invoice#run", Call, 4, "Order.new.run"),
        ],
    );
}

/// A call on `self` in a local path gem runs on the objects of every
/// project that loads the gem too: an application's subclass or includer
/// that overrides the name leaves it unproven. Ruby 3.3: `Child.new.run`
/// is `:child`, `Widget.new.run` `:widget`.
#[test]
fn c10_ruby_dynamic_dispatch_a_path_gems_self_call_holds_for_the_apps_objects() {
    // R6
    let gemspec = "Gem::Specification.new do |s|\n  s.name = \"eng\"\nend\n";
    let rows = index(&[
        ("Gemfile", "gem \"eng\", path: \"engines/eng\"\n"),
        ("engines/eng/eng.gemspec", gemspec),
        (
            "engines/eng/lib/eng/base.rb",
            "module Eng
  class Base
    def run; hook; end
    def hook; end
    def stay; kept; end
    def kept; end
  end
  module Helpers
    def run; label; end
    def label; end
  end
end
",
        ),
        (
            "app/models/child.rb",
            "class Child < Eng::Base\n  def hook; end\nend\n",
        ),
        (
            "app/models/widget.rb",
            "class Widget\n  include Eng::Helpers\n  def label; end\nend\n",
        ),
    ]);
    assert_eq!(
        resolved(&rows),
        labelled(&[
            ("Eng::Base#stay", Call, 5, "Eng::Base#kept"),
            ("Child", Reference, 1, "Eng::Base"),
            ("Widget", Reference, 2, "Eng::Helpers"),
        ])
    );
    assert_unresolved(
        &rows,
        &[
            ("Eng::Base#run", Call, 3, "hook"),
            ("Eng::Helpers#run", Call, 9, "label"),
        ],
    );
}
