// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! v1's resolution cases (port spec 15.3 to 15.6) as workspaces, with the
//! MVP's expectations. v1 decided by tiers: the same file (T0), an import
//! (T1), the only file defining a name (T2, `Probable`), a receiver's type
//! segment (R-8), a module path matched by suffix (R-9) and the sole
//! inherent method (R-10). Decision 1 keeps only what Rust's name
//! resolution proves, so every `Probable` of v1 is either proven here or
//! unresolved; each changed expectation says which.
//!
//! Not ported: tests of v1's helpers for rules the MVP drops (kind
//! priority, receiver shapes, suffix matching, line-range scopes), the
//! top-level site without an owner (every Rust site has one, the `File`
//! symbol at worst), and the incremental half of the build tests, which
//! `link` owns: resolution reads rows only, so
//! [`c10_rust_resolution_depends_on_the_rows_not_their_order`] stands for
//! it here.

use super::*;

const APP: &str = "[package]\nname = \"app\"\n";

fn manifest(name: &str) -> String {
    format!("[package]\nname = \"{name}\"\n")
}

/// A package depending on the sibling packages `deps` (name, directory)
/// by path, as Rust requires to name them.
fn depending(name: &str, deps: &[(&str, &str)]) -> String {
    let mut out = format!("{}\n[dependencies]\n", manifest(name));
    for (dep, dir) in deps {
        out.push_str(&format!("{dep} = {{ path = \"../{dir}\" }}\n"));
    }
    out
}

#[test]
fn c10_rust_resolution_depends_on_the_rows_not_their_order() {
    let mut files = vec![
        ("Cargo.toml", APP),
        ("src/lib.rs", "pub mod push;\npub mod ship;\n"),
        ("src/push.rs", "pub fn push() {}\n"),
        (
            "src/ship.rs",
            "use crate::push::push as leased;\npub fn ship() { leased(); }\n",
        ),
    ];
    let forward = calls(&files);
    files.reverse();
    assert_eq!(calls(&files), forward);
    assert_eq!(
        forward,
        [edge("ship", "leased", Some("src/push.rs#push#function"))]
    );
}

/// v1 `resolve_calls_counts_every_decision`.
#[test]
fn c10_rust_same_module_calls_resolve_and_other_files_names_do_not() {
    let files = [
        ("Cargo.toml", APP),
        ("src/lib.rs", "pub mod local;\npub mod remote;\n"),
        (
            "src/local.rs",
            "pub fn f() {\n    f();\n    f();\n    g();\n    nosuch();\n}\n",
        ),
        ("src/remote.rs", "pub fn f() {}\npub fn g() {}\n"),
    ];
    // v1: `g` Probable, the only file defining it (T2); nothing brings it
    // into `local`'s scope (decision 1).
    assert_eq!(
        calls(&files),
        [
            edge("f", "f", Some("src/local.rs#f#function")),
            edge("f", "f", Some("src/local.rs#f#function")),
            edge("f", "g", None),
            edge("f", "nosuch", None),
        ]
    );
}

/// v1 `qualified_call_with_a_locally_defined_name_is_unresolved` and
/// `unqualified_and_self_receiver_calls_keep_the_t0_definition`.
#[test]
fn c10_rust_a_path_through_an_unknown_crate_is_not_the_local_namesake() {
    let files = [
        ("Cargo.toml", APP),
        ("src/lib.rs", "pub mod local;\n"),
        (
            "src/local.rs",
            "pub fn f() {}\npub fn caller() {\n    other_crate::f();\n    f();\n    self::f();\n}\n",
        ),
    ];
    assert_eq!(
        calls(&files),
        [
            edge("caller", "other_crate::f", None),
            edge("caller", "f", Some("src/local.rs#f#function")),
            edge("caller", "self::f", Some("src/local.rs#f#function")),
        ]
    );
}

