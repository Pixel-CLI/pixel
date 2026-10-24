// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Resolution, one class of input per test (C-10: "every caller edge comes
//! from a call site resolved by that language's rule file (imports,
//! aliases, shadowing); a call the rules cannot resolve gives no edge").
//! Each fixture is a small workspace; each expectation is the exact list
//! of edges of one kind, `None` for an unresolved one.

use super::super::{AssocKind, SelfKind};
use super::*;

const APP: &str = "[package]\nname = \"app\"\n";

// D44: a uid writes `%` and `#` of its path escaped, and the rule file
// rebuilds a definition's uid from its path.
#[test]
fn c10_rust_a_crate_under_a_hash_or_percent_sign_still_resolves() {
    let files = [
        ("odd#dir%/Cargo.toml", APP),
        (
            "odd#dir%/src/lib.rs",
            "pub mod a;\nuse crate::a;\npub fn top() { a::run(); }\n",
        ),
        ("odd#dir%/src/a.rs", "pub fn run() {}\n"),
    ];
    assert_eq!(
        calls(&files),
        [edge(
            "top",
            "a::run",
            Some("odd%23dir%25/src/a.rs#run#function")
        )]
    );
    assert_eq!(
        imports(&files),
        [edge(
            "odd#dir%/src/lib.rs",
            "crate::a",
            Some("odd%23dir%25/src/a.rs#odd#dir%/src/a.rs#file")
        )]
    );
}

#[test]
fn c10_rust_module_paths_follow_mod_declarations() {
    let files = [
        ("Cargo.toml", APP),
        ("src/lib.rs", "pub mod a;\npub mod c;\n"),
        ("src/a.rs", "pub mod b;\npub fn in_a() {}\n"),
        (
            "src/a/b.rs",
            "pub fn deep() {}\npub fn run() {\n    super::in_a();\n    self::local();\n    crate::c::top();\n    crate::a::b::deep();\n}\nfn local() {}\n",
        ),
        (
            "src/c/mod.rs",
            "pub fn top() {}\npub fn go() { super::a::in_a(); }\n",
        ),
    ];
    // v1 never reached an item of a parent module, and read `self::` and
    // `super::` one level off from a file that is not `mod.rs` (P-3).
    assert_eq!(
        calls(&files),
        [
            edge("run", "super::in_a", Some("src/a.rs#in_a#function")),
            edge("run", "self::local", Some("src/a/b.rs#local#function")),
            edge("run", "crate::c::top", Some("src/c/mod.rs#top#function")),
            edge("run", "crate::a::b::deep", Some("src/a/b.rs#deep#function")),
            edge("go", "super::a::in_a", Some("src/a.rs#in_a#function")),
        ]
    );
}

#[test]
fn c10_rust_module_paths_a_file_no_mod_declares_is_no_module() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "mod twice;\npub fn f() { crate::loose::g(); twice::g(); }\n",
        ),
        ("src/loose.rs", "pub fn g() {}\n"),
        ("src/twice.rs", "pub fn g() {}\n"),
        ("src/twice/mod.rs", "pub fn g() {}\n"),
    ];
    // `loose.rs` is never declared; `mod twice;` has two candidate files,
    // which Rust rejects: neither is the module.
    assert_eq!(
        calls(&files),
        [
            edge("f", "crate::loose::g", None),
            edge("f", "twice::g", None)
        ]
    );
}

#[test]
fn c10_rust_crate_names_come_from_cargo_toml() {
    let files = [
        ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
        (
            "crates/store/Cargo.toml",
            "[package]\nname = \"mvp-store\"\n",
        ),
        (
            "crates/store/src/lib.rs",
            "pub fn open() {}\npub fn own() { mvp_store::open(); }\n",
        ),
        (
            "crates/store/tests/it.rs",
            "fn it() { mvp_store::open(); }\n",
        ),
        (
            "crates/model/Cargo.toml",
            "[package]\nname = \"model-x\"\n\n[lib]\nname = \"model\"\n",
        ),
        ("crates/model/src/lib.rs", "pub fn shape() {}\n"),
        (
            "crates/app/Cargo.toml",
            "[package]\nname = \"app\"\n\n[dependencies]\npixel-store = { path = \"../store\" }\n\
             model-x = { path = \"../model\" }\n",
        ),
        (
            "crates/app/src/main.rs",
            "fn main() { mvp_store::open(); model::shape(); model_x::shape(); }\n",
        ),
    ];
    // v1 read no manifest: a workspace crate's name resolved nothing (P-3).
    // A library does not see its own name; its tests do; `[lib] name` is
    // the crate's name, not the package's.
    assert_eq!(
        calls(&files),
        [
            edge("own", "mvp_store::open", None),
            edge(
                "it",
                "mvp_store::open",
                Some("crates/store/src/lib.rs#open#function")
            ),
            edge(
                "main",
                "mvp_store::open",
                Some("crates/store/src/lib.rs#open#function")
            ),
            edge(
                "main",
                "model::shape",
                Some("crates/model/src/lib.rs#shape#function")
            ),
            edge("main", "model_x::shape", None),
        ]
    );
}

