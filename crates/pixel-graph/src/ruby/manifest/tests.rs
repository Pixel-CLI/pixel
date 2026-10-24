// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Which files are Ruby, the generated-blob guard, and the facts of the
//! manifests, on v1's fixtures (`crates/pixel-graph/src/extract.rs`,
//! `extract/contract_tests.rs`, `imports/ruby.rs`, `tests/all/ruby_projects.rs`).

use super::*;
use crate::guard::{GENERATED_MAX_BYTES_PER_LINE, GENERATED_MIN_BYTES};
use crate::ruby::extract::testing::{imports, rows, symbols};

/// By whole name or by the extension of the file name, never of a
/// directory, case-sensitive. v1
/// `lang_of_should_map_every_wired_extension_to_its_walker_language` and
/// its companions, `ruby_files_should_be_known_by_name_or_ruby_shebang_only`.
#[test]
fn a_ruby_source_is_known_by_its_file_name_or_extension() {
    for path in [
        "a.rb",
        "Rakefile.rake",
        "x.gemspec",
        "config.ru",
        "Gemfile",
        "app/Gemfile",
        "Rakefile",
        "Guardfile",
        "Capfile",
        "pkg.rs/main.rb",
    ] {
        assert!(is_ruby_by_name(path) && wants(path), "{path}");
    }
    for path in [
        "src/a.rs",
        "Makefile",
        "README.md",
        "a.toml",
        "dir.d/noext",
        "a.json",
        "pkg.rb/README",
        "Procfile",
        "A.RB",
    ] {
        assert!(!wants(path), "{path}");
    }
}

/// An extensionless file directly in `bin/` or `exe/` is Ruby only when
/// its first line is a Ruby shebang: `bin/dev` is often a shell script.
/// v1 `ruby_files_should_be_known_by_name_or_ruby_shebang_only`.
#[test]
fn a_binstub_is_ruby_only_with_a_ruby_shebang() {
    let ruby = b"#!/usr/bin/env ruby\nputs 1\n";
    for path in ["bin/rails", "exe/mygem", "engines/x/bin/setup"] {
        assert!(wants(path) && accepts(path, ruby), "{path}");
    }
    assert!(accepts("bin/rails", b"#!/usr/bin/ruby -w\n"));
    assert!(accepts(
        "bin/rails",
        b"#!/usr/bin/env -S ruby --disable-gems\n"
    ));
    for (path, content) in [
        ("bin/dev", &b"#!/usr/bin/env sh\nexec foreman\n"[..]),
        ("bin/rubyish", b"#!/usr/bin/env rubyx\n"),
        ("bin/plain", b"puts 1\n"),
        ("bin/env", b"#!/usr/bin/env\n"),
    ] {
        assert!(wants(path) && !accepts(path, content), "{path}");
    }
    for path in ["scripts/tool", "bin/tool.sh", "rails", "bin/sub/x"] {
        assert!(!wants(path), "{path}");
    }
}

/// A Ruby binstub is a whole Ruby file: its `File` symbol owns its code,
/// and its requires are imports.
#[test]
fn a_ruby_binstub_is_read_as_a_ruby_file() {
    let rows = rows(
        "bin/rails",
        "#!/usr/bin/env ruby\nrequire_relative \"../config/boot\"\nrequire \"rails/commands\"\n",
    );
    assert_eq!(symbols(&rows), ["file bin/rails 1-4"]);
    assert_eq!(
        imports(&rows),
        [
            "2 bin/rails: require_relative ../config/boot -> \"../config/boot\" []",
            "3 bin/rails: require rails/commands -> \"rails/commands\" []",
        ]
    );
}