/// v1 `real_receiver_with_a_name_defined_elsewhere_stays_probable`,
/// `sole_inherent_method_resolves_a_value_receiver_at_probable_never_exact`,
/// `two_same_named_methods_in_one_file_keep_the_shadow_veto`,
/// `a_trait_impl_among_several_local_candidates_keeps_the_shadow_veto` and
/// `a_second_file_definition_keeps_the_shadow_veto`.
#[test]
fn c10_rust_a_method_name_alone_proves_nothing() {
    let files = [
        ("Cargo.toml", APP),
        ("src/lib.rs", "pub mod local;\npub mod remote;\n"),
        (
            "src/local.rs",
            "pub struct A;\npub struct B;\nimpl A {\n    pub fn walk(&self) {}\n    pub fn push_call(&mut self) {}\n}\nimpl B {\n    pub fn walk(&self) {}\n}\n\
             pub fn caller(x: Unknown, w: Unknown, a: &A) {\n    x.g();\n    w.push_call();\n    w.walk();\n    a.walk();\n}\n",
        ),
        (
            "src/remote.rs",
            "pub fn g() {}\npub struct C;\nimpl C {\n    pub fn walk(&self) {}\n}\n",
        ),
    ];
    // v1: `x.g()` and `w.push_call()` Probable (T2, R-10). A value of
    // unknown type proves no method (decision 1); a stated type does.
    assert_eq!(
        calls(&files),
        [
            edge("caller", "x.g", None),
            edge("caller", "w.push_call", None),
            edge("caller", "w.walk", None),
            edge("caller", "a.walk", Some("src/local.rs#A::walk#method")),
        ]
    );
}

/// v1 `a_free_function_with_a_receiver_keeps_the_shadow_veto`,
/// `a_trait_impl_method_keeps_the_shadow_veto` and
/// `a_chained_receiver_keeps_the_shadow_veto`.
#[test]
fn c10_rust_a_std_receiver_never_reaches_a_workspace_namesake() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "use std::path::Path;\npub fn exists() {}\npub struct Boxed;\nimpl Clone for Boxed {\n    fn clone(&self) -> Self { Boxed }\n}\npub struct Words;\nimpl Words {\n    pub fn count(&self) {}\n}\n\
             pub fn f(path: &Path, words: Vec<String>) {\n    path.exists();\n    path.clone();\n    words.iter().filter(|w| w.is_empty()).count();\n}\n",
        ),
    ];
    let method_calls: Vec<_> = calls(&files)
        .into_iter()
        .filter(|(src, _, _)| src == "f")
        .collect();
    assert_eq!(
        method_calls,
        [
            edge("f", "path.exists", None),
            edge("f", "path.clone", None),
            edge("f", "words.iter().filter(|w|w.is_empty()).count", None),
            edge("f", "words.iter().filter", None),
            edge("f", "words.iter", None),
            edge("f", "w.is_empty", None),
        ]
    );
}

/// v1 `receiver_path_type_matches_the_unique_qualified_candidate`,
/// `two_same_named_types_make_the_path_match_ambiguous`,
/// `path_prefix_must_be_the_whole_type_segment` and
/// `a_chained_receiver_never_path_matches`.
#[test]
fn c10_rust_a_type_path_resolves_through_the_crate_that_defines_it() {
    let remote = manifest("pixel-remote");
    let local = depending("app", &[("pixel-remote", "remote")]);
    let files = [
        ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
        ("crates/remote/Cargo.toml", remote.as_str()),
        (
            "crates/remote/src/lib.rs",
            "pub struct Store;\nimpl Store {\n    pub fn open() {}\n}\n",
        ),
        ("crates/local/Cargo.toml", local.as_str()),
        (
            "crates/local/src/lib.rs",
            "pub struct Other;\nimpl Other {\n    pub fn open() {}\n}\npub struct WalkerHelper;\nimpl WalkerHelper {\n    pub fn walk() {}\n}\n\
             pub fn f(o: Unknown) {\n    pixel_remote::Store::open();\n    Store::open();\n    o.open();\n    x::Store::open();\n    Walker::walk();\n    get_store().open();\n}\n",
        ),
    ];
    // v1: `Store::open()` Probable by its type segment (R-8); here `Store`
    // is in no scope of `local`. The crate path proves `Store` (decision 1).
    assert_eq!(
        calls(&files),
        [
            edge(
                "f",
                "pixel_remote::Store::open",
                Some("crates/remote/src/lib.rs#Store::open#method")
            ),
            edge("f", "Store::open", None),
            edge("f", "o.open", None),
            edge("f", "x::Store::open", None),
            edge("f", "Walker::walk", None),
            edge("f", "get_store().open", None),
            edge("f", "get_store", None),
        ]
    );
}

/// v1 `a_type_qualified_trait_method_links_to_the_implementor`.
#[test]
fn c10_rust_a_type_path_reaches_its_trait_impl() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub struct Options;\nimpl Default for Options {\n    fn default() -> Self { Options }\n}\npub fn f(opts: Unknown) {\n    Options::default();\n    opts.default();\n}\n",
        ),
    ];
    // v1: `Options::default()` Probable (R-8); the one trait impl of a
    // proven type providing `default` is proven.
    assert_eq!(
        calls(&files),
        [
            edge(
                "f",
                "Options::default",
                Some("src/lib.rs#<Options as Default>::default#method")
            ),
            edge("f", "opts.default", None),
        ]
    );
}