// D22: `[dependencies]` were never read, so a registry crate sharing a
// workspace library's name resolved into the workspace library, and any
// workspace library was reachable from a package that did not depend on it.
#[test]
fn c10_rust_crate_names_are_the_dependencies_a_package_declares() {
    let lib = |name: &str| format!("[package]\nname = \"{name}\"\n");
    let libs = ["config", "util", "helper", "devonly", "buildonly"].map(lib);
    let files = [
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.dependencies]\nutil = { path = \"crates/util\" }\n",
        ),
        ("crates/config/Cargo.toml", libs[0].as_str()),
        ("crates/config/src/lib.rs", "pub fn go() {}\n"),
        ("crates/util/Cargo.toml", libs[1].as_str()),
        ("crates/util/src/lib.rs", "pub fn go() {}\n"),
        ("crates/helper/Cargo.toml", libs[2].as_str()),
        ("crates/helper/src/lib.rs", "pub fn go() {}\n"),
        ("crates/devonly/Cargo.toml", libs[3].as_str()),
        ("crates/devonly/src/lib.rs", "pub fn go() {}\n"),
        ("crates/buildonly/Cargo.toml", libs[4].as_str()),
        ("crates/buildonly/src/lib.rs", "pub fn go() {}\n"),
        (
            "crates/app/Cargo.toml",
            "[package]\nname = \"app\"\n\n[dependencies]\nutil.workspace = true\n\
             cfgx = { path = \"../config\", package = \"config\" }\n\n\
             [target.'cfg(unix)'.dependencies]\nhelper = { path = \"../helper\" }\n\n\
             [dev-dependencies]\ndevonly = { path = \"../devonly\" }\n\n\
             [build-dependencies.buildonly]\npath = \"../buildonly\"\n",
        ),
        (
            "crates/app/src/main.rs",
            "fn main() {\n    util::go();\n    cfgx::go();\n    helper::go();\n    config::go();\n    buildonly::go();\n}\n\
             #[cfg(test)]\nmod tests {\n    fn t() { devonly::go(); }\n}\n",
        ),
        (
            "crates/app/build.rs",
            "fn main() {\n    buildonly::go();\n    util::go();\n}\n",
        ),
        (
            "crates/app/tests/it.rs",
            "fn it() {\n    devonly::go();\n    util::go();\n}\n",
        ),
        (
            "crates/reg/Cargo.toml",
            "[package]\nname = \"reg\"\n\n[dependencies]\nconfig = \"0.14\"\n",
        ),
        (
            "crates/reg/src/main.rs",
            "fn main() {\n    config::go();\n}\n",
        ),
    ];
    let go = |krate: &str| format!("crates/{krate}/src/lib.rs#go#function");
    let (util, config, helper, devonly, buildonly) = (
        go("util"),
        go("config"),
        go("helper"),
        go("devonly"),
        go("buildonly"),
    );
    // Checked with cargo 1.99: `config` and `buildonly` are not names of
    // `app`'s binary (E0433), `util` is none of its build script's; the
    // registry's `config` is what `reg` calls.
    assert_eq!(
        calls(&files),
        [
            edge("main", "util::go", Some(&util)),
            edge("main", "cfgx::go", Some(&config)),
            edge("main", "helper::go", Some(&helper)),
            edge("main", "config::go", None),
            edge("main", "buildonly::go", None),
            edge("t", "devonly::go", Some(&devonly)),
            edge("main", "buildonly::go", Some(&buildonly)),
            edge("main", "util::go", None),
            edge("it", "devonly::go", Some(&devonly)),
            edge("it", "util::go", Some(&util)),
            edge("main", "config::go", None),
        ]
    );
}

#[test]
fn c10_rust_crate_names_a_binary_is_a_crate_of_its_own() {
    let files = [
        ("Cargo.toml", "[package]\nname = \"a\"\n"),
        ("src/lib.rs", "pub mod util;\n"),
        ("src/util.rs", "pub fn run() {}\n"),
        (
            "src/bin/foo/main.rs",
            "mod util;\nfn main() { crate::util::run(); a::util::run(); }\n",
        ),
        ("src/bin/foo/util.rs", "pub fn run() {}\n"),
    ];
    // `crate::` in a binary is the binary; the library is `a::`.
    assert_eq!(
        calls(&files),
        [
            edge(
                "main",
                "crate::util::run",
                Some("src/bin/foo/util.rs#run#function")
            ),
            edge("main", "a::util::run", Some("src/util.rs#run#function")),
        ]
    );
}

#[test]
fn c10_rust_use_alias_binds_the_alias_and_not_the_source() {
    let files = [
        ("Cargo.toml", APP),
        ("src/lib.rs", "pub mod push;\npub mod ship;\n"),
        ("src/push.rs", "pub fn push() {}\n"),
        (
            "src/ship.rs",
            "use crate::push::push as leased;\npub fn ship() { leased(); push(); }\n",
        ),
    ];
    assert_eq!(
        calls(&files),
        [
            edge("ship", "leased", Some("src/push.rs#push#function")),
            edge("ship", "push", None),
        ]
    );
}

#[test]
fn c10_rust_use_self_in_a_group_binds_the_module() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub mod store;\nuse self::store::{self as db, Store};\npub fn go() { db::open(); }\n",
        ),
        ("src/store.rs", "pub fn open() {}\npub struct Store;\n"),
    ];
    // v1 bound nothing for `self` in a group (U-5).
    assert_eq!(
        calls(&files),
        [edge("go", "db::open", Some("src/store.rs#open#function"))]
    );
    assert_eq!(
        imports(&files),
        [
            edge(
                "src/lib.rs",
                "self::store::self",
                Some("src/store.rs#src/store.rs#file")
            ),
            edge(
                "src/lib.rs",
                "self::store::Store",
                Some("src/store.rs#Store#struct")
            ),
        ]
    );
}

#[test]
fn c10_rust_glob_of_a_workspace_module_proves_a_name() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "mod util;\npub fn helper() {}\n#[cfg(test)]\nmod tests {\n    use super::*;\n    fn t() { helper(); util::deep(); }\n}\n",
        ),
        ("src/util.rs", "pub fn deep() {}\n"),
    ];
    // A child sees its parent's private items through `use super::*`.
    assert_eq!(
        calls(&files),
        [
            edge("t", "helper", Some("src/lib.rs#helper#function")),
            edge("t", "util::deep", Some("src/util.rs#deep#function")),
        ]
    );
}

#[test]
fn c10_rust_glob_of_a_dependency_leaves_unbound_names_unresolved() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub fn helper() {}\nmod tests {\n    use super::*;\n    use proptest::prelude::*;\n    fn own() {}\n    fn t() { helper(); own(); }\n}\n",
        ),
    ];
    // `proptest::prelude` may export a `helper`; the module's own item is
    // explicit and beats every glob.
    assert_eq!(
        calls(&files),
        [
            edge("t", "helper", None),
            edge("t", "own", Some("src/lib.rs#own#function")),
        ]
    );
}

#[test]
fn c10_rust_glob_two_providers_prove_neither_and_explicit_beats_glob() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub mod a;\npub mod b;\nmod user {\n    use crate::a::*;\n    use crate::b::*;\n    fn f() { shared(); only_a(); hidden(); }\n}\n\
             mod picky {\n    use crate::a::*;\n    use crate::b::shared;\n    fn g() { shared(); }\n}\n",
        ),
        (
            "src/a.rs",
            "pub fn shared() {}\npub fn only_a() {}\nfn hidden() {}\n",
        ),
        ("src/b.rs", "pub fn shared() {}\n"),
    ];
    // A glob from outside `a` does not import its private `hidden`.
    assert_eq!(
        calls(&files),
        [
            edge("f", "shared", None),
            edge("f", "only_a", Some("src/a.rs#only_a#function")),
            edge("f", "hidden", None),
            edge("g", "shared", Some("src/b.rs#shared#function")),
        ]
    );
}

