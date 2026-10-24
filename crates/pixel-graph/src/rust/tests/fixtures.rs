// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! C-10's hand-labelled edge fixtures for Rust (`design/testing.md`, "Unit
//! and property"): one workspace per class of input, every site labelled
//! by hand with the definition it must reach, or none.
//!
//! Each fixture is extracted and resolved as `link` does it: every file
//! through the driver's `extract::rows`, then `Rules::resolve` over all the
//! rows at once. A label is `(source, kind, line, target)`: the name of the
//! symbol the site belongs to (the file's path at top level), its kind and
//! line, and for a resolved site the definition it reached as `file:line
//! name` (a module file as its path alone), for an unresolved one the
//! target as written. Both lists are asserted whole: a site the rules
//! resolve that the label leaves unresolved is a guessed edge. Line
//! numbers are in the comments beside each fixture's lines.

use pixel_model::{Lang, SymbolKind};

use super::{Rust, rows};
use crate::lang::{Rules, World};

const APP: &str = "[package]\nname = \"app\"\n";

/// `(source, kind, line, target)`: see the module's documentation.
type Label = (String, &'static str, u32, String);

/// The sites of a workspace, resolved and unresolved, each list sorted.
fn decide(files: &[(&str, &str)]) -> (Vec<Label>, Vec<Label>) {
    let mut world_files = Vec::new();
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    for (path, src) in files {
        let rows = rows(path, Lang::Rust, src.as_bytes());
        world_files.push((path.to_string(), rows.facts));
        symbols.extend(rows.symbols);
        edges.extend(rows.edges);
    }
    let decided = Rust.resolve(&World {
        files: &world_files,
        symbols: &symbols,
        edges: &edges,
        unread: &[],
    });
    let (mut resolved, mut unresolved) = (Vec::new(), Vec::new());
    for (edge, dst) in edges.iter().zip(decided) {
        let src = symbols
            .iter()
            .find(|s| s.uid == edge.src)
            .map_or_else(String::new, |s| s.name.clone());
        match dst.map(|i| &symbols[i]) {
            Some(target) if target.kind == SymbolKind::File => {
                resolved.push((src, edge.kind.as_str(), edge.line, target.file.clone()));
            }
            Some(target) => resolved.push((
                src,
                edge.kind.as_str(),
                edge.line,
                format!("{}:{} {}", target.file, target.line, target.name),
            )),
            None => unresolved.push((src, edge.kind.as_str(), edge.line, edge.target.clone())),
        }
    }
    resolved.sort();
    unresolved.sort();
    (resolved, unresolved)
}

fn labels(written: &[(&str, &'static str, u32, &str)]) -> Vec<Label> {
    let mut out: Vec<Label> = written
        .iter()
        .map(|(src, kind, line, target)| (src.to_string(), *kind, *line, target.to_string()))
        .collect();
    out.sort();
    out
}

#[test]
fn c10_rust_aliasing_binds_only_the_alias() {
    let lib = concat!(
        "pub mod store;\n",                         // 1
        "use store::open as connect;\n",            // 2
        "use store::{self as db, Pool as Conn};\n", // 3
        "use store::close as _;\n",                 // 4
        "pub type Handle = store::Pool;\n",         // 5
        "pub fn run() {\n",                         // 6
        "    connect();\n",                         // 7
        "    open();\n",                            // 8
        "    db::close();\n",                       // 9
        "    Conn::new();\n",                       // 10
        "    Handle::new();\n",                     // 11
        "    close();\n",                           // 12
        "}\n",                                      // 13
    );
    let store = concat!(
        "pub fn open() {}\n",                  // 1
        "pub fn close() {}\n",                 // 2
        "pub struct Pool;\n",                  // 3
        "impl Pool {\n",                       // 4
        "    pub fn new() -> Pool { Pool }\n", // 5
        "}\n",                                 // 6
    );
    let files = [
        ("Cargo.toml", APP),
        ("src/lib.rs", lib),
        ("src/store.rs", store),
    ];
    let (resolved, unresolved) = decide(&files);
    assert_eq!(
        resolved,
        labels(&[
            ("src/lib.rs", "import", 2, "src/store.rs:1 open"),
            ("src/lib.rs", "import", 3, "src/store.rs"),
            ("src/lib.rs", "import", 3, "src/store.rs:3 Pool"),
            ("src/lib.rs", "import", 4, "src/store.rs:2 close"),
            ("run", "call", 7, "src/store.rs:1 open"),
            ("run", "call", 9, "src/store.rs:2 close"),
            ("run", "call", 10, "src/store.rs:5 Pool::new"),
            ("run", "call", 11, "src/store.rs:5 Pool::new"),
        ])
    );
    // The alias is the only name `use … as` binds; `as _` binds none.
    assert_eq!(
        unresolved,
        labels(&[("run", "call", 8, "open"), ("run", "call", 12, "close")])
    );
}

#[test]
fn c10_rust_imports_follow_crate_super_and_self() {
    let lib = concat!(
        "pub mod net;\n",        // 1
        "pub fn root_fn() {}\n", // 2
    );
    let net = concat!(
        "pub mod http;\n",                // 1
        "pub fn send() {}\n",             // 2
        "fn local() {}\n",                // 3
        "pub fn go() {\n",                // 4
        "    self::local();\n",           // 5
        "    http::get();\n",             // 6
        "    crate::net::http::get();\n", // 7
        "}\n",                            // 8
    );
    let http = concat!(
        "use super::send;\n",             // 1
        "use crate::root_fn;\n",          // 2
        "pub fn get() {\n",               // 3
        "    send();\n",                  // 4
        "    super::local();\n",          // 5
        "    super::super::root_fn();\n", // 6
        "    root_fn();\n",               // 7
        "    crate::missing();\n",        // 8
        "}\n",                            // 9
    );
    let files = [
        ("Cargo.toml", APP),
        ("src/lib.rs", lib),
        ("src/net.rs", net),
        ("src/net/http.rs", http),
    ];
    let (resolved, unresolved) = decide(&files);
    assert_eq!(
        resolved,
        labels(&[
            ("go", "call", 5, "src/net.rs:3 local"),
            ("go", "call", 6, "src/net/http.rs:3 get"),
            ("go", "call", 7, "src/net/http.rs:3 get"),
            ("src/net/http.rs", "import", 1, "src/net.rs:2 send"),
            ("src/net/http.rs", "import", 2, "src/lib.rs:2 root_fn"),
            ("get", "call", 4, "src/net.rs:2 send"),
            ("get", "call", 5, "src/net.rs:3 local"),
            ("get", "call", 6, "src/lib.rs:2 root_fn"),
            ("get", "call", 7, "src/lib.rs:2 root_fn"),
        ])
    );
    assert_eq!(unresolved, labels(&[("get", "call", 8, "crate::missing")]));
}

#[test]
fn c10_rust_imports_reach_workspace_crates_by_their_cargo_name() {
    let store = concat!(
        "pub fn open() {}\n",                                   // 1
        "pub fn own() { mvp_store::open(); crate::open(); }\n", // 2
    );
    let store_test = concat!(
        "use mvp_store::open;\n",                   // 1
        "fn it() { open(); mvp_store::open(); }\n", // 2
    );
    let app = concat!(
        "use mvp_store as store;\n",        // 1
        "fn main() {\n",                    // 2
        "    store::open();\n",             // 3
        "    shapes::shape();\n",           // 4
        "    model::shape();\n",            // 5
        "    serde_json::to_string(&1);\n", // 6
        "}\n",                              // 7
    );
    let files = [
        ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
        (
            "crates/store/Cargo.toml",
            "[package]\nname = \"mvp-store\"\n",
        ),
        ("crates/store/src/lib.rs", store),
        ("crates/store/tests/it.rs", store_test),
        (
            "crates/model/Cargo.toml",
            "[package]\nname = \"model\"\n[lib]\nname = \"shapes\"\n",
        ),
        ("crates/model/src/lib.rs", "pub fn shape() {}\n"),
        (
            "crates/app/Cargo.toml",
            "[package]\nname = \"app\"\n\n[dependencies]\npixel-store = { path = \"../store\" }\n\
             model = { path = \"../model\" }\nserde_json = \"1\"\n",
        ),
        ("crates/app/src/main.rs", app),
    ];
    let (resolved, unresolved) = decide(&files);
    assert_eq!(
        resolved,
        labels(&[
            ("own", "call", 2, "crates/store/src/lib.rs:1 open"),
            (
                "crates/store/tests/it.rs",
                "import",
                1,
                "crates/store/src/lib.rs:1 open"
            ),
            ("it", "call", 2, "crates/store/src/lib.rs:1 open"),
            ("it", "call", 2, "crates/store/src/lib.rs:1 open"),
            (
                "crates/app/src/main.rs",
                "import",
                1,
                "crates/store/src/lib.rs"
            ),
            ("main", "call", 3, "crates/store/src/lib.rs:1 open"),
            ("main", "call", 4, "crates/model/src/lib.rs:1 shape"),
        ])
    );
    // A library does not see itself by name; `[lib] name` replaces the
    // package name; a dependency is outside the workspace.
    assert_eq!(
        unresolved,
        labels(&[
            ("own", "call", 2, "mvp_store::open"),
            ("main", "call", 5, "model::shape"),
            ("main", "call", 6, "serde_json::to_string"),
        ])
    );
}

#[test]
fn c10_rust_imports_groups_bind_each_leaf() {
    let lib = concat!(
        "pub mod a;\n",                                  // 1
        "use crate::a::{self, b::{c, d as e}, f::*};\n", // 2
        "pub fn g() {\n",                                // 3
        "    a::top();\n",                               // 4
        "    c();\n",                                    // 5
        "    e();\n",                                    // 6
        "    deep();\n",                                 // 7
        "    d();\n",                                    // 8
        "}\n",                                           // 9
    );
    let files = [
        ("Cargo.toml", APP),
        ("src/lib.rs", lib),
        ("src/a.rs", "pub mod b;\npub mod f;\npub fn top() {}\n"),
        ("src/a/b.rs", "pub fn c() {}\npub fn d() {}\n"),
        ("src/a/f.rs", "pub fn deep() {}\n"),
    ];
    let (resolved, unresolved) = decide(&files);
    assert_eq!(
        resolved,
        labels(&[
            ("src/lib.rs", "import", 2, "src/a.rs"),
            ("src/lib.rs", "import", 2, "src/a/b.rs:1 c"),
            ("src/lib.rs", "import", 2, "src/a/b.rs:2 d"),
            ("src/lib.rs", "import", 2, "src/a/f.rs"),
            ("g", "call", 4, "src/a.rs:3 top"),
            ("g", "call", 5, "src/a/b.rs:1 c"),
            ("g", "call", 6, "src/a/b.rs:2 d"),
            ("g", "call", 7, "src/a/f.rs:1 deep"),
        ])
    );
    assert_eq!(unresolved, labels(&[("g", "call", 8, "d")]));
}

#[test]
fn c10_rust_imports_globs_prove_a_single_workspace_provider() {
    let lib = concat!(
        "pub mod a;\n",                                         // 1
        "pub mod b;\n",                                         // 2
        "mod one {\n",                                          // 3
        "    use crate::a::*;\n",                               // 4
        "    fn in_one() { only_a(); hidden(); }\n",            // 5
        "}\n",                                                  // 6
        "mod two {\n",                                          // 7
        "    use crate::a::*;\n",                               // 8
        "    use crate::b::*;\n",                               // 9
        "    fn in_two() { shared(); only_a(); }\n",            // 10
        "}\n",                                                  // 11
        "mod explicit {\n",                                     // 12
        "    use crate::a::*;\n",                               // 13
        "    use crate::b::shared;\n",                          // 14
        "    fn in_explicit() { shared(); }\n",                 // 15
        "}\n",                                                  // 16
        "mod foreign {\n",                                      // 17
        "    use crate::a::*;\n",                               // 18
        "    use rayon::prelude::*;\n",                         // 19
        "    fn in_foreign() { only_a(); }\n",                  // 20
        "}\n",                                                  // 21
        "mod relay {\n",                                        // 22
        "    use crate::a::*;\n",                               // 23
        "    pub use crate::b::*;\n",                           // 24
        "}\n",                                                  // 25
        "fn through() { relay::only_a(); relay::shared(); }\n", // 26
    );
    let files = [
        ("Cargo.toml", APP),
        ("src/lib.rs", lib),
        (
            "src/a.rs",
            "pub fn shared() {}\npub fn only_a() {}\nfn hidden() {}\n",
        ),
        ("src/b.rs", "pub fn shared() {}\n"),
    ];
    let (resolved, unresolved) = decide(&files);
    assert_eq!(
        resolved,
        labels(&[
            ("one", "import", 4, "src/a.rs"),
            ("in_one", "call", 5, "src/a.rs:2 only_a"),
            ("two", "import", 8, "src/a.rs"),
            ("two", "import", 9, "src/b.rs"),
            ("in_two", "call", 10, "src/a.rs:2 only_a"),
            ("explicit", "import", 13, "src/a.rs"),
            ("explicit", "import", 14, "src/b.rs:1 shared"),
            ("in_explicit", "call", 15, "src/b.rs:1 shared"),
            ("foreign", "import", 18, "src/a.rs"),
            ("relay", "import", 23, "src/a.rs"),
            ("relay", "import", 24, "src/b.rs"),
            ("through", "call", 26, "src/b.rs:1 shared"),
        ])
    );
    // A private item is no glob's; two globs providing a name prove
    // neither; a dependency's glob could provide any name; a private glob
    // passes nothing on to another module.
    assert_eq!(
        unresolved,
        labels(&[
            ("in_one", "call", 5, "hidden"),
            ("in_two", "call", 10, "shared"),
            ("foreign", "import", 19, "rayon::prelude::*"),
            ("in_foreign", "call", 20, "only_a"),
            ("through", "call", 26, "relay::only_a"),
        ])
    );
}

#[test]
fn c10_rust_shadowing_locals_hide_the_function() {
    let lib = concat!(
        "pub fn run() {}\n",                                     // 1
        "pub fn closure() {\n",                                  // 2
        "    let run = || 1;\n",                                 // 3
        "    run();\n",                                          // 4
        "}\n",                                                   // 5
        "pub fn parameter(run: fn()) {\n",                       // 6
        "    run();\n",                                          // 7
        "}\n",                                                   // 8
        "pub fn patterns(v: Vec<fn()>, o: Option<fn()>) {\n",    // 9
        "    for run in v { run(); }\n",                         // 10
        "    if let Some(run) = o { run(); } else { run(); }\n", // 11
        "    match o { Some(run) => run(), None => run() }\n",   // 12
        "}\n",                                                   // 13
        "pub fn later() {\n",                                    // 14
        "    run();\n",                                          // 15
        "    let run = 2;\n",                                    // 16
        "}\n",                                                   // 17
        "pub fn initializer() {\n",                              // 18
        "    let run = run();\n",                                // 19
        "}\n",                                                   // 20
    );
    let (resolved, unresolved) = decide(&[("Cargo.toml", APP), ("src/lib.rs", lib)]);
    // A binding is in scope after its `let`, in its own branch, arm or
    // body only.
    assert_eq!(
        resolved,
        labels(&[
            ("patterns", "call", 11, "src/lib.rs:1 run"),
            ("patterns", "call", 12, "src/lib.rs:1 run"),
            ("later", "call", 15, "src/lib.rs:1 run"),
            ("initializer", "call", 19, "src/lib.rs:1 run"),
        ])
    );
    assert_eq!(
        unresolved,
        labels(&[
            ("closure", "call", 4, "run"),
            ("parameter", "call", 7, "run"),
            ("patterns", "call", 10, "run"),
            ("patterns", "call", 11, "run"),
            ("patterns", "call", 12, "run"),
        ])
    );
}

#[test]
fn c10_rust_shadowing_block_items_and_uses_win_inside_the_block() {
    let lib = concat!(
        "pub mod util;\n",                             // 1
        "use crate::util::early as step;\n",           // 2
        "pub fn outer() { step(); }\n",                // 3
        "pub fn block_use() {\n",                      // 4
        "    step();\n",                               // 5
        "    use crate::util::late as step;\n",        // 6
        "}\n",                                         // 7
        "pub fn block_item() {\n",                     // 8
        "    step();\n",                               // 9
        "    fn step() {}\n",                          // 10
        "    fn nested() { step(); }\n",               // 11
        "}\n",                                         // 12
        "mod inner {\n",                               // 13
        "    fn blind() { step(); super::step(); }\n", // 14
        "}\n",                                         // 15
    );
    let files = [
        ("Cargo.toml", APP),
        ("src/lib.rs", lib),
        ("src/util.rs", "pub fn early() {}\npub fn late() {}\n"),
    ];
    let (resolved, unresolved) = decide(&files);
    // A block's items and `use`s hold in the whole block, before their
    // line too, and in the items nested in it.
    assert_eq!(
        resolved,
        labels(&[
            ("src/lib.rs", "import", 2, "src/util.rs:1 early"),
            ("outer", "call", 3, "src/util.rs:1 early"),
            ("block_use", "call", 5, "src/util.rs:2 late"),
            ("block_use", "import", 6, "src/util.rs:2 late"),
            ("block_item", "call", 9, "src/lib.rs:10 step"),
            ("nested", "call", 11, "src/lib.rs:10 step"),
            ("blind", "call", 14, "src/util.rs:1 early"),
        ])
    );
    // An inline module does not see its parent's names.
    assert_eq!(unresolved, labels(&[("blind", "call", 14, "step")]));
}

#[test]
fn c10_rust_dynamic_dispatch_reaches_the_trait_method_only() {
    let lib = concat!(
        "pub trait Shape {\n",                            // 1
        "    fn area(&self) -> u8;\n",                    // 2
        "    fn describe(&self) -> u8 { self.area() }\n", // 3
        "}\n",                                            // 4
        "pub trait Named: Shape {\n",                     // 5
        "    fn name(&self) -> u8;\n",                    // 6
        "}\n",                                            // 7
        "pub fn objects(s: &dyn Shape, b: Box<dyn Shape>, i: impl Shape, n: &dyn Named) {\n", // 8
        "    s.area();\n",                                // 9
        "    b.area();\n",                                // 10
        "    i.describe();\n",                            // 11
        "    n.name();\n",                                // 12
        "    n.area();\n",                                // 13
        "}\n",                                            // 14
        "pub fn generic<T: Shape>(t: &T) {\n",            // 15
        "    t.area();\n",                                // 16
        "    T::area(t);\n",                              // 17
        "    <T as Shape>::area(t);\n",                   // 18
        "    Shape::area(t);\n",                          // 19
        "}\n",                                            // 20
    );
    let (resolved, unresolved) = decide(&[("Cargo.toml", APP), ("src/lib.rs", lib)]);
    // The implementation a dynamic call runs is unknown; the trait method
    // it calls is what Rust's resolution proves.
    assert_eq!(
        resolved,
        labels(&[
            ("Shape::describe", "call", 3, "src/lib.rs:2 Shape::area"),
            ("objects", "call", 9, "src/lib.rs:2 Shape::area"),
            ("objects", "call", 10, "src/lib.rs:2 Shape::area"),
            ("objects", "call", 11, "src/lib.rs:3 Shape::describe"),
            ("objects", "call", 12, "src/lib.rs:6 Named::name"),
            ("generic", "call", 18, "src/lib.rs:2 Shape::area"),
            ("generic", "call", 19, "src/lib.rs:2 Shape::area"),
        ])
    );
    // A supertrait's method, a generic bound and a path rooted at a
    // generic parameter are not read.
    assert_eq!(
        unresolved,
        labels(&[
            ("objects", "call", 13, "n.area"),
            ("generic", "call", 16, "t.area"),
            ("generic", "call", 17, "T::area"),
        ])
    );
}

#[test]
fn c10_rust_methods_follow_the_probe_order_and_the_traits_in_scope() {
    let lib = concat!(
        "pub mod ops {\n",                                                     // 1
        "    pub trait Run { fn run(&self) -> u8 { 0 } fn edit(&self) {} }\n", // 2
        "}\n",                                                                 // 3
        "use ops::Run;\n",                                                     // 4
        "pub struct S;\n",                                                     // 5
        "impl S {\n",                                                          // 6
        "    pub fn new() -> S { S }\n",                                       // 7
        "    pub fn run(&self) -> u8 { 1 }\n",                                 // 8
        "    pub fn edit(&mut self) {}\n",                                     // 9
        "    fn go(&self) { self.run(); Self::new(); }\n",                     // 10
        "}\n",                                                                 // 11
        "impl Run for S {}\n",                                                 // 12
        "pub struct T;\n",                                                     // 13
        "impl Run for T {}\n",                                                 // 14
        "pub fn calls(s: &S, t: &T) {\n",                                      // 15
        "    s.run();\n",                                                      // 16
        "    s.edit();\n",                                                     // 17
        "    t.run();\n",                                                      // 18
        "    <S as Run>::run(s);\n",                                           // 19
        "    S::new();\n",                                                     // 20
        "}\n",                                                                 // 21
        "mod elsewhere {\n",                                                   // 22
        "    fn blind(t: &crate::T) { t.run(); }\n",                           // 23
        "}\n",                                                                 // 24
        "pub struct U;\n",                                                     // 25
        "impl U { pub fn step(&mut self) {} pub fn look(&self) {} }\n",        // 26
        "pub trait Walk { fn step(self); }\n",                                 // 27
        "impl Walk for &U { fn step(self) {} }\n",                             // 28
        "pub fn walk(u: &mut U) { u.step(); u.look(); }\n",                    // 29
    );
    let (resolved, unresolved) = decide(&[("Cargo.toml", APP), ("src/lib.rs", lib)]);
    // An inherent method beats a trait method taking `self` the same way;
    // `S`'s impl of `Run` keeps the trait's default.
    assert_eq!(
        resolved,
        labels(&[
            ("src/lib.rs", "import", 4, "src/lib.rs:2 Run"),
            ("S::go", "call", 10, "src/lib.rs:8 S::run"),
            ("S::go", "call", 10, "src/lib.rs:7 S::new"),
            ("calls", "call", 16, "src/lib.rs:8 S::run"),
            ("calls", "call", 18, "src/lib.rs:2 Run::run"),
            ("calls", "call", 19, "src/lib.rs:2 Run::run"),
            ("calls", "call", 20, "src/lib.rs:7 S::new"),
            ("walk", "call", 29, "src/lib.rs:26 U::look"),
        ])
    );
    // `Run::edit(&self)` comes before `S::edit(&mut self)` for a value or
    // a shared reference, after it for a mutable one: the receiver's
    // reference is not read. Where `Run` is not in scope, its methods are
    // no candidates. `Walk::step(self)` on `&U` fits before `U::step(&mut
    // self)` for a value or a shared reference.
    assert_eq!(
        unresolved,
        labels(&[
            ("calls", "call", 17, "s.edit"),
            ("blind", "call", 23, "t.run"),
            ("walk", "call", 29, "u.step"),
        ])
    );
}

#[test]
fn c10_rust_typed_receivers_resolve_through_the_stated_type() {
    let lib = concat!(
        "use std::rc::Rc;\n",                                   // 1
        "pub struct Store;\n",                                  // 2
        "impl Store {\n",                                       // 3
        "    pub fn new() -> Store { Store }\n",                // 4
        "    pub fn open(&self) {}\n",                          // 5
        "    pub fn clone(&self) -> Store { Store }\n",         // 6
        "}\n",                                                  // 7
        "pub fn typed(param: &Store, shared: Rc<Store>) {\n",   // 8
        "    param.open();\n",                                  // 9
        "    shared.open();\n",                                 // 10
        "    shared.clone();\n",                                // 11
        "    let made = Store::new();\n",                       // 12
        "    made.open();\n",                                   // 13
        "    let literal = Store {};\n",                        // 14
        "    literal.open();\n",                                // 15
        "    Store::new().open();\n",                           // 16
        "    let boxed: Box<Store> = Box::new(Store);\n",       // 17
        "    boxed.open();\n",                                  // 18
        "}\n",                                                  // 19
        "pub fn untyped(v: Vec<Store>) {\n",                    // 20
        "    let first = make();\n",                            // 21
        "    first.open();\n",                                  // 22
        "    v[0].open();\n",                                   // 23
        "}\n",                                                  // 24
        "fn make() -> Store { Store }\n",                       // 25
        "pub trait Describe { fn describe(&self) {} }\n",       // 26
        "impl<T> Describe for T {}\n",                          // 27
        "impl Store { pub fn describe(&self) {} }\n",           // 28
        "pub fn blanket(param: &Store, shared: Rc<Store>) {\n", // 29
        "    Store {}.describe();\n",                           // 30
        "    param.describe();\n",                              // 31
        "    shared.describe();\n",                             // 32
        "}\n",                                                  // 33
    );
    let (resolved, unresolved) = decide(&[("Cargo.toml", APP), ("src/lib.rs", lib)]);
    // References and `Box`/`Rc`/`Arc` unwrap to the type they hold.
    assert_eq!(
        resolved,
        labels(&[
            ("typed", "call", 9, "src/lib.rs:5 Store::open"),
            ("typed", "call", 10, "src/lib.rs:5 Store::open"),
            ("typed", "call", 12, "src/lib.rs:4 Store::new"),
            ("typed", "call", 13, "src/lib.rs:5 Store::open"),
            ("typed", "call", 15, "src/lib.rs:5 Store::open"),
            ("typed", "call", 16, "src/lib.rs:4 Store::new"),
            ("typed", "call", 16, "src/lib.rs:5 Store::open"),
            ("typed", "reference", 17, "src/lib.rs:2 Store"),
            ("typed", "call", 18, "src/lib.rs:5 Store::open"),
            ("untyped", "call", 21, "src/lib.rs:25 make"),
            ("blanket", "call", 30, "src/lib.rs:28 Store::describe"),
        ])
    );
    // `Rc`'s own `clone` comes before the pointee's; a value whose type
    // the code does not state is not typed. A blanket impl fits `Rc<Store>`
    // before `Store`, and `&&mut Store` before `&Store`: the inherent
    // method wins only for a value known to be one.
    assert_eq!(
        unresolved,
        labels(&[
            ("src/lib.rs", "import", 1, "std::rc::Rc"),
            ("typed", "call", 11, "shared.clone"),
            ("typed", "call", 17, "Box::new"),
            ("untyped", "call", 22, "first.open"),
            ("untyped", "call", 23, "v[0].open"),
            ("blanket", "call", 31, "param.describe"),
            ("blanket", "call", 32, "shared.describe"),
        ])
    );
}

#[test]
fn c10_rust_test_modules_see_their_parent_through_use_super() {
    let lib = concat!(
        "mod util;\n",                                    // 1
        "fn helper() -> u8 { 1 }\n",                      // 2
        "pub fn api() -> u8 { helper() }\n",              // 3
        "#[cfg(test)]\n",                                 // 4
        "mod tests {\n",                                  // 5
        "    use super::*;\n",                            // 6
        "    fn fixture() -> u8 { 2 }\n",                 // 7
        "    #[test]\n",                                  // 8
        "    fn checks() {\n",                            // 9
        "        helper();\n",                            // 10
        "        api();\n",                               // 11
        "        fixture();\n",                           // 12
        "        util::deep();\n",                        // 13
        "    }\n",                                        // 14
        "    mod nested {\n",                             // 15
        "        use super::*;\n",                        // 16
        "        #[test]\n",                              // 17
        "        fn deeper() { fixture(); helper(); }\n", // 18
        "    }\n",                                        // 19
        "}\n",                                            // 20
        "#[cfg(test)]\n",                                 // 21
        "mod props {\n",                                  // 22
        "    use super::*;\n",                            // 23
        "    use proptest::prelude::*;\n",                // 24
        "    #[test]\n",                                  // 25
        "    fn mixed() { helper(); }\n",                 // 26
        "}\n",                                            // 27
    );
    let files = [
        ("Cargo.toml", APP),
        ("src/lib.rs", lib),
        ("src/util.rs", "pub fn deep() {}\n"),
    ];
    let (resolved, unresolved) = decide(&files);
    // Test code is indexed like any code; a glob of the parent brings its
    // private items, its `mod`s and what it imports itself.
    assert_eq!(
        resolved,
        labels(&[
            ("api", "call", 3, "src/lib.rs:2 helper"),
            ("tests", "import", 6, "src/lib.rs"),
            ("checks", "call", 10, "src/lib.rs:2 helper"),
            ("checks", "call", 11, "src/lib.rs:3 api"),
            ("checks", "call", 12, "src/lib.rs:7 fixture"),
            ("checks", "call", 13, "src/util.rs:1 deep"),
            ("nested", "import", 16, "src/lib.rs:5 tests"),
            ("deeper", "call", 18, "src/lib.rs:7 fixture"),
            ("deeper", "call", 18, "src/lib.rs:2 helper"),
            ("props", "import", 23, "src/lib.rs"),
        ])
    );
    // Beside a dependency's glob, nothing a glob provides is proven.
    assert_eq!(
        unresolved,
        labels(&[
            ("props", "import", 24, "proptest::prelude::*"),
            ("mixed", "call", 26, "helper"),
        ])
    );
}

#[test]
fn c10_rust_misread_code_and_item_macros_prove_nothing() {
    let lib = concat!(
        "pub fn run() {}\n",                           // 1
        "pub fn fine() { run(); }\n",                  // 2
        "mod cached {\n",                              // 3
        "    use super::*;\n",                         // 4
        "    thread_local! { static RUN: u8 = 0; }\n", // 5
        "    fn uses() { run(); keep(RUN); }\n",       // 6
        "}\n",                                         // 7
        "fn keep(_: u8) {}\n",                         // 8
        "const RUN: u8 = 1;\n",                        // 9
    );
    let broken = concat!(
        "pub fn run() {}\n",   // 1
        "pub fn broken() {\n", // 2
        "    run();\n",        // 3
        "    let x = ;\n",     // 4
        "}\n",                 // 5
    );
    let generated = concat!(
        "pub fn run() {}\n",                           // 1
        "mod generated {\n",                           // 2
        "    use super::*;\n",                         // 3
        "    make_items!();\n",                        // 4
        "    fn uses() { run(); }\n",                  // 5
        "}\n",                                         // 6
        "mod cached {\n",                              // 7
        "    use super::*;\n",                         // 8
        "    thread_local! { static RUN: u8 = 0; }\n", // 9
        "    fn uses() { run(); }\n",                  // 10
        "}\n",                                         // 11
    );
    let (resolved, unresolved) = decide(&[
        ("Cargo.toml", APP),
        ("src/lib.rs", lib),
        ("crates/bad/Cargo.toml", "[package]\nname = \"bad\"\n"),
        ("crates/bad/src/lib.rs", broken),
        ("crates/gen/Cargo.toml", "[package]\nname = \"gen\"\n"),
        ("crates/gen/src/lib.rs", generated),
    ]);
    // `thread_local!` defines the statics it names, nothing else: its `RUN`
    // shadows the parent's.
    assert_eq!(
        resolved,
        labels(&[
            ("fine", "call", 2, "src/lib.rs:1 run"),
            ("cached", "import", 4, "src/lib.rs"),
            ("uses", "call", 6, "src/lib.rs:1 run"),
            ("uses", "call", 6, "src/lib.rs:8 keep"),
            ("generated", "import", 3, "crates/gen/src/lib.rs"),
            ("cached", "import", 8, "crates/gen/src/lib.rs"),
        ])
    );
    // A function the grammar misread may hide a binding; a macro among a
    // module's items may define the name a glob would provide, and, beside
    // it in a crate, a macro named `thread_local` (D23), which `cached`'s
    // call may then be.
    assert_eq!(
        unresolved,
        labels(&[
            ("broken", "call", 3, "run"),
            ("uses", "reference", 6, "RUN"),
            ("uses", "call", 5, "run"),
            ("uses", "call", 10, "run"),
        ])
    );
}