/// v1 `an_import_resolved_exact_target_is_not_second_guessed_by_a_path`.
#[test]
fn c10_rust_a_method_cannot_be_imported() {
    let files = [
        ("Cargo.toml", APP),
        ("src/lib.rs", "pub mod a;\npub mod b;\npub mod local;\n"),
        (
            "src/a.rs",
            "pub struct A;\nimpl A {\n    pub fn open() {}\n}\n",
        ),
        (
            "src/b.rs",
            "pub struct B;\nimpl B {\n    pub fn open() {}\n}\n",
        ),
        (
            "src/local.rs",
            "use crate::a::open;\npub fn f() { open(); crate::b::B::open(); }\n",
        ),
    ];
    // v1 bound `open` to `a.rs` and linked it Exact to the method `A::open`
    // (T1); a module has no item `open`, so neither the import nor the call
    // is proven.
    assert_eq!(
        imports(&files),
        [edge("src/local.rs", "crate::a::open", None)]
    );
    assert_eq!(
        calls(&files),
        [
            edge("f", "open", None),
            edge("f", "crate::b::B::open", Some("src/b.rs#B::open#method")),
        ]
    );
}

/// v1 `import_tier_matches_the_local_name_against_the_source_definition`
/// and `import_tier_leaves_two_items_bound_for_the_same_lines_unresolved`.
#[test]
fn c10_rust_an_alias_binds_its_source_and_two_bindings_prove_neither() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub mod a;\npub mod b;\npub mod c;\npub mod local;\n",
        ),
        ("src/a.rs", "pub fn push() {}\n"),
        ("src/b.rs", "pub fn push() {}\n"),
        ("src/c.rs", "pub fn open() {}\n"),
        (
            "src/local.rs",
            "use crate::a::push as leased;\nuse crate::a::push as both;\nuse crate::b::push as both;\nuse crate::a::open;\n\
             pub fn f() {\n    leased();\n    push();\n    both();\n    open();\n}\n",
        ),
    ];
    // v1: `open()` Probable to `c.rs`, the only file defining it (T2); the
    // `use` names `a::open`, which does not exist, and nothing else binds it.
    assert_eq!(
        calls(&files),
        [
            edge("f", "leased", Some("src/a.rs#push#function")),
            edge("f", "push", None),
            edge("f", "both", None),
            edge("f", "open", None),
        ]
    );
}

/// v1 `rust_module_path_reads_the_crate_and_the_files_under_src` and
/// `anchored_module_paths_start_from_the_caller`.
#[test]
fn c10_rust_crate_self_and_super_are_anchored_at_the_caller() {
    let a = manifest("a");
    let files = [
        ("Cargo.toml", a.as_str()),
        ("src/lib.rs", "pub mod store;\npub fn x() {}\n"),
        ("src/store/mod.rs", "pub mod rows;\npub fn x() {}\n"),
        (
            "src/store/rows.rs",
            "pub fn x() {}\npub fn caller() {\n    crate::x();\n    self::x();\n    super::x();\n    super::super::x();\n    super::super::super::x();\n    a::store::x();\n}\n",
        ),
    ];
    // `super` past the crate root names nothing; a library does not see its
    // own name.
    assert_eq!(
        calls(&files),
        [
            edge("caller", "crate::x", Some("src/lib.rs#x#function")),
            edge("caller", "self::x", Some("src/store/rows.rs#x#function")),
            edge("caller", "super::x", Some("src/store/mod.rs#x#function")),
            edge("caller", "super::super::x", Some("src/lib.rs#x#function")),
            edge("caller", "super::super::super::x", None),
            edge("caller", "a::store::x", None),
        ]
    );
}