// D39: `pub(in path)` was read as `pub(crate)`, so a glob from outside the
// path imported a name rustc does not, shadowing the prelude's `drop` or a
// module-level item.
#[test]
fn c10_rust_glob_a_visibility_restricted_to_a_path_is_seen_inside_it_only() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/main.rs",
            "pub mod x {\n    pub(in crate::x) fn drop<T>(_: T) {}\n    pub fn other() {}\n    pub(in crate::x) fn helper() {}\n    pub mod inner {\n\
             \x20       pub(in crate::x) fn deep() {}\n        pub(super) fn up() {}\n        pub(crate) fn everywhere() {}\n        pub(in super::super) fn root_only() {}\n    }\n\
             \x20   pub mod sib {\n        use super::inner::*;\n        pub fn f() { deep(); up(); everywhere(); root_only(); }\n    }\n}\n\
             pub mod y {\n    use crate::x::*;\n    use crate::x::inner::*;\n    pub fn helper() {}\n    pub fn deep() {}\n    pub fn up() {}\n\
             \x20   pub fn f() { drop(1); other(); }\n    pub fn g() {\n        use crate::x::*;\n        helper();\n    }\n    pub fn h() { everywhere(); root_only(); }\n}\n\
             fn main() { y::f(); y::g(); y::h(); x::sib::f(); }\n",
        ),
    ];
    let at = |name: &str| Some(format!("src/main.rs#{name}#function"));
    let edge =
        |src: &str, target: &str, dst: Option<String>| (src.to_owned(), target.to_owned(), dst);
    // rustc 1.99 runs std's `drop`, `y::helper`, and the rest as below.
    assert_eq!(
        calls(&files),
        [
            edge("f", "deep", at("deep")),
            edge("f", "up", at("up")),
            edge("f", "everywhere", at("everywhere")),
            edge("f", "root_only", at("root_only")),
            edge("f", "drop", None),
            edge("f", "other", at("other")),
            edge(
                "g",
                "helper",
                Some("src/main.rs#helper#function~2".to_owned())
            ),
            edge("h", "everywhere", at("everywhere")),
            edge("h", "root_only", at("root_only")),
            edge("main", "y::f", Some("src/main.rs#f#function~2".to_owned())),
            edge("main", "y::g", at("g")),
            edge("main", "y::h", at("h")),
            edge("main", "x::sib::f", at("f")),
        ]
    );
}

#[test]
fn c10_rust_glob_a_private_one_of_another_module_is_not_seen() {
    let files = [
        ("Cargo.toml", APP),
        ("src/lib.rs", "pub mod build;\npub mod cluster;\n"),
        (
            "src/build.rs",
            "use rayon::prelude::*;\npub fn build_graph() {}\n",
        ),
        (
            "src/cluster.rs",
            "use crate::build::build_graph;\npub fn f() { build_graph(); crate::build::build_graph(); }\n",
        ),
    ];
    // `build`'s private glob binds nothing for `cluster`, so the dependency
    // it reads cannot add a `build_graph` that `cluster` would see.
    assert_eq!(
        imports(&files),
        [
            edge("src/build.rs", "rayon::prelude::*", None),
            edge(
                "src/cluster.rs",
                "crate::build::build_graph",
                Some("src/build.rs#build_graph#function")
            ),
        ]
    );
    assert_eq!(
        calls(&files),
        [
            edge(
                "f",
                "build_graph",
                Some("src/build.rs#build_graph#function")
            ),
            edge(
                "f",
                "crate::build::build_graph",
                Some("src/build.rs#build_graph#function")
            ),
        ]
    );
}

#[test]
fn c10_rust_glob_a_crate_name_in_a_use_survives_a_dependency_glob() {
    let files = [
        ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
        ("crates/git/Cargo.toml", "[package]\nname = \"pixel-git\"\n"),
        ("crates/git/src/lib.rs", "pub fn batch() {}\n"),
        (
            "crates/index/Cargo.toml",
            "[package]\nname = \"mvp-index\"\n\n[dependencies]\npixel-git = { path = \"../git\" }\n\
             rayon = \"1\"\n",
        ),
        (
            "crates/index/src/lib.rs",
            "use rayon::prelude::*;\nuse pixel_git::batch;\npub fn f() { batch(); pixel_git::batch(); }\n",
        ),
    ];
    // In a `use`, Rust rejects a first segment both a glob and an outer
    // scope (here the crate names) provide, so `pixel_git` is the crate. In
    // code a glob shadows the crate names, and `rayon::prelude` may.
    assert_eq!(
        imports(&files),
        [
            edge("crates/index/src/lib.rs", "rayon::prelude::*", None),
            edge(
                "crates/index/src/lib.rs",
                "pixel_git::batch",
                Some("crates/git/src/lib.rs#batch#function")
            ),
        ]
    );
    assert_eq!(
        calls(&files),
        [
            edge("f", "batch", Some("crates/git/src/lib.rs#batch#function")),
            edge("f", "pixel_git::batch", None),
        ]
    );
}

#[test]
fn c10_rust_block_use_and_block_item_shadow_the_module() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub mod utils;\nuse crate::utils::early as run;\npub fn outer() { run(); }\npub fn inner() {\n    use crate::utils::later as run;\n    run();\n}\n\
             pub fn local() {\n    run();\n    fn run() {}\n}\n",
        ),
        ("src/utils.rs", "pub fn early() {}\npub fn later() {}\n"),
    ];
    // A block's items are in scope in the whole block, before them too.
    assert_eq!(
        calls(&files),
        [
            edge("outer", "run", Some("src/utils.rs#early#function")),
            edge("inner", "run", Some("src/utils.rs#later#function")),
            edge("local", "run", Some("src/lib.rs#run#function")),
        ]
    );
}