/// A `Gemfile.lock` is wanted and read as text, never parsed: it yields the
/// gems its `GEM` and `GIT` sections list as specs, transitive ones
/// included, and no `PATH` spec. v1
/// `lockfiles_should_list_remote_specs_with_transitive_ones_but_not_path_specs`.
#[test]
fn a_lockfile_lists_remote_specs_with_transitive_ones_but_not_path_specs() {
    let lock = [
        "PATH",
        "  remote: engines/billing",
        "  specs:",
        "    billing (0.1.0)",
        "      rails (>= 7)",
        "",
        "GEM",
        "  remote: https://rubygems.org/",
        "  specs:",
        "    rails (7.1.0)",
        "      railties (= 7.1.0)",
        "    railties (7.1.0)",
        "      thor (~> 1.0)",
        "    thor (1.3.0)",
        "",
        "PLATFORMS",
        "  ruby",
        "",
        "DEPENDENCIES",
        "  billing!",
        "  rails",
    ]
    .join("\n");
    assert!(wants("Gemfile.lock") && wants("engines/x/Gemfile.lock"));
    assert!(accepts("Gemfile.lock", lock.as_bytes()));
    let rows = rows("Gemfile.lock", &lock);
    assert_eq!(symbols(&rows), ["file Gemfile.lock 1-21"]);
    assert!(rows.edges.is_empty());
    let gem = |name: &str| Fact::LockGem {
        name: name.to_owned(),
    };
    assert_eq!(
        Fact::decode_all(&rows.facts),
        [gem("rails"), gem("railties"), gem("thor")]
    );
}

/// Literal `gem`, `gemspec path:` and `path "dir" do` declarations, in any
/// block: a remote gem with its literal `require:`, a local gem by its
/// path; a computed `path:` names neither. v1
/// `gemfile_literals_should_name_remote_gems_path_gems_and_require_names`.
#[test]
fn a_gemfile_names_remote_gems_path_gems_and_require_names() {
    let rows = rows(
        "Gemfile",
        r#"
source "https://rubygems.org"
gemspec path: "gems/core"
gem "rails", "~> 7.1"
gem "my-client", require: "my_client/api"
gem "quiet", require: false
gem "billing", path: "engines/billing"
gem "forked", github: "acme/forked"
gem "computed", path: File.join("x", "y")
group :test do
  gem "rspec-rails"
end
path "components" do
  gem "search"
end
"#,
    );
    let remote = |name: &str, require: Option<&str>| Fact::Gem {
        name: name.to_owned(),
        require: require.map(str::to_owned),
    };
    let local = |path: &str| Fact::PathGem {
        path: path.to_owned(),
    };
    assert_eq!(
        Fact::decode_all(&rows.facts),
        [
            local("gems/core"),
            remote("rails", None),
            remote("my-client", Some("my_client/api")),
            remote("quiet", None),
            local("engines/billing"),
            remote("forked", None),
            remote("rspec-rails", None),
            local("components/search"),
        ]
    );
}

/// The first literal `name` and `require_paths` assignments, whatever the
/// receiver is called; `version` and the like are not read. v1
/// `gemspec_literals_should_give_the_name_and_require_paths`.
#[test]
fn a_gemspec_gives_its_literal_name_and_require_paths() {
    let rows = rows(
        "engines/billing/billing.gemspec",
        r#"Gem::Specification.new do |spec|
  spec.name = "billing"
  spec.require_paths = ["lib", "ext/lib"]
  spec.version = Billing::VERSION
end
"#,
    );
    assert_eq!(
        Fact::decode_all(&rows.facts),
        [
            Fact::GemspecName {
                name: "billing".to_owned()
            },
            Fact::RequirePaths {
                paths: vec!["lib".to_owned(), "ext/lib".to_owned()]
            },
        ]
    );
}

/// A computed name or load path is not evaluated: no fact. v1
/// `gemspec_literals_should_give_the_name_and_require_paths`.
#[test]
fn a_computed_gemspec_gives_no_fact() {
    let rows = rows(
        "x.gemspec",
        "Gem::Specification.new do |s|\n  s.name = NAME\n  s.require_paths = Dir['lib']\nend\n",
    );
    assert!(Fact::decode_all(&rows.facts).is_empty());
}

/// A manifest is known by its file name: a Gemfile, a lockfile, a gemspec
/// with a name. v1 `manifests_should_be_recognised_by_file_name`.
#[test]
fn manifests_are_known_by_file_name() {
    assert!(is_gemfile("Gemfile") && is_gemfile("app/Gemfile"));
    assert!(is_lockfile("Gemfile.lock") && is_lockfile("x/Gemfile.lock"));
    assert!(is_gemspec("a.gemspec"));
    for path in ["Gemfile.rb", "MyGemfile", "lib/gemspec.rb", ".gemspec"] {
        assert!(
            !is_gemfile(path) && !is_lockfile(path) && !is_gemspec(path),
            "{path}"
        );
    }
}