/// v1 `module_match_picks_the_free_function_of_the_named_module` and
/// `module_paths_follow_the_cargo_target_and_the_call_form`.
#[test]
fn c10_rust_a_module_path_names_one_crate_and_never_a_method() {
    let rank = manifest("pixel-rank");
    let graph = depending("pixel-graph", &[("pixel-rank", "pixel-rank")]);
    let files = [
        ("crates/pixel-rank/Cargo.toml", rank.as_str()),
        (
            "crates/pixel-rank/src/lib.rs",
            "pub mod signals;\npub fn own() { crate::signals::is_test_path(); }\n",
        ),
        (
            "crates/pixel-rank/src/signals.rs",
            "pub fn is_test_path() {}\n",
        ),
        ("crates/pixel-graph/Cargo.toml", graph.as_str()),
        (
            "crates/pixel-graph/src/lib.rs",
            "pub mod signals;\npub mod concept;\n",
        ),
        (
            "crates/pixel-graph/src/signals.rs",
            "pub fn is_test_path() {}\n",
        ),
        (
            "crates/pixel-graph/src/concept.rs",
            "pub struct Concept;\nimpl Concept {\n    pub fn is_test_path(&self) {}\n}\npub fn f(signals: Unknown) {\n    pixel_rank::signals::is_test_path();\n    crate::signals::is_test_path();\n    signals::is_test_path();\n    signals.is_test_path();\n}\n",
        ),
    ];
    // v1 matched `signals::` by suffix and failed only on two candidates
    // (R-9); a path starts in a scope or at a crate, and `signals` is in no
    // scope of `concept`. A method call on a value named `signals` is not a
    // module's function, as in v1.
    assert_eq!(
        calls(&files),
        [
            edge(
                "own",
                "crate::signals::is_test_path",
                Some("crates/pixel-rank/src/signals.rs#is_test_path#function")
            ),
            edge(
                "f",
                "pixel_rank::signals::is_test_path",
                Some("crates/pixel-rank/src/signals.rs#is_test_path#function")
            ),
            edge(
                "f",
                "crate::signals::is_test_path",
                Some("crates/pixel-graph/src/signals.rs#is_test_path#function")
            ),
            edge("f", "signals::is_test_path", None),
            edge("f", "signals.is_test_path", None),
        ]
    );
}

/// v1 `rust_crate_depth_counts_the_target_root`.
#[test]
fn c10_rust_each_cargo_target_is_a_crate_root() {
    let a = manifest("a");
    let files = [
        ("Cargo.toml", a.as_str()),
        ("src/lib.rs", "pub fn root() {}\n"),
        (
            "src/bin/foo.rs",
            "fn root() {}\nfn main() { crate::root(); a::root(); }\n",
        ),
        (
            "tests/all/main.rs",
            "fn root() {}\nfn t() { crate::root(); a::root(); }\n",
        ),
        (
            "build.rs",
            "fn root() {}\nfn main() { crate::root(); a::root(); }\n",
        ),
        ("tools/gen.rs", "fn root() {}\nfn g() { crate::root(); }\n"),
    ];
    // A build script does not see its own library; a file no target
    // reaches has no crate root.
    assert_eq!(
        calls(&files),
        [
            edge("main", "crate::root", Some("src/bin/foo.rs#root#function")),
            edge("main", "a::root", Some("src/lib.rs#root#function")),
            edge("t", "crate::root", Some("tests/all/main.rs#root#function")),
            edge("t", "a::root", Some("src/lib.rs#root#function")),
            edge("main", "crate::root", Some("build.rs#root#function")),
            edge("main", "a::root", None),
            edge("g", "crate::root", None),
        ]
    );
}

/// v1 `a_rust_use_makes_the_imported_same_name_function_exact`.
#[test]
fn c10_rust_a_use_makes_the_imported_namesake_the_callee() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub mod push;\npub mod other;\npub mod ship;\n",
        ),
        ("src/push.rs", "pub struct PushOptions;\npub fn push() {}\n"),
        ("src/other.rs", "pub fn push() {}\npub fn publish() {}\n"),
        (
            "src/ship.rs",
            "use crate::push::{PushOptions, push};\npub fn ship() { push(); publish(); }\n",
        ),
    ];
    assert_eq!(
        calls(&files),
        [
            edge("ship", "push", Some("src/push.rs#push#function")),
            edge("ship", "publish", None),
        ]
    );
}

/// v1 `typed_and_module_receivers_survive_incremental_updates`, full build.
#[test]
fn c10_rust_a_typed_receiver_and_a_crate_path_pick_their_namesake() {
    let (git, guard, idx, ops) = (
        manifest("git"),
        manifest("guard"),
        manifest("idx"),
        depending("ops", &[("git", "git"), ("idx", "idx")]),
    );
    let files = [
        ("crates/git/Cargo.toml", git.as_str()),
        (
            "crates/git/src/lib.rs",
            "pub struct GitRunner;\nimpl GitRunner {\n    pub fn current_branch(&self) {}\n}\n",
        ),
        ("crates/guard/Cargo.toml", guard.as_str()),
        ("crates/guard/src/lib.rs", "pub fn current_branch() {}\n"),
        ("crates/idx/Cargo.toml", idx.as_str()),
        ("crates/idx/src/lib.rs", "pub mod gitsync;\n"),
        ("crates/idx/src/gitsync.rs", "pub fn current_branch() {}\n"),
        ("crates/ops/Cargo.toml", ops.as_str()),
        (
            "crates/ops/src/lib.rs",
            "pub fn run(runner: &git::GitRunner) {\n    runner.current_branch();\n}\npub fn wrap() {\n    idx::gitsync::current_branch();\n}\n",
        ),
    ];
    // v1 read crate names from directories and ran with no manifest and no
    // `idx/src/lib.rs` (M-1); the MVP reads `Cargo.toml` and the `mod`
    // declarations, so the fixture declares both. Both edges are proven
    // where v1 had them `Probable` (R-8, R-9).
    assert_eq!(
        calls(&files),
        [
            edge(
                "run",
                "runner.current_branch",
                Some("crates/git/src/lib.rs#GitRunner::current_branch#method")
            ),
            edge(
                "wrap",
                "idx::gitsync::current_branch",
                Some("crates/idx/src/gitsync.rs#current_branch#function")
            ),
        ]
    );
}