#[test]
fn c10_rust_shadowing_a_local_of_the_callee_name_is_not_the_function() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub fn push() {}\npub fn a() {\n    let push = || 1;\n    push();\n}\npub fn b(push: fn()) {\n    push();\n    register(push);\n}\n\
             pub fn c() {\n    push();\n    register(push);\n}\nfn register(_: fn()) {}\n",
        ),
    ];
    assert_eq!(
        calls(&files),
        [
            edge("a", "push", None),
            edge("b", "push", None),
            edge("b", "register", Some("src/lib.rs#register#function")),
            edge("c", "push", Some("src/lib.rs#push#function")),
            edge("c", "register", Some("src/lib.rs#register#function")),
        ]
    );
    // An argument a parameter binds is a value: no reference site at all.
    assert_eq!(
        references(&files),
        [edge("c", "push", Some("src/lib.rs#push#function"))]
    );
}

#[test]
fn c10_rust_self_paths_and_self_methods_reach_the_impl() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub fn run() {}\npub struct S(u8);\nimpl S {\n    pub fn new() -> Self { Self(0) }\n    fn run(&self) {}\n    fn go(&self) {\n        self.run();\n        Self::new();\n        S::new();\n        run();\n    }\n}\n",
        ),
    ];
    // v1 linked `self.run()` to the free `run`, first by kind priority
    // (R-13); `self` is the impl's type (decision 1).
    assert_eq!(
        calls(&files),
        [
            edge("S::new", "Self", Some("src/lib.rs#S#struct")),
            edge("S::go", "self.run", Some("src/lib.rs#S::run#method")),
            edge("S::go", "Self::new", Some("src/lib.rs#S::new#method")),
            edge("S::go", "S::new", Some("src/lib.rs#S::new#method")),
            edge("S::go", "run", Some("src/lib.rs#run#function")),
        ]
    );
}

#[test]
fn c10_rust_inherent_method_beats_a_trait_method() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub trait Named {\n    fn name(&self) -> u8;\n    fn greet(&self) {}\n}\npub struct S;\nimpl S {\n    pub fn name(&self) -> u8 { 1 }\n}\n\
             impl Named for S {\n    fn name(&self) -> u8 { 2 }\n}\npub fn f(s: &S) {\n    s.name();\n    s.greet();\n    <S as Named>::name(s);\n}\n",
        ),
    ];
    // A trait method `greet` the impl does not override is the trait's.
    assert_eq!(
        calls(&files),
        [
            edge("f", "s.name", Some("src/lib.rs#S::name#method")),
            edge("f", "s.greet", Some("src/lib.rs#Named::greet#method")),
            edge(
                "f",
                "<S as Named>::name",
                Some("src/lib.rs#<S as Named>::name#method")
            ),
        ]
    );
}

// D42: `&'commut self` ended in `mut` once whitespace was removed and was
// read as `&mut self`, so the trait's `&self` method came first.
#[test]
fn c10_rust_how_a_method_takes_self_is_read_from_the_tree() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub struct S;\npub trait Tr {\n    fn go(&self);\n    fn put(&mut self);\n}\nimpl Tr for S {\n    fn go(&self) {}\n    fn put(&mut self) {}\n}\n\
             impl<'commut> S {\n    pub fn new() -> Self { S }\n    pub fn go(&'commut self) {}\n    pub fn put(self: &mut Self) {}\n}\n\
             pub fn f() {\n    S::new().go();\n    S::new().put();\n}\n",
        ),
    ];
    // rustc 1.99 runs both inherent methods.
    assert_eq!(
        calls(&files),
        [
            edge("f", "S::new().go", Some("src/lib.rs#S::go#method")),
            edge("f", "S::new", Some("src/lib.rs#S::new#method")),
            edge("f", "S::new().put", Some("src/lib.rs#S::put#method")),
            edge("f", "S::new", Some("src/lib.rs#S::new#method")),
        ]
    );
    let receivers: Vec<(String, SelfKind)> = facts(&extract(
        "src/lib.rs",
        "struct S;\nimpl<'a, 'commut> S {\n    fn a(self) {}\n    fn b(mut self) {}\n    fn c(&self) {}\n    fn d(&mut self) {}\n    fn e(&'a self) {}\n    fn f(&'a mut self) {}\n    fn g(&'commut self) {}\n\
         fn h(self: Self) {}\n    fn i(mut self: Self) {}\n    fn j(self: &Self) {}\n    fn k(self: &'a mut Self) {}\n    fn l(self: Box<Self>) {}\n    fn m(self: &&Self) {}\n    fn n(self: &'commut Self) {}\n    fn o() {}\n}\n",
    ))
    .assocs
    .into_iter()
    .map(|a| match a.kind {
        AssocKind::Fn { receiver, .. } => (a.name, receiver),
        AssocKind::Const => (a.name, SelfKind::None),
    })
    .collect();
    let expected = [
        ("a", SelfKind::Value),
        ("b", SelfKind::Value),
        ("c", SelfKind::Ref),
        ("d", SelfKind::RefMut),
        ("e", SelfKind::Ref),
        ("f", SelfKind::RefMut),
        ("g", SelfKind::Ref),
        ("h", SelfKind::Value),
        ("i", SelfKind::Value),
        ("j", SelfKind::Ref),
        ("k", SelfKind::RefMut),
        ("l", SelfKind::Typed),
        ("m", SelfKind::Typed),
        ("n", SelfKind::Ref),
        ("o", SelfKind::None),
    ]
    .map(|(name, kind)| (name.to_owned(), kind));
    assert_eq!(receivers, expected);
}

#[test]
fn c10_rust_typed_receivers_resolve_the_method() {
    let files = [
        ("Cargo.toml", APP),
        ("src/lib.rs", "pub mod git;\npub mod user;\n"),
        (
            "src/git.rs",
            "pub struct Runner { pub root: u8 }\nimpl Runner {\n    pub fn new() -> Runner { Runner { root: 0 } }\n    pub fn branch(&self) {}\n    pub fn clone(&self) -> Runner { Runner { root: self.root } }\n}\n",
        ),
        (
            "src/user.rs",
            "use std::sync::Arc;\nuse crate::git::{self, Runner};\npub fn f(r: &git::Runner, shared: Arc<Runner>) {\n    r.branch();\n    shared.branch();\n    shared.clone();\n    let made = Runner::new();\n    made.branch();\n    let lit = Runner { root: 1 };\n    lit.branch();\n    Runner::new().branch();\n}\n",
        ),
    ];
    // `shared.clone()` is `Arc`'s own `clone`, not the pointee's.
    assert_eq!(
        calls(&files),
        [
            edge("f", "r.branch", Some("src/git.rs#Runner::branch#method")),
            edge(
                "f",
                "shared.branch",
                Some("src/git.rs#Runner::branch#method")
            ),
            edge("f", "shared.clone", None),
            edge("f", "Runner::new", Some("src/git.rs#Runner::new#method")),
            edge("f", "made.branch", Some("src/git.rs#Runner::branch#method")),
            edge("f", "lit.branch", Some("src/git.rs#Runner::branch#method")),
            edge(
                "f",
                "Runner::new().branch",
                Some("src/git.rs#Runner::branch#method")
            ),
            edge("f", "Runner::new", Some("src/git.rs#Runner::new#method")),
        ]
    );
}

