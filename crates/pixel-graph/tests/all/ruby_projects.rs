// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Ruby files known by name or shebang, and `require`s resolved inside the
//! project that declares them: its load roots, its local path gems, never an
//! external gem's path or a sibling project.

use std::fs;
use std::path::Path;

use pixel_graph::GraphStore;
use pixel_graph::build::{build_graph, update_file};
use pixel_graph::extract::{extract_file, lang_of, lang_of_file};

const GEMFILE: &str = "source \"https://rubygems.org\"
gem \"rails\", \"~> 7.1\"
gem \"my-client\", require: \"my_client\"
gem \"billing\", path: \"engines/billing\"
";

const LOCK: &str = "PATH
  remote: engines/billing
  specs:
    billing (0.1.0)

GEM
  remote: https://rubygems.org/
  specs:
    rails (7.1.0)
      railties (= 7.1.0)
    railties (7.1.0)
      thor (~> 1.0)
    thor (1.3.0)

DEPENDENCIES
  billing!
  rails
";

const ORDER: &str = "require \"rails/generators\"
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
";

const TREE: &[(&str, &str)] = &[
    ("Gemfile", GEMFILE),
    ("Gemfile.lock", LOCK),
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
    ("app/models/order.rb", ORDER),
    ("app/services/checkout.rb", "class Checkout\nend\n"),
    ("cwd_relative.rb", "X = 1\n"),
    // Same tails as external gems' files: never their targets.
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
    // A sibling project: its files are not on the shop's load path.
    ("other_app/Gemfile", "source \"https://rubygems.org\"\n"),
    ("other_app/lib/missing/thing.rb", "module Missing\nend\n"),
];

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