/// v1 `a_rust_use_alias_binds_the_alias_and_survives_an_incremental_update`.
#[test]
fn c10_rust_an_alias_reaches_its_source_whatever_else_defines_the_name() {
    let mut files = vec![
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub mod push;\npub mod other;\npub mod ship;\npub mod third;\n",
        ),
        ("src/push.rs", "pub fn push() {}\n"),
        ("src/other.rs", "pub fn push() {}\n"),
        (
            "src/ship.rs",
            "use crate::push::push as leased;\npub fn ship() { leased(); }\npub fn stray() { push(); }\n",
        ),
    ];
    let expected = [
        edge("ship", "leased", Some("src/push.rs#push#function")),
        edge("stray", "push", None),
    ];
    assert_eq!(calls(&files), expected);
    files.push(("src/third.rs", "pub fn push() {}\n"));
    assert_eq!(calls(&files), expected, "a third `push` changes nothing");
}

/// v1 `a_grouped_rust_use_resolves_each_path_to_its_own_file`.
#[test]
fn c10_rust_a_grouped_use_resolves_each_path_to_its_own_module() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub mod left;\npub mod right;\npub mod push;\npub mod other;\npub mod ship;\n",
        ),
        ("src/left.rs", "pub fn push() {}\n"),
        ("src/right.rs", "pub fn publish() {}\n"),
        ("src/push.rs", "pub mod a;\npub fn y() {}\n"),
        ("src/push/a.rs", "pub fn x() {}\n"),
        (
            "src/other.rs",
            "pub fn push() {}\npub fn publish() {}\npub fn x() {}\npub fn y() {}\n",
        ),
        (
            "src/ship.rs",
            "use crate::{left::push, right::publish};\nuse crate::push::{a::x, y};\npub fn ship() { push(); publish(); x(); y(); }\n",
        ),
    ];
    assert_eq!(
        calls(&files),
        [
            edge("ship", "push", Some("src/left.rs#push#function")),
            edge("ship", "publish", Some("src/right.rs#publish#function")),
            edge("ship", "x", Some("src/push/a.rs#x#function")),
            edge("ship", "y", Some("src/push.rs#y#function")),
        ]
    );
}

/// v1 `a_dangling_path_of_a_grouped_use_resolves_when_its_file_appears`.
#[test]
fn c10_rust_a_dangling_use_resolves_once_its_file_appears() {
    let mut files = vec![
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub mod left;\npub mod right;\npub mod other;\npub mod ship;\n",
        ),
        ("src/right.rs", "pub fn publish() {}\n"),
        ("src/other.rs", "pub fn push() {}\n"),
        (
            "src/ship.rs",
            "use crate::{left::push, right::publish};\npub fn ship() { push(); publish(); }\n",
        ),
    ];
    assert_eq!(calls(&files)[0], edge("ship", "push", None));
    files.push(("src/left.rs", "pub fn push() {}\n"));
    assert_eq!(
        calls(&files)[0],
        edge("ship", "push", Some("src/left.rs#push#function"))
    );
}

/// v1 `a_rust_use_binds_names_only_where_it_is_in_scope`.
#[test]
fn c10_rust_a_use_binds_only_in_its_scope_and_a_glob_proves_a_name() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub mod left;\npub mod right;\npub mod other;\npub mod ship;\npub mod top;\n",
        ),
        ("src/left.rs", "pub fn push() {}\n"),
        ("src/right.rs", "pub fn push() {}\n"),
        ("src/other.rs", "pub fn push() {}\n"),
        (
            "src/ship.rs",
            "mod a {\n    use crate::left::push;\n    pub fn fa() { push(); }\n}\nmod b {\n    use crate::right::*;\n    pub fn fb() { push(); }\n}\n",
        ),
        (
            "src/top.rs",
            "use crate::left::push;\npub fn top() { push(); }\nmod tests {\n    use super::*;\n    fn t() { push(); }\n}\nmod bare {\n    use crate::right::*;\n    fn u() { push(); }\n}\n",
        ),
    ];
    // v1 left `fb` and `u` unresolved: it read no glob. `right::*` is a
    // workspace module's and the scope's only glob, so it proves `push`
    // (decision 1).
    assert_eq!(
        calls(&files),
        [
            edge("fa", "push", Some("src/left.rs#push#function")),
            edge("fb", "push", Some("src/right.rs#push#function")),
            edge("top", "push", Some("src/left.rs#push#function")),
            edge("t", "push", Some("src/left.rs#push#function")),
            edge("u", "push", Some("src/right.rs#push#function")),
        ]
    );
}