/// One line of `width` bytes and its newline.
fn line_of(width: usize) -> String {
    let mut line = "x".repeat(width);
    line.push('\n');
    line
}

/// Both conditions decide and both comparisons are inclusive; an
/// unterminated last line counts as a line. v1
/// `guard_boundary_is_exact_on_both_conditions`.
#[test]
fn the_generated_blob_guard_is_exact_on_both_conditions() {
    let mut at_threshold = String::new();
    while at_threshold.len() < GENERATED_MIN_BYTES {
        at_threshold.push_str(&line_of(GENERATED_MAX_BYTES_PER_LINE - 1));
    }
    assert!(is_generated_blob(at_threshold.as_bytes()));
    let mut narrower = String::new();
    while narrower.len() < GENERATED_MIN_BYTES * 2 {
        narrower.push_str(&line_of(GENERATED_MAX_BYTES_PER_LINE - 2));
    }
    assert!(!is_generated_blob(narrower.as_bytes()));
    assert!(!is_generated_blob(
        &at_threshold.as_bytes()[..GENERATED_MIN_BYTES - 1]
    ));
    assert!(is_generated_blob(
        "y".repeat(GENERATED_MIN_BYTES).as_bytes()
    ));
    assert!(!is_generated_blob(b""));
    let body_lines = 128;
    let mut no_final_newline = line_of(GENERATED_MAX_BYTES_PER_LINE - 2).repeat(body_lines);
    let target = (GENERATED_MAX_BYTES_PER_LINE - 1) * (body_lines + 1);
    no_final_newline.push_str(&"z".repeat(target - no_final_newline.len()));
    assert!(no_final_newline.len() / body_lines >= GENERATED_MAX_BYTES_PER_LINE);
    assert!(!is_generated_blob(no_final_newline.as_bytes()));
}

/// A minified one-line bundle is refused before any parse: pointing
/// tree-sitter at it cost v1 seconds and thousands of junk symbols. v1
/// `minified_bundle_contributes_nothing_to_the_graph`.
#[test]
fn a_minified_bundle_is_refused() {
    let mut bundle = String::new();
    let mut i = 0;
    while bundle.len() < 1_000_000 {
        bundle.push_str(&format!("def f{i}(a, b) = a ? [b, {i}] : b; "));
        i += 1;
    }
    assert!(!accepts("public/assets/app.rb", bundle.as_bytes()));
    assert!(!accepts(
        "bin/bundle",
        format!("#!/usr/bin/env ruby\n{bundle}").as_bytes()
    ));
}

/// Hand-written source with one very long line, and a large ordinary file,
/// are no blobs: only the mean per line separates them from a bundle. v1
/// `dense_real_source_still_extracts`.
#[test]
fn dense_hand_written_source_is_no_blob() {
    let mut ruby = String::from("class ParkingIdExtractor\n  def call\n    plates = \"");
    ruby.push_str(&"AB-123-CD ".repeat(1_700));
    ruby.push_str("\"\n    plates.split\n  end\nend\n");
    assert!(ruby.lines().map(str::len).max().unwrap_or(0) > 16_000);
    assert!(accepts(
        "app/services/parking_id_extractor.rb",
        ruby.as_bytes()
    ));
    let rows = rows("app/services/parking_id_extractor.rb", &ruby);
    assert!(
        rows.symbols
            .iter()
            .any(|s| s.name == "ParkingIdExtractor#call")
    );
    let mut schema = String::from("# frozen_string_literal: true\n");
    while schema.len() < GENERATED_MIN_BYTES * 6 {
        schema.push_str("  create_table \"parkings\", force: :cascade do |t|\n  end\n");
    }
    assert!(!is_generated_blob(schema.as_bytes()));
}

#[test]
fn audited_fixture_sets_should_preserve_nested_project_paths_and_refuse_empty_or_directory_names() {
    for (path, expected) in [
        ("test/fixtures/users.yml", Some("users")),
        (
            "engines/billing/test/fixtures/admin/users.yml",
            Some("admin/users"),
        ),
        ("test/fixtures/.yml", None),
        ("test/fixtures/admin/.yml", None),
        ("test/fixtures/users.yaml", None),
        ("fixtures/users.yml", None),
    ] {
        assert_eq!(fixture_set(path), expected, "{path}");
    }
}