/// `(spec, resolved path)` of every import `importer` stores, in order.
fn imports(db: &Path, importer: &str) -> Vec<(String, Option<String>)> {
    let store = GraphStore::open(db).unwrap();
    store
        .conn()
        .prepare(
            "SELECT i.spec, t.path FROM imports i JOIN files f ON f.id = i.file_id
               LEFT JOIN files t ON t.id = i.resolved_file_id
              WHERE f.path = ?1 ORDER BY i.id",
        )
        .unwrap()
        .query_map([importer], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn row(spec: &str, target: Option<&str>) -> (String, Option<String>) {
    (spec.to_string(), target.map(str::to_string))
}

fn order_imports(with_billing: bool) -> Vec<(String, Option<String>)> {
    vec![
        row("rails/generators", None),
        row("thor", None),
        row("my_client/api", None),
        row("reports/pdf", Some("lib/reports/pdf.rb")),
        row(
            "billing/invoice",
            with_billing.then_some("engines/billing/lib/billing/invoice.rb"),
        ),
        row("missing/thing", None),
        row("./cwd_relative", None),
        row("../services/checkout", Some("app/services/checkout.rb")),
        row("absent", None),
    ]
}

#[test]
fn ruby_files_should_be_known_by_name_or_ruby_shebang_only() {
    for path in ["Gemfile", "app/Gemfile", "Rakefile", "Guardfile", "Capfile"] {
        assert_eq!(lang_of(path), Some("ruby"), "{path}");
    }
    for path in ["Gemfile.lock", "Makefile", "bin/rails", "Procfile"] {
        assert_eq!(lang_of(path), None, "{path}");
    }
    let ruby = b"#!/usr/bin/env ruby\nputs 1\n";
    for path in ["bin/rails", "exe/mygem", "engines/x/bin/setup"] {
        assert_eq!(lang_of_file(path, ruby), Some("ruby"), "{path}");
    }
    assert_eq!(
        lang_of_file("bin/rails", b"#!/usr/bin/ruby -w\n"),
        Some("ruby")
    );
    assert_eq!(
        lang_of_file("bin/rails", b"#!/usr/bin/env -S ruby --disable-gems\n"),
        Some("ruby")
    );
    // Not every extensionless executable: a shell binstub, a ruby shebang
    // outside `bin/`/`exe/`, a script with an extension, no shebang at all.
    for (path, content) in [
        ("bin/dev", &b"#!/usr/bin/env sh\nexec foreman\n"[..]),
        ("bin/rubyish", b"#!/usr/bin/env rubyx\n"),
        ("scripts/tool", ruby),
        ("bin/tool.sh", ruby),
        ("bin/plain", b"puts 1\n"),
        ("bin/env", b"#!/usr/bin/env\n"),
    ] {
        assert_eq!(lang_of_file(path, content), None, "{path}");
    }
    assert_eq!(
        extract_file("bin/rails", ruby).map(|fx| fx.lang),
        Some("ruby")
    );
    assert!(extract_file("bin/dev", b"#!/usr/bin/env sh\n").is_none());
}

#[test]
fn ruby_requires_should_resolve_inside_their_project_only() {
    let root = tempfile::tempdir().unwrap();
    for (rel, body) in TREE {
        write(root.path(), rel, body);
    }
    let db = root.path().join("graph.db");
    build_graph(root.path(), &db).unwrap();

    assert_eq!(imports(&db, "app/models/order.rb"), order_imports(true));
    // A path gem resolves within its own roots: the app's `lib` is not on
    // the engine's load path.
    assert_eq!(
        imports(&db, "engines/billing/lib/billing/charge.rb"),
        [
            row(
                "billing/invoice",
                Some("engines/billing/lib/billing/invoice.rb")
            ),
            row("reports/pdf", None),
        ]
    );
    assert_eq!(
        imports(&db, "bin/rails"),
        [
            row("../config/boot", Some("config/boot.rb")),
            row("rails/commands", None)
        ]
    );
    assert_eq!(
        imports(&db, "Rakefile"),
        [row("config/application", Some("config/application.rb"))]
    );
    let store = GraphStore::open(&db).unwrap();
    let langs: Vec<(String, String)> = store
        .conn()
        .prepare("SELECT path, lang FROM files WHERE path IN ('Gemfile', 'Rakefile', 'bin/rails', 'bin/dev', 'Gemfile.lock') ORDER BY path")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        langs,
        [
            ("Gemfile".to_string(), "ruby".to_string()),
            ("Rakefile".to_string(), "ruby".to_string()),
            ("bin/rails".to_string(), "ruby".to_string()),
        ],
        "a shell binstub and the lockfile are not Ruby sources"
    );
}

#[test]
fn ruby_requires_should_follow_manifest_edits_incrementally() {
    let root = tempfile::tempdir().unwrap();
    for (rel, body) in TREE {
        write(root.path(), rel, body);
    }
    let db = root.path().join("graph.db");
    build_graph(root.path(), &db).unwrap();

    // Dropping the path gem from the Gemfile takes its roots off the load
    // path of every file of the project, not only of the file edited.
    write(
        root.path(),
        "Gemfile",
        &GEMFILE.replace("gem \"billing\", path: \"engines/billing\"\n", ""),
    );
    update_file(root.path(), &db, "Gemfile").unwrap();
    assert_eq!(imports(&db, "app/models/order.rb"), order_imports(false));
    write(root.path(), "Gemfile", GEMFILE);
    update_file(root.path(), &db, "Gemfile").unwrap();
    assert_eq!(imports(&db, "app/models/order.rb"), order_imports(true));

    // A second file on another load root makes the require ambiguous.
    write(
        root.path(),
        "engines/billing/lib/reports/pdf.rb",
        "module Reports\nend\n",
    );
    update_file(root.path(), &db, "engines/billing/lib/reports/pdf.rb").unwrap();
    let mut want = order_imports(true);
    want[3] = row("reports/pdf", None);
    assert_eq!(imports(&db, "app/models/order.rb"), want);
    fs::remove_file(root.path().join("engines/billing/lib/reports/pdf.rb")).unwrap();
    update_file(root.path(), &db, "engines/billing/lib/reports/pdf.rb").unwrap();
    assert_eq!(imports(&db, "app/models/order.rb"), order_imports(true));

    // The incremental graph equals a full build of the same tree.
    let fresh = tempfile::tempdir().unwrap();
    for (rel, body) in TREE {
        write(fresh.path(), rel, body);
    }
    let fresh_db = fresh.path().join("graph.db");
    build_graph(fresh.path(), &fresh_db).unwrap();
    for importer in [
        "app/models/order.rb",
        "engines/billing/lib/billing/charge.rb",
        "bin/rails",
        "Rakefile",
    ] {
        assert_eq!(
            imports(&db, importer),
            imports(&fresh_db, importer),
            "{importer}"
        );
    }
}