/// v1 `an_alias_blocks_the_repo_wide_tier_only_where_it_is_in_scope`.
#[test]
fn c10_rust_an_alias_of_a_missing_item_proves_nothing_and_binds_nothing_elsewhere() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub mod left;\npub mod other;\npub mod ship;\n",
        ),
        ("src/left.rs", "pub fn push() {}\n"),
        ("src/other.rs", "pub fn zap() {}\n"),
        (
            "src/ship.rs",
            "mod a {\n    use crate::left::helper as zap;\n    pub fn fa() { zap(); }\n}\nmod b {\n    pub fn fb() { zap(); }\n}\n",
        ),
    ];
    // v1: `fb`'s `zap()` Probable to `other.rs`, the only file defining it
    // (T2); `zap` is in no scope of `b`.
    assert_eq!(
        calls(&files),
        [edge("fa", "zap", None), edge("fb", "zap", None)]
    );
}

/// v1 `a_block_use_shadows_the_file_level_alias_of_the_same_name`.
#[test]
fn c10_rust_a_block_use_shadows_the_file_use_of_the_same_name() {
    let files = [
        ("Cargo.toml", APP),
        ("src/lib.rs", "pub mod utils;\npub mod ship;\n"),
        ("src/utils.rs", "pub fn early() {}\npub fn later() {}\n"),
        (
            "src/ship.rs",
            "use crate::utils::early as run;\npub fn outer() { run(); }\npub fn inner() {\n    use crate::utils::later as run;\n    run();\n}\n",
        ),
    ];
    assert_eq!(
        calls(&files),
        [
            edge("outer", "run", Some("src/utils.rs#early#function")),
            edge("inner", "run", Some("src/utils.rs#later#function")),
        ]
    );
}

/// v1 `an_out_of_scope_alias_does_not_hide_a_reference_to_a_local_function`.
#[test]
fn c10_rust_an_alias_elsewhere_does_not_hide_a_function_passed_as_a_value() {
    let files = [
        ("Cargo.toml", APP),
        ("src/lib.rs", "pub mod consts;\npub mod ship;\n"),
        ("src/consts.rs", "pub const VALUE: u32 = 1;\n"),
        (
            "src/ship.rs",
            "mod a {\n    use crate::consts::VALUE as zap;\n}\nmod b {\n    fn zap() {}\n    fn consume(_: fn()) {}\n    fn f() { consume(zap); }\n}\n",
        ),
    ];
    assert_eq!(
        references(&files),
        [edge("f", "zap", Some("src/ship.rs#zap#function"))]
    );
}

/// v1 `an_incremental_update_should_store_what_a_full_build_stores` (its
/// Rust tree).
#[test]
fn c10_rust_the_equivalence_tree_resolves_by_imports_only() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub mod push;\npub mod other;\npub mod ship;\npub mod cfg;\n",
        ),
        ("src/push.rs", "pub fn push() {}\npub fn pull() {}\n"),
        ("src/other.rs", "pub fn push() {}\n"),
        (
            "src/ship.rs",
            "use crate::push::push as leased;\nuse crate::{push::pull, other};\npub fn ship() { leased(); pull(); other::push(); }\npub fn stray() { push(); }\n",
        ),
        (
            "src/cfg.rs",
            "pub fn dir() -> String {\n    if let Ok(d) = std::env::var(\"PIXEL_FLOW_DIR\") {\n        return d;\n    }\n    String::new()\n}\n",
        ),
    ];
    // v1: `other::push()` Probable by its module path (R-9); `use crate::
    // {…, other}` binds the module, so the path is proven.
    assert_eq!(
        calls(&files),
        [
            edge("ship", "leased", Some("src/push.rs#push#function")),
            edge("ship", "pull", Some("src/push.rs#pull#function")),
            edge("ship", "other::push", Some("src/other.rs#push#function")),
            edge("stray", "push", None),
            edge("dir", "std::env::var", None),
            edge("dir", "String::new", None),
        ]
    );
}

