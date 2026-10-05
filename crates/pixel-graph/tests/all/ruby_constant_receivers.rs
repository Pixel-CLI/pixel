// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Constant receivers must retain their owner through full and incremental builds.

use std::fs;
use std::path::Path;

use pixel_graph::GraphStore;
use pixel_graph::build::{build_graph, update_file};
use pixel_graph::resolve::{Decision, ResolveIndex};

const TARGETS: &str = "module A\n  class B\n    def self.run; end\n    def initialize(value = nil); end\n    def run; end\n  end\nend\nclass B\n  def self.run; end\n  def initialize; end\n  def run; end\nend\n";
const CALLER: &str = "module A\n  class C\n    def invoke\n      B.run\n      ::B.run\n      A::B.run\n      B.new(1)\n      B.new(1).run\n    end\n  end\nend\n";

fn fixture(caller: &str, targets: &str) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("caller.rb"), caller).unwrap();
    fs::write(root.path().join("targets.rb"), targets).unwrap();
    fs::create_dir(root.path().join(".pixel")).unwrap();
    build_graph(root.path(), &root.path().join(".pixel/graph.db")).unwrap();
    root
}

fn calls(root: &Path) -> Vec<(u32, String, String, String)> {
    let store = GraphStore::open(&root.join(".pixel/graph.db")).unwrap();
    let mut query = store
        .conn()
        .prepare(
            "SELECT e.site_line, e.callee, dst.qualified, e.tier FROM edges e
         JOIN symbols src ON src.id=e.src_id JOIN files f ON f.id=src.file_id
         JOIN symbols dst ON dst.id=e.dst_id
         WHERE f.path='caller.rb' AND e.kind='calls'
         ORDER BY e.site_line, e.callee, dst.qualified",
        )
        .unwrap();
    query
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn expected(rows: &[(u32, &str, &str, &str)]) -> Vec<(u32, String, String, String)> {
    rows.iter()
        .map(|&(line, name, target, tier)| (line, name.into(), target.into(), tier.into()))
        .collect()
}

#[test]
fn ruby_constants_should_resolve_class_constructor_and_instance_calls_through_every_update() {
    let root = fixture(CALLER, TARGETS);
    let want = expected(&[
        (4, "run", "A::B.run", "exact"),
        (5, "run", "B.run", "exact"),
        (6, "run", "A::B.run", "exact"),
        (7, "new", "A::B#initialize", "exact"),
        (8, "new", "A::B#initialize", "exact"),
        (8, "run", "A::B#run", "exact"),
    ]);
    assert_eq!(calls(root.path()), want);
    for (file, text) in [
        ("caller.rb", CALLER),
        ("targets.rb", TARGETS),
        (
            "rival.rb",
            "module Unreachable\n class B\n def self.run; end\n def run; end\n end\nend\n",
        ),
    ] {
        fs::write(root.path().join(file), format!("{text}\n")).unwrap();
        update_file(root.path(), &root.path().join(".pixel/graph.db"), file).unwrap();
        assert_eq!(calls(root.path()), want, "after updating {file}");
    }
    // Diagnostics have a line but no caller id: they must make the same decision.
    let store = GraphStore::open(&root.path().join(".pixel/graph.db")).unwrap();
    let caller = store.file_by_path("caller.rb").unwrap().unwrap();
    let target = store
        .symbols_by_name("run", None, 50)
        .unwrap()
        .into_iter()
        .find(|s| s.qualified == "A::B.run")
        .unwrap();
    let index = ResolveIndex::build(&store).unwrap();
    assert_eq!(
        index.decide_at(caller.id, "run", Some("B"), 4),
        Decision::Exact(target.id)
    );
    assert_eq!(
        index.decide(caller.id, "run", Some("B")),
        Decision::Unresolved,
        "without a site the lexical scope cannot be inferred"
    );
}

#[test]
fn ruby_constants_should_use_lexical_nesting_not_qualified_name_prefixes() {
    let caller = "module A\n class C\n def invoke\n B.run\n end\n end\nend\nclass A::D\n def invoke\n B.run\n end\nend\n";
    let root = fixture(caller, TARGETS);
    assert_eq!(
        calls(root.path()),
        expected(&[
            (4, "run", "A::B.run", "exact"),
            (10, "run", "B.run", "exact"),
        ])
    );
}

#[test]
fn ruby_constants_should_invalidate_edges_when_a_new_constant_shadows_the_receiver() {
    let root = fixture(CALLER, TARGETS);
    assert_eq!(calls(root.path()).len(), 6);
    // No new `run` or `initialize` definition: name-only invalidation misses this.
    fs::write(root.path().join("shadow.rb"), "class A::C::B\nend\n").unwrap();
    update_file(
        root.path(),
        &root.path().join(".pixel/graph.db"),
        "shadow.rb",
    )
    .unwrap();
    assert_eq!(
        calls(root.path()),
        expected(&[
            (5, "run", "B.run", "exact"),
            (6, "run", "A::B.run", "exact"),
        ])
    );
    fs::write(root.path().join("shadow.rb"), "# removed shadow\n").unwrap();
    update_file(
        root.path(),
        &root.path().join(".pixel/graph.db"),
        "shadow.rb",
    )
    .unwrap();
    assert_eq!(
        calls(root.path()).len(),
        6,
        "stored unresolved calls must recover"
    );
}

#[test]
fn ruby_constants_should_not_fall_back_to_an_unrelated_method_or_reopened_definition() {
    let caller = "def invoke\n Lost.unique\n Missing.unique\n Known.unique\nend\n";
    let root = fixture(
        caller,
        "module X\n class Lost; end\nend\nmodule Y\n class Lost; end\nend\nclass Known\n def unique; end\nend\n",
    );
    assert_eq!(
        calls(root.path()),
        vec![],
        "an instance method is not a class method"
    );
    let root = fixture(CALLER, TARGETS);
    fs::write(
        root.path().join("reopened.rb"),
        "class A::B\n def self.run; end\nend\n",
    )
    .unwrap();
    update_file(
        root.path(),
        &root.path().join(".pixel/graph.db"),
        "reopened.rb",
    )
    .unwrap();
    assert_eq!(
        calls(root.path()),
        expected(&[
            (5, "run", "B.run", "exact"),
            (7, "new", "A::B#initialize", "exact"),
            (8, "new", "A::B#initialize", "exact"),
            (8, "run", "A::B#run", "exact"),
        ])
    );
}

#[test]
fn ruby_rails_dispatch_should_be_probable_and_explicit_class_methods_should_win() {
    let caller = "def invoke\n Job.perform_later(1)\n Job.perform_now(1)\n Job.perform_async(1)\n Job.perform_in(1, 2)\n Job.perform_at(1, 2)\n Job.set(wait: 1).perform_later(1)\n NoticeMailer.welcome(1).deliver_later\n Job.set(wait: 1).perform_now(1)\n Job.set(wait: 1).perform_async(1)\n Job.set(wait: 1).run\nend\n";
    let targets = "class Job\n def perform; end\nend\nclass NoticeMailer\n def welcome; end\nend\n";
    let root = fixture(caller, targets);
    let want = expected(&[
        (2, "perform_later", "Job#perform", "probable"),
        (3, "perform_now", "Job#perform", "probable"),
        (4, "perform_async", "Job#perform", "probable"),
        (5, "perform_in", "Job#perform", "probable"),
        (6, "perform_at", "Job#perform", "probable"),
        (7, "perform_later", "Job#perform", "probable"),
        (8, "welcome", "NoticeMailer#welcome", "probable"),
        (9, "perform_now", "Job#perform", "probable"),
    ]);
    assert_eq!(calls(root.path()), want);
    fs::write(root.path().join("targets.rb"), format!("{targets}\n")).unwrap();
    update_file(
        root.path(),
        &root.path().join(".pixel/graph.db"),
        "targets.rb",
    )
    .unwrap();
    assert_eq!(
        calls(root.path()),
        want,
        "replay must retain perform_later, not rename it to perform"
    );
    fs::write(root.path().join("overrides.rb"), "class Job\n def self.perform_later; end\nend\nclass NoticeMailer\n def self.welcome; end\nend\n").unwrap();
    update_file(
        root.path(),
        &root.path().join(".pixel/graph.db"),
        "overrides.rb",
    )
    .unwrap();
    assert_eq!(
        calls(root.path()),
        expected(&[
            (2, "perform_later", "Job.perform_later", "exact"),
            (3, "perform_now", "Job#perform", "probable"),
            (4, "perform_async", "Job#perform", "probable"),
            (5, "perform_in", "Job#perform", "probable"),
            (6, "perform_at", "Job#perform", "probable"),
            (8, "welcome", "NoticeMailer.welcome", "exact"),
            (9, "perform_now", "Job#perform", "probable"),
        ])
    );
}

#[test]
fn ruby_constructor_should_respect_an_explicit_new_override() {
    let root = fixture(CALLER, TARGETS);
    fs::write(
        root.path().join("factory.rb"),
        "class A::B\n def self.new(value); Object.new; end\nend\n",
    )
    .unwrap();
    update_file(
        root.path(),
        &root.path().join(".pixel/graph.db"),
        "factory.rb",
    )
    .unwrap();
    assert_eq!(
        calls(root.path()),
        expected(&[
            (4, "run", "A::B.run", "exact"),
            (5, "run", "B.run", "exact"),
            (6, "run", "A::B.run", "exact"),
            (7, "new", "A::B.new", "exact"),
            (8, "new", "A::B.new", "exact"),
        ]),
        "custom factories do not prove the type of their return value"
    );
}

#[test]
fn ruby_constants_should_not_guess_lexical_nesting_when_line_spans_are_indistinguishable() {
    let root = fixture(
        "module A; class C; def invoke; B.run; end; end; end\n",
        TARGETS,
    );
    assert_eq!(
        calls(root.path()),
        vec![],
        "line-only scopes cannot distinguish nested declarations from adjacent reopenings"
    );
    let root = fixture(
        "class C\n def invoke\n B.run; end; end; class Z\nend\n",
        TARGETS,
    );
    assert_eq!(
        calls(root.path()),
        vec![],
        "adjacent classes share the call's line without being nested"
    );
}

#[test]
fn ruby_dispatch_should_reject_non_classes_and_overridden_configurators() {
    let root = fixture(
        "def invoke\n Helpers.new.run\n Helpers.new\n Helpers.perform_later\n HelpersMailer.welcome\n Job.set(wait: 1).perform_now\n Job.set(wait: 1).perform_async\n Job.set(wait: 1).run\n Helpers.set(wait: 1).perform_now\nend\n",
        "module Helpers\n def initialize; end\n def run; end\n def perform; end\nend\nmodule HelpersMailer\n def welcome; end\nend\nclass Job\n def self.set(options); Object.new; end\n def perform; end\nend\n",
    );
    assert_eq!(
        calls(root.path()),
        expected(&[
            (6, "set", "Job.set", "exact"),
            (7, "set", "Job.set", "exact"),
            (8, "set", "Job.set", "exact")
        ]),
        "module instance methods and custom configurator returns cannot prove a Rails target"
    );
}

#[test]
fn ruby_constants_should_resolve_a_late_required_file_and_preserve_rename_spelling() {
    let root = fixture(
        "require_relative 'later'\ndef invoke\n Widget.new(1).run\nend\n",
        "",
    );
    assert_eq!(calls(root.path()), vec![]);
    fs::write(
        root.path().join("later.rb"),
        "class Widget\n def initialize(value); end\n def run; end\nend\n",
    )
    .unwrap();
    update_file(
        root.path(),
        &root.path().join(".pixel/graph.db"),
        "later.rb",
    )
    .unwrap();
    assert_eq!(
        calls(root.path()),
        expected(&[
            (3, "new", "Widget#initialize", "exact"),
            (3, "run", "Widget#run", "exact"),
        ])
    );
    let store = GraphStore::open(&root.path().join(".pixel/graph.db")).unwrap();
    for (name, renamed, caller_lines) in
        [("initialize", "setup", vec![]), ("run", "execute", vec![3])]
    {
        let symbol = store.symbols_by_name(name, None, 10).unwrap().remove(0);
        let plan = pixel_graph::rename::plan(&store, root.path(), &symbol, renamed).unwrap();
        let lines: Vec<_> = plan
            .files
            .get("caller.rb")
            .into_iter()
            .flatten()
            .map(|edit| edit.line)
            .collect();
        assert_eq!(
            lines, caller_lines,
            "renaming {name} must not rewrite the constructor word new"
        );
    }
}