#[test]
fn c10_rust_typed_receivers_a_typed_self_is_the_pointer_it_states() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub struct S;\nimpl S {\n    pub fn go(&self) {}\n    pub fn clone(&self) -> S { S }\n    pub fn boxed(self: Box<Self>) {\n        self.go();\n        self.clone();\n    }\n    pub fn borrowed(self: &Self) {\n        self.go();\n    }\n}\n",
        ),
    ];
    // Called on a `Box<Self>`, `clone` is the box's own.
    assert_eq!(
        calls(&files),
        [
            edge("S::boxed", "self.go", Some("src/lib.rs#S::go#method")),
            edge("S::boxed", "self.clone", None),
            edge("S::borrowed", "self.go", Some("src/lib.rs#S::go#method")),
        ]
    );
}

#[test]
fn c10_rust_typed_receivers_a_trait_object_calls_the_trait_method() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub trait Shape {\n    fn area(&self) -> u8;\n    fn describe(&self) { self.area(); }\n}\npub fn total(s: &dyn Shape, t: impl Shape, b: Box<dyn Shape>) {\n    s.area();\n    t.area();\n    b.area();\n}\n",
        ),
    ];
    assert_eq!(
        calls(&files),
        [
            edge(
                "Shape::describe",
                "self.area",
                Some("src/lib.rs#Shape::area#method")
            ),
            edge("total", "s.area", Some("src/lib.rs#Shape::area#method")),
            edge("total", "t.area", Some("src/lib.rs#Shape::area#method")),
            edge("total", "b.area", Some("src/lib.rs#Shape::area#method")),
        ]
    );
}

#[test]
fn c10_rust_typed_receivers_an_unknown_receiver_is_unresolved() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub struct Walker;\nimpl Walker {\n    pub fn push_call(&mut self) {}\n}\npub fn walk(w: &mut Walker, v: Vec<u8>) {\n    w.push_call();\n    let x = make();\n    x.push_call();\n    v.push_call();\n    make().push_call();\n}\nfn make() -> Walker { Walker }\n",
        ),
    ];
    // v1 linked `x.push_call()` to the file's only method of that name
    // (R-10, Probable); the type of `x` is not stated (decision 1).
    assert_eq!(
        calls(&files),
        [
            edge(
                "walk",
                "w.push_call",
                Some("src/lib.rs#Walker::push_call#method")
            ),
            edge("walk", "make", Some("src/lib.rs#make#function")),
            edge("walk", "x.push_call", None),
            edge("walk", "v.push_call", None),
            edge("walk", "make().push_call", None),
            edge("walk", "make", Some("src/lib.rs#make#function")),
        ]
    );
}

#[test]
fn c10_rust_typed_receivers_a_constructor_must_return_self() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub struct Store;\nimpl Store {\n    pub fn new() -> Result<Store, ()> { Ok(Store) }\n    pub fn open(&self) {}\n}\n#[derive(Default)]\npub struct Config;\nimpl Config {\n    pub fn load(&self) {}\n}\n\
             pub fn f() {\n    let s = Store::new();\n    s.open();\n    let c = Config::default();\n    c.load();\n}\n",
        ),
    ];
    // v1 took any `new` for a constructor by convention (Y-7); the MVP
    // checks the function returns `Self`, or that `Default` is derived.
    let method_calls: Vec<_> = calls(&files)
        .into_iter()
        .filter(|(_, target, _)| target.contains('.'))
        .collect();
    assert_eq!(
        method_calls,
        [
            edge("f", "s.open", None),
            edge("f", "c.load", Some("src/lib.rs#Config::load#method")),
        ]
    );
}

#[test]
fn c10_rust_typed_receivers_a_derive_competes_with_a_trait_impl() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "use std::fmt;\n#[derive(Debug)]\npub struct S;\nimpl fmt::Display for S {\n    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { Ok(()) }\n}\npub struct T;\n\
             impl fmt::Display for T {\n    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { Ok(()) }\n}\npub fn f(t: &T, out: &mut fmt::Formatter<'_>) {\n    S {}.fmt(out);\n    T {}.fmt(out);\n    t.fmt(out);\n}\n",
        ),
    ];
    // `#[derive(Debug)]` gives `S` a second `fmt` no symbol carries. The
    // receivers are values: a reference read as its type could be a `&mut
    // T`, which reaches std's `Display for &mut T` first.
    let method_calls: Vec<_> = calls(&files)
        .into_iter()
        .filter(|(_, target, _)| target.contains('.'))
        .collect();
    assert_eq!(
        method_calls,
        [
            edge("f", "S{}.fmt", None),
            edge(
                "f",
                "T{}.fmt",
                Some("src/lib.rs#<T as Display>::fmt#method")
            ),
            edge("f", "t.fmt", None),
        ]
    );
}

#[test]
fn c10_rust_typed_receivers_a_std_blanket_method_competes() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub struct W(u8);\nimpl From<u8> for W {\n    fn from(v: u8) -> Self { W(v) }\n}\npub struct S;\nimpl S {\n    pub fn into(&self) -> u8 { 1 }\n    pub fn size(&self) -> u8 { 1 }\n}\n\
             pub fn f(s: &S, w: W) {\n    W::from(1);\n    s.into();\n    s.size();\n}\n",
        ),
    ];
    // `W::from(w)` would be std's reflexive `From<W> for W`, and a value
    // `s` reaches `Into::into(self)` before `S::into(&self)`: the names std
    // gives every type prove nothing.
    let in_f: Vec<_> = calls(&files)
        .into_iter()
        .filter(|(src, _, _)| src == "f")
        .collect();
    assert_eq!(
        in_f,
        [
            edge("f", "W::from", None),
            edge("f", "s.into", None),
            edge("f", "s.size", Some("src/lib.rs#S::size#method")),
        ]
    );
}