/// v1 `an_incremental_update_should_retry_every_row_whose_inputs_changed`.
#[test]
fn c10_rust_a_name_only_one_other_file_defines_stays_unresolved() {
    let files = [
        ("Cargo.toml", APP),
        ("src/lib.rs", "pub mod x;\npub mod z;\n"),
        ("src/x.rs", "pub fn helper() {}\n"),
        ("src/z.rs", "pub fn caller() { helper(); }\n"),
    ];
    // v1 resolved it (T2) once a second definition went away.
    assert_eq!(calls(&files), [edge("caller", "helper", None)]);
}

/// v1 `ambiguous_suffix_imports_stay_unresolved_in_every_file_order`,
/// `unique_suffix_imports_and_missing_imports_keep_their_behavior` and
/// `graph_build_and_incremental_refresh_do_not_invent_ambiguous_import_edges`.
#[test]
fn c10_rust_crate_is_the_importers_crate_never_another() {
    let (a, b, caller) = (manifest("a"), manifest("b"), manifest("caller"));
    let mut files = vec![
        ("crates/a/Cargo.toml", a.as_str()),
        ("crates/a/src/lib.rs", "pub mod util;\n"),
        ("crates/a/src/util.rs", "pub fn parse_rust() {}\n"),
        ("crates/b/Cargo.toml", b.as_str()),
        ("crates/b/src/lib.rs", "pub mod util;\n"),
        ("crates/b/src/util.rs", "pub fn parse_rust() {}\n"),
        ("crates/caller/Cargo.toml", caller.as_str()),
        (
            "crates/caller/src/main.rs",
            "use crate::util::parse_rust;\nfn rust_caller() { parse_rust(); }\n",
        ),
    ];
    // v1 resolved `crate::util` to the one `util.rs` it found anywhere (a
    // pinned bug, P-3); `crate` is the caller's crate, which has no `util`.
    let expected = [edge("rust_caller", "parse_rust", None)];
    assert_eq!(calls(&files), expected);
    assert_eq!(
        imports(&files),
        [edge(
            "crates/caller/src/main.rs",
            "crate::util::parse_rust",
            None
        )]
    );
    files.reverse();
    assert_eq!(calls(&files), expected);
}

/// v1 `exact_and_relative_paths_do_not_lose_resolution_to_unrelated_suffixes`.
#[test]
fn c10_rust_a_module_of_the_crate_wins_over_any_other_file_of_its_name() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/main.rs",
            "mod util;\nuse crate::util::run;\nfn main() { run(); }\n",
        ),
        ("src/util.rs", "pub fn run() {}\n"),
        ("other/src/util.rs", "pub fn run() {}\n"),
    ];
    assert_eq!(
        calls(&files),
        [edge("main", "run", Some("src/util.rs#run#function"))]
    );
}

/// v1 `qualified_call_no_longer_links_the_callers_own_same_name_symbol`.
#[test]
fn c10_rust_a_qualified_call_is_never_the_callers_own_namesake() {
    let (bridge, graph, standalone) = (
        depending("bridge", &[("graph", "graph")]),
        manifest("graph"),
        manifest("standalone"),
    );
    let files = [
        ("crates/bridge/Cargo.toml", bridge.as_str()),
        (
            "crates/bridge/src/lib.rs",
            "pub fn f() -> u32 {\n    graph::build::f()\n}\n\npub fn g() -> u32 {\n    f()\n}\n",
        ),
        ("crates/graph/Cargo.toml", graph.as_str()),
        ("crates/graph/src/lib.rs", "pub mod build;\n"),
        (
            "crates/graph/src/build.rs",
            "pub fn f() -> u32 {\n    1\n}\n",
        ),
        ("crates/standalone/Cargo.toml", standalone.as_str()),
        (
            "crates/standalone/src/lib.rs",
            "pub fn h() -> u32 {\n    2\n}\n",
        ),
    ];
    // v1 linked `graph::build::f()` `Probable` by its module path (R-9),
    // with no manifest or `lib.rs` for `graph`; declared, it is proven.
    assert_eq!(
        calls(&files),
        [
            edge(
                "f",
                "graph::build::f",
                Some("crates/graph/src/build.rs#f#function")
            ),
            edge("g", "f", Some("crates/bridge/src/lib.rs#f#function")),
        ]
    );
}

/// v1 `a_module_path_two_files_end_with_stays_unresolved`.
#[test]
fn c10_rust_a_module_path_names_the_module_in_scope_not_a_suffix() {
    let (a, b, app) = (
        manifest("a"),
        manifest("b"),
        depending("app", &[("a", "a"), ("b", "b")]),
    );
    let files = [
        ("crates/a/Cargo.toml", a.as_str()),
        ("crates/a/src/lib.rs", "pub mod util;\n"),
        ("crates/a/src/util.rs", "pub fn f() -> u32 {\n    1\n}\n"),
        ("crates/b/Cargo.toml", b.as_str()),
        ("crates/b/src/lib.rs", "pub mod util;\n"),
        ("crates/b/src/util.rs", "pub fn f() -> u32 {\n    2\n}\n"),
        ("crates/app/Cargo.toml", app.as_str()),
        (
            "crates/app/src/lib.rs",
            "pub fn vague() -> u32 {\n    util::f()\n}\n\npub fn precise() -> u32 {\n    a::util::f()\n}\n",
        ),
    ];
    // v1: `a::util::f()` Probable by suffix (R-9); `a` is a crate.
    assert_eq!(
        calls(&files),
        [
            edge("vague", "util::f", None),
            edge(
                "precise",
                "a::util::f",
                Some("crates/a/src/util.rs#f#function")
            ),
        ]
    );
}

/// v1 `sole_inherent_method_with_a_value_receiver_becomes_a_probable_edge`.
#[test]
fn c10_rust_a_typed_parameter_resolves_in_a_file_no_crate_reaches() {
    let files = [(
        "walk.rs",
        "struct Walker { calls: Vec<String> }\nimpl Walker {\n    fn push_call(&mut self, name: &str) {\n        self.calls.push(name.to_string());\n    }\n}\nfn walk(w: &mut Walker, name: &str) {\n    w.push_call(name);\n}\n",
    )];
    // v1: `Probable` by the sole inherent method (R-10); the parameter's
    // stated type proves it, and a file without a crate still resolves its
    // own items.
    let linked = link(&files);
    let walk: Vec<_> = edges_of(EdgeKind::Call, &linked)
        .into_iter()
        .filter(|(src, _, _)| src == "walk")
        .collect();
    assert_eq!(
        walk,
        [edge(
            "walk",
            "w.push_call",
            Some("walk.rs#Walker::push_call#method")
        )]
    );
}

/// v1 `trait_impl_method_with_a_value_receiver_stays_unresolved`.
#[test]
fn c10_rust_a_std_value_never_reaches_a_workspace_trait_impl() {
    let files = [(
        "clone.rs",
        "struct Boxed;\nimpl Clone for Boxed {\n    fn clone(&self) -> Self { Boxed }\n}\nfn dup(path: &std::path::Path) -> std::path::PathBuf {\n    path.clone()\n}\n",
    )];
    assert_eq!(calls(&files), [edge("dup", "path.clone", None)]);
}

/// v1 `receiver_path_type_selects_the_right_constructor` and
/// `two_same_named_types_keep_the_path_call_unresolved`.
#[test]
fn c10_rust_a_type_path_needs_a_scope_or_a_crate_to_start_from() {
    let (store, a, b, bridge) = (
        manifest("store"),
        manifest("a"),
        manifest("b"),
        depending("bridge", &[("store", "store"), ("a", "a"), ("b", "b")]),
    );
    let store_rs = "pub struct Store;\nimpl Store {\n    pub fn open() -> Store { Store }\n}\n";
    let files = [
        ("crates/store/Cargo.toml", store.as_str()),
        ("crates/store/src/lib.rs", store_rs),
        ("crates/a/Cargo.toml", a.as_str()),
        ("crates/a/src/lib.rs", store_rs),
        ("crates/b/Cargo.toml", b.as_str()),
        ("crates/b/src/lib.rs", store_rs),
        ("crates/bridge/Cargo.toml", bridge.as_str()),
        (
            "crates/bridge/src/lib.rs",
            "pub struct Other;\nimpl Other {\n    pub fn open() -> Other { Other }\n}\npub fn connect() {\n    crate::store::Store::open();\n    store::Store::open();\n    a::Store::open();\n}\n",
        ),
    ];
    // v1: `crate::store::Store::open()` Probable by the type segment (R-8),
    // and `a::Store::open()` unresolved, two `Store::open` existing. The
    // bridge has no module `store`; `a` and `store` are crates.
    assert_eq!(
        calls(&files),
        [
            edge("connect", "crate::store::Store::open", None),
            edge(
                "connect",
                "store::Store::open",
                Some("crates/store/src/lib.rs#Store::open#method")
            ),
            edge(
                "connect",
                "a::Store::open",
                Some("crates/a/src/lib.rs#Store::open#method")
            ),
        ]
    );
}