// D19: a private inherent method shadowed the trait method rustc calls
// from outside the impl's module, by method call and by path.
#[test]
fn c10_rust_an_inherent_item_the_caller_cannot_see_is_no_candidate() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub mod a;\npub mod b;\npub trait Tr {\n    fn go(&self);\n    fn make() -> u8;\n    fn up(&self);\n    fn put(&self);\n    fn sup(&self);\n}\n\
             pub fn f(s: &a::S) {\n    s.go();\n    a::S::go(s);\n    a::S::make();\n    s.up();\n    s.put();\n    s.sup();\n}\n",
        ),
        (
            "src/a.rs",
            "pub struct S;\nimpl S {\n    fn go(&self) {}\n    fn make() -> u8 { 0 }\n}\nimpl crate::Tr for S {\n    fn go(&self) {}\n    fn make() -> u8 { 1 }\n    fn up(&self) {}\n    fn put(&self) {}\n    fn sup(&self) {}\n}\n\
             pub mod child {\n    use crate::Tr;\n    pub fn f(s: &super::S) { s.go(); }\n}\n",
        ),
        (
            "src/b.rs",
            "use crate::Tr;\npub mod inner {\n    impl crate::a::S {\n        fn up(&self) {}\n        pub(crate) fn put(&self) {}\n        pub(super) fn sup(&self) {}\n    }\n    pub fn g(s: &crate::a::S) { s.up(); }\n}\n\
             pub fn h(s: &crate::a::S) { s.sup(); s.up(); }\n",
        ),
    ];
    // Each expectation is what rustc 1.99 runs: an item is seen from the
    // module of its impl and that module's descendants (`pub(super)` from
    // the parent's), not from the type's module.
    let trait_impl = |name: &str| Some(format!("src/a.rs#<S as Tr>::{name}#method"));
    let expected = [
        ("f", "s.go", trait_impl("go")),
        ("f", "a::S::go", trait_impl("go")),
        ("f", "a::S::make", trait_impl("make")),
        ("f", "s.up", trait_impl("up")),
        ("f", "s.put", Some("src/b.rs#S::put#method".to_owned())),
        ("f", "s.sup", trait_impl("sup")),
        ("f", "s.go", Some("src/a.rs#S::go#method".to_owned())),
        ("g", "s.up", Some("src/b.rs#S::up#method".to_owned())),
        ("h", "s.sup", Some("src/b.rs#S::sup#method".to_owned())),
        ("h", "s.up", trait_impl("up")),
    ]
    .map(|(src, target, dst)| (src.to_owned(), target.to_owned(), dst));
    assert_eq!(calls(&files), expected);
}

// D18: an impl whose self type the rules could not resolve was dropped, so
// the trait method won over the inherent method rustc 1.99 runs.
#[test]
fn c10_rust_an_impl_the_rules_cannot_place_leaves_its_names_unresolved() {
    let files = [
        ("Cargo.toml", APP),
        ("src/lib.rs", "pub mod a;\npub mod b;\npub mod c;\n"),
        (
            "src/a.rs",
            "pub struct S;\npub trait Tr {\n    fn go(&self);\n    fn other(&self);\n}\nimpl Tr for S {\n    fn go(&self) {}\n    fn other(&self) {}\n}\n",
        ),
        (
            "src/b.rs",
            "use crate::a::*;\nuse std::io::*;\nimpl S {\n    pub fn go(&self) {}\n}\n",
        ),
        (
            "src/c.rs",
            "use crate::a::{S, Tr};\npub fn f(s: &S) {\n    s.go();\n    S::go(s);\n    s.other();\n}\n",
        ),
    ];
    // `S` in b.rs may be std::io's (a glob the rules do not read), so the
    // impl's `go` may be any type's: rustc calls it here.
    assert_eq!(
        calls(&files),
        [
            edge("f", "s.go", None),
            edge("f", "S::go", None),
            edge("f", "s.other", Some("src/a.rs#<S as Tr>::other#method")),
        ]
    );
}

// D21: a trait reached only through a renamed re-export or a glob of
// `pub use Tr as _` was judged out of scope, so a later step's inherent
// method was linked.
#[test]
fn c10_rust_a_trait_is_in_scope_under_any_name_a_binding_gives_it() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub mod a { pub trait Tr { fn go(self); } }\npub mod m { pub use crate::a::Tr as Renamed; }\nuse m::Renamed;\npub struct S;\n\
             impl a::Tr for S { fn go(self) {} }\nimpl S { pub fn new() -> Self { S } pub fn go(&self) {} }\npub fn f() { S::new().go(); }\n\
             pub mod j {\n    pub mod prelude { pub use crate::a::Tr as _; }\n    use prelude::*;\n    pub fn g() { crate::S::new().go() }\n}\n\
             pub mod n { pub fn g() { crate::S::new().go() } }\n\
             pub mod o { pub mod p { pub use crate::a::*; } use p::*; pub fn g() { crate::S::new().go() } }\n\
             pub mod q { pub mod r { pub use crate::m::Renamed as Again; } use r::Again as _; pub fn g() { crate::S::new().go() } }\n\
             pub mod s { use crate::o::p::*; pub struct Tr; pub fn g() { crate::S::new().go() } }\n\
             pub mod t { pub fn g() { use crate::m::*; crate::S::new().go() } }\n",
        ),
    ];
    // rustc 1.99 calls the trait's `go(self)` wherever a binding of any
    // name (`Renamed`, `_`, `Again`, a glob's) brings `Tr` into scope, and
    // the inherent `go(&self)` in `n`, with no binding, and in `s`, whose
    // own `Tr` shadows the one its glob brings.
    let method_calls: Vec<_> = calls(&files)
        .into_iter()
        .filter(|(_, target, _)| target.ends_with(".go"))
        .map(|(src, _, dst)| (src, dst))
        .collect();
    let by_trait = Some("src/lib.rs#<S as Tr>::go#method".to_owned());
    let inherent = Some("src/lib.rs#S::go#method".to_owned());
    assert_eq!(
        method_calls,
        [
            ("f".to_owned(), by_trait.clone()),
            ("g".to_owned(), by_trait.clone()),
            ("g".to_owned(), inherent.clone()),
            ("g".to_owned(), by_trait.clone()),
            ("g".to_owned(), by_trait.clone()),
            ("g".to_owned(), inherent),
            ("g".to_owned(), by_trait),
        ]
    );
}

// A trait outside the workspace was taken as in scope everywhere: in a
// module that binds nothing, `<V as Not>::not(self)` was linked at the
// first step of the probe, where rustc, with `Not` out of scope, runs the
// inherent `not(&self)` at the next.
#[test]
fn c10_rust_a_foreign_trait_is_taken_only_where_it_may_be_in_scope() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub struct V;\nimpl V { pub fn new() -> V { V } }\nimpl V { pub fn not(&self) -> u8 { 0 } }\n\
             impl std::ops::Not for V { type Output = u8; fn not(self) -> u8 { 1 } }\nimpl Clone for V { fn clone(&self) -> V { V } }\n\
             impl std::ops::Neg for V { type Output = u8; fn neg(self) -> u8 { 2 } }\n\
             pub mod u { pub fn c1() { crate::V::new().not(); } pub fn c2() { crate::V::not(&crate::V::new()); } \
             pub fn c4() { crate::V::new().clone(); } }\n\
             pub mod w { use std::ops::{Neg, Not}; pub fn c3() { crate::V::new().not(); } pub fn c5() { crate::V::new().neg(); } }\n",
        ),
    ];
    // What cargo 1.99 runs: the inherent `not` (c1, c2), `<V as Not>::not`
    // (c3, where `use std::ops::Not` brings it), `<V as Clone>::clone` (c4,
    // `Clone` being in every prelude) and `<V as Neg>::neg` (c5). The rules
    // cannot tell which trait a binding outside the workspace brings: there,
    // such a trait's method stands only when nothing else provides the name
    // (c3 abstains, c5 does not).
    let method = |name: &str| Some(format!("src/lib.rs#{name}#method"));
    let got: Vec<(String, Option<String>)> = calls(&files)
        .into_iter()
        .filter(|(_, target, _)| !target.ends_with("::new"))
        .map(|(src, _, dst)| (src, dst))
        .collect();
    assert_eq!(
        got,
        [
            ("c1", method("V::not")),
            ("c2", method("V::not")),
            ("c4", method("<V as Clone>::clone")),
            ("c3", None),
            ("c5", method("<V as Neg>::neg")),
        ]
        .map(|(src, dst)| (src.to_owned(), dst))
    );
}

#[test]
fn c10_rust_std_and_dependency_paths_are_unresolved() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "use std::collections::HashMap;\npub fn read() {}\npub fn f() {\n    std::fs::read(\"x\");\n    serde_json::to_string(&1);\n    Vec::new();\n    Some(1);\n    HashMap::new();\n    read();\n}\n",
        ),
    ];
    // v1 vetoed `std::fs::read()` only because the file defines `read`
    // (R-11); a path through std is unresolved whatever the file defines.
    assert_eq!(
        calls(&files),
        [
            edge("f", "std::fs::read", None),
            edge("f", "serde_json::to_string", None),
            edge("f", "Vec::new", None),
            edge("f", "Some", None),
            edge("f", "HashMap::new", None),
            edge("f", "read", Some("src/lib.rs#read#function")),
        ]
    );
    assert_eq!(
        imports(&files),
        [edge("src/lib.rs", "std::collections::HashMap", None)]
    );
}

#[test]
fn c10_rust_cfg_duplicates_are_unresolved() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "#[cfg(unix)]\nfn sys() {}\n#[cfg(windows)]\nfn sys() {}\npub struct S;\n#[cfg(unix)]\nimpl S {\n    fn os(&self) {}\n}\n#[cfg(windows)]\nimpl S {\n    fn os(&self) {}\n}\n\
             pub fn f(s: &S) {\n    sys();\n    s.os();\n}\n",
        ),
    ];
    assert_eq!(
        calls(&files),
        [edge("f", "sys", None), edge("f", "s.os", None)]
    );
}

#[test]
fn c10_rust_imports_point_at_items_modules_and_files() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub mod store;\nmod inline {\n    pub fn x() {}\n}\nuse store::Store;\nuse inline::x;\nuse self::inline as alias;\nuse store::*;\nuse store::{self};\n",
        ),
        ("src/store.rs", "pub struct Store;\n"),
    ];
    // v1 stored imports as file rows and resolved them to files only (O-2);
    // decision 5 resolves each leaf to what it names.
    assert_eq!(
        imports(&files),
        [
            edge(
                "src/lib.rs",
                "store::Store",
                Some("src/store.rs#Store#struct")
            ),
            edge("src/lib.rs", "inline::x", Some("src/lib.rs#x#function")),
            edge(
                "src/lib.rs",
                "self::inline",
                Some("src/lib.rs#inline#module")
            ),
            edge(
                "src/lib.rs",
                "store::*",
                Some("src/store.rs#src/store.rs#file")
            ),
            edge(
                "src/lib.rs",
                "store::self",
                Some("src/store.rs#src/store.rs#file")
            ),
        ]
    );
}

#[test]
fn c10_rust_imports_an_inline_module_does_not_see_its_parent() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub fn helper() {}\nmod blind {\n    fn f() {\n        helper();\n        super::helper();\n    }\n}\n",
        ),
    ];
    assert_eq!(
        calls(&files),
        [
            edge("f", "helper", None),
            edge("f", "super::helper", Some("src/lib.rs#helper#function")),
        ]
    );
}

#[test]
fn c10_rust_a_generic_parameter_names_no_item() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub struct T;\nimpl T {\n    pub fn new() -> T { T }\n}\npub fn make<T: Default>() -> T { T::default() }\npub fn build() -> T { T::new() }\n",
        ),
    ];
    assert_eq!(
        calls(&files),
        [
            edge("make", "T::default", None),
            edge("build", "T::new", Some("src/lib.rs#T::new#method")),
        ]
    );
}

#[test]
fn c10_rust_a_module_two_crates_compile_resolves_where_they_agree() {
    let files = [
        ("Cargo.toml", APP),
        ("src/lib.rs", "pub mod util;\npub fn origin() {}\n"),
        ("src/main.rs", "mod util;\nfn origin() {}\nfn main() {}\n"),
        (
            "src/util.rs",
            "pub fn helper() {}\npub fn run() {\n    helper();\n    crate::origin();\n}\n",
        ),
    ];
    // `crate::origin` is the library's in one crate, the binary's in the
    // other.
    assert_eq!(
        calls(&files),
        [
            edge("run", "helper", Some("src/util.rs#helper#function")),
            edge("run", "crate::origin", None),
        ]
    );
}

#[test]
fn c10_rust_a_path_attribute_module_is_unresolved() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "#[path = \"imp_unix.rs\"]\nmod imp;\npub fn f() { imp::go(); }\n",
        ),
        ("src/imp_unix.rs", "pub fn go() {}\n"),
        ("src/imp.rs", "pub fn go() {}\n"),
    ];
    // The layout would say `imp.rs`; the attribute says otherwise.
    assert_eq!(calls(&files), [edge("f", "imp::go", None)]);
}

// D38: `#[path]` on an inline module was ignored, so a `mod y;` inside it
// bound the file of the layout directory instead of the one rustc compiles.
#[test]
fn c10_rust_a_path_attribute_on_an_inline_module_is_its_directory() {
    let go = "pub fn go() {}\n";
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "#[path = \"real\"]\npub mod m {\n    pub mod y;\n    pub mod n {\n        pub mod z;\n    }\n    #[path = \"b\"]\n    pub mod inner {\n        pub mod w;\n    }\n}\n\
             pub mod foo;\n#[cfg_attr(unix, path = \"real\")]\npub mod maybe {\n    pub mod y;\n}\n\
             #[r#path = \"other\"]\npub mod raw {\n    pub mod y;\n}\n#[cfg_attr(all(), path /* c */ = \"other\")]\npub mod hidden {\n    pub mod y;\n}\n\
             pub fn all() {\n    m::y::go();\n    m::n::z::go();\n    m::inner::w::go();\n    foo::m::v::go();\n    foo::m2::v::go();\n    maybe::y::go();\n    raw::y::go();\n    hidden::y::go();\n}\n",
        ),
        (
            "src/foo.rs",
            "#[path = \"real\"]\npub mod m {\n    pub mod v;\n}\npub mod m2 {\n    pub mod v;\n}\n",
        ),
        ("src/real/y.rs", go),
        ("src/real/n/z.rs", go),
        ("src/real/b/w.rs", go),
        ("src/real/v.rs", go),
        ("src/other/y.rs", go),
        ("src/foo/m2/v.rs", go),
        // Where the layout alone would look.
        ("src/m/y.rs", go),
        ("src/m/n/z.rs", go),
        ("src/m/inner/w.rs", go),
        ("src/foo/m/v.rs", go),
        ("src/foo/real/v.rs", go),
        ("src/maybe/y.rs", go),
        ("src/raw/y.rs", go),
        ("src/hidden/y.rs", go),
    ];
    // `cargo run` (rustc 1.99) of this tree runs the first five and the
    // seventh from the files below; the attribute inside a non-`mod.rs`
    // file is relative to that file's directory, not to `src/foo`, and
    // `r#path` is `path`. `cfg_attr` may or may not apply the path, a
    // comment in it hiding nothing.
    assert_eq!(
        calls(&files),
        [
            edge("all", "m::y::go", Some("src/real/y.rs#go#function")),
            edge("all", "m::n::z::go", Some("src/real/n/z.rs#go#function")),
            edge(
                "all",
                "m::inner::w::go",
                Some("src/real/b/w.rs#go#function")
            ),
            edge("all", "foo::m::v::go", Some("src/real/v.rs#go#function")),
            edge("all", "foo::m2::v::go", Some("src/foo/m2/v.rs#go#function")),
            edge("all", "maybe::y::go", None),
            edge("all", "raw::y::go", Some("src/other/y.rs#go#function")),
            edge("all", "hidden::y::go", None),
        ]
    );
}

#[test]
fn c10_rust_references_name_functions_constants_and_constructors() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub fn handler() {}\npub const LIMIT: u8 = 1;\npub enum Kind { Plain(u8) }\npub struct S;\nimpl S {\n    pub fn wrap() {}\n    pub fn go(&self) {\n        register(handler);\n        register(Self::wrap);\n        register(LIMIT);\n        register(Kind::Plain);\n        register(other::cb);\n    }\n}\nfn register<T>(_: T) {}\n",
        ),
    ];
    assert_eq!(
        references(&files),
        [
            edge("S::go", "handler", Some("src/lib.rs#handler#function")),
            edge("S::go", "Self::wrap", Some("src/lib.rs#S::wrap#method")),
            edge("S::go", "LIMIT", Some("src/lib.rs#LIMIT#const")),
            edge(
                "S::go",
                "Kind::Plain",
                Some("src/lib.rs#Kind::Plain#variant")
            ),
            edge("S::go", "other::cb", None),
        ]
    );
}

#[test]
fn facts_another_version_wrote_resolve_nothing_and_never_panic() {
    let src = "pub fn helper() {}\nmod inner {\n    pub fn f() { super::helper(); }\n}\n";
    let rows = rows("src/lib.rs", Lang::Rust, src.as_bytes());
    // Records this version cannot read; scopes, items and owners out of
    // range; a context naming a scope the facts do not have.
    let garbage = crate::codec::records(&[
        vec!["scope", "", "file"],
        vec!["scope", "7", "impl", "Ghost", "", "", ""],
        vec!["item", "5", "ghost", "#ghost#function", "pub", "fn"],
        vec!["item", "0", "inner", "", "", "mod", "42", ""],
        vec!["assoc", "9", "f", "#f#method", "fn", "ref", "Self", "1"],
        vec!["use", "3", "", "crate::nowhere", "glob", ""],
        vec!["something", "else"],
    ]);
    let mut edges = rows.edges.clone();
    for edge in &mut edges {
        edge.context = Context {
            scope: 99,
            form: super::super::Form::Path(vec!["super".to_owned(), "helper".to_owned()]),
            cfg: Vec::new(),
        }
        .encode();
    }
    for facts in [garbage.as_str(), "", rows.facts.as_str()] {
        let files = [("src/lib.rs".to_owned(), facts.to_owned())];
        let world = World {
            files: &files,
            symbols: &rows.symbols,
            edges: &edges,
            unread: &[],
        };
        // The index itself, not `resolve`, whose guard would hide a panic.
        let index = super::super::resolve::Index::build(&world);
        let decided: Vec<Option<usize>> = edges.iter().map(|e| index.decide(e)).collect();
        assert_eq!(decided, vec![None; edges.len()], "facts {facts:?}");
    }
}
