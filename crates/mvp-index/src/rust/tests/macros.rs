// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Macros invoked where items go (C-10, D23): what they may define and the
//! impls they may write, which the rows do not hold. Each expectation is
//! what rustc or cargo 1.99 runs, or `None` where a macro may change the
//! answer: a method probe in a crate compiling a macro invoked where items
//! go, whatever the macro's tokens, or on a type carrying a derive that is
//! not std's.

use super::*;

const APP: &str = "[package]\nname = \"app\"\n";

// D23: a macro invoked among a module's items may expand to an impl, so
// the one method the rules saw was linked where rustc runs the macro's.
#[test]
fn c10_rust_a_macro_among_items_may_add_the_methods_it_names() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub mod t6b;\npub mod plain;\npub struct W<T>(pub T);\nimpl W<u8> { pub fn get(&self) -> u8 { 0 } pub fn put(&self) {} }\n\
             mod made {\n    use super::W;\n    macro_rules! gen { ($t:ty) => { impl W<$t> { pub fn get(&self) -> u8 { 1 } } }; }\n    gen!(u16);\n}\n\
             pub fn f(w: &W<u16>, v: &W<u8>) { w.get(); W::<u16>::get(w); v.put(); }\n",
        ),
        (
            "src/t6b.rs",
            "pub trait Tr { fn get(&self); fn other(&self); }\npub struct V;\nimpl Tr for V { fn get(&self) {} fn other(&self) {} }\n\
             macro_rules! gen { ($name:ident) => { impl V { pub fn $name(self) {} } }; }\ngen!(get);\npub fn f(v: V) { v.other(); v.get(); }\n",
        ),
        (
            "src/plain.rs",
            "pub struct P;\nimpl P { pub fn run(&self) {} }\nmacro_rules! named { ($n:ident) => { pub fn $n() {} }; }\nnamed!(helper);\npub fn f(p: &P) { p.run(); }\n",
        ),
        (
            "crates/dep/Cargo.toml",
            "[package]\nname = \"dep\"\n[dependencies]\nrayon = \"1\"\n",
        ),
        (
            "crates/dep/src/lib.rs",
            "use rayon::prelude::*;\nuse std::collections::HashMap;\npub trait Shown { fn show(&self); }\npub trait Go { fn go(self); }\npub struct D;\nimpl D { pub fn run(&self) {} pub fn go(&self) {} }\n\
             impl Shown for D { fn show(&self) {} }\npub fn f(d: &D) { d.run(); d.show(); d.go(); }\n#[cfg(test)]\nmod tests {\n    proptest::proptest! {}\n}\n",
        ),
        (
            "crates/user/Cargo.toml",
            "[package]\nname = \"user\"\n[dependencies]\ndep = { path = \"../dep\" }\n",
        ),
        (
            "crates/user/src/lib.rs",
            "use dep::Shown;\npub fn f(d: &dep::D) { d.run(); d.show(); }\n",
        ),
        (
            "crates/globbed/Cargo.toml",
            "[package]\nname = \"globbed\"\n[dependencies]\nrayon = \"1\"\n",
        ),
        (
            "crates/globbed/src/lib.rs",
            "use rayon::prelude::*;\nuse std::collections::HashMap;\npub mod t {\n    pub trait Go { fn go(self); }\n    \
             impl Go for super::G { fn go(self) {} }\n}\npub struct G;\nimpl G { pub fn go(&self) {} }\npub fn f(g: G) { g.go(); }\n",
        ),
    ];
    // rustc 1.99 runs the macros' `get` twice and `V::get` once; `W::put`,
    // `<V as Tr>::other` and `P::run`, which no macro of the crate writes,
    // are unresolved too: the rules do not read what a macro writes, and
    // any macro invoked where items go leaves every probe of its crate
    // unresolved. A macro under `cfg(test)` (`proptest!`) counts only in
    // its own crate, never in one compiling it as a dependency, where
    // `test` never holds: `user` runs `D::run` and `<D as Shown>::show`.
    // Without a macro, `std` beside a dependency's glob is std's (rustc
    // rejects a `use` both could name, E0659), so no binding the rules
    // cannot read brings `t::Go` beside `G::go`.
    assert_eq!(
        calls(&files),
        [
            edge("f", "w.get", None),
            edge("f", "W::<u16>::get", None),
            edge("f", "v.put", None),
            edge("f", "v.other", None),
            edge("f", "v.get", None),
            edge("f", "p.run", None),
            edge("f", "d.run", None),
            edge("f", "d.show", None),
            edge("f", "d.go", None),
            edge("f", "d.run", Some("crates/dep/src/lib.rs#D::run#method")),
            edge(
                "f",
                "d.show",
                Some("crates/dep/src/lib.rs#<D as Shown>::show#method")
            ),
            edge("f", "g.go", Some("crates/globbed/src/lib.rs#G::go#method")),
        ]
    );
}

// D23 (review of the fix): a trait a macro defines in one module, and
// implements for a type, was out of sight of every other module: the mark
// a macro leaves was private, so a `use`, a glob or a re-export naming the
// trait read as binding nothing, and the inherent method was linked.
#[test]
fn c10_rust_what_a_macro_defines_is_seen_from_every_module() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub struct V;\nimpl V { pub fn new() -> V { V } pub fn get(&self) {} }\n\
             macro_rules! mk_trait {\n    ($t:ident, $n:ident) => {\n        pub trait Hidden { fn $n(self); }\n        \
             impl Hidden for $t { fn $n(self) {} }\n        pub fn helper() {}\n    };\n}\npub fn helper() {}\n\
             pub mod m { use crate::V; mk_trait!(V, get); }\n\
             pub mod c1 { use crate::m::Hidden; pub fn f() { crate::V::new().get(); } }\n\
             pub mod c2 { use crate::m::Hidden as _; pub fn f() { crate::V::new().get(); } }\n\
             pub mod c3 { use crate::m::{Hidden}; pub fn f(v: crate::V) { v.get(); } }\n\
             pub mod c4 { pub fn f() { crate::V::new().get(); } }\n\
             pub mod c7 { use crate::m::*; pub fn f() { crate::V::new().get(); } }\n\
             pub mod r { pub use crate::m::Hidden as Again; }\n\
             pub mod c8 { use crate::r::Again; pub fn f() { crate::V::new().get(); } }\n\
             pub fn g() { use crate::m::Hidden as _; V::new().get(); }\n\
             pub fn h() { use crate::m::*; helper(); crate::m::helper(); }\npub fn k() { helper(); }\n",
        ),
    ];
    // rustc 1.99 runs the macro's `<V as Hidden>::get` wherever a binding
    // brings `Hidden`, the inherent `get` in `c4` alone (unresolved: the
    // crate invokes a macro), the macro's `helper` where a block's glob
    // brings it before the crate root's, and the root's in `k`.
    assert_eq!(
        calls(&files)
            .into_iter()
            .filter(|(_, target, _)| !target.ends_with("::new"))
            .collect::<Vec<_>>(),
        [
            edge("f", "crate::V::new().get", None),
            edge("f", "crate::V::new().get", None),
            edge("f", "v.get", None),
            edge("f", "crate::V::new().get", None),
            edge("f", "crate::V::new().get", None),
            edge("f", "crate::V::new().get", None),
            edge("g", "V::new().get", None),
            edge("h", "helper", None),
            edge("h", "crate::m::helper", None),
            edge("k", "helper", Some("src/lib.rs#helper#function")),
        ]
    );
}

// D23 (review of the fix): a macro's trait impl gives its type every item
// of the trait, defaults included, whose names its tokens need not hold;
// any crate may write one for a type it can name; and the trait whose
// method then runs may be the prelude's or a dependency's, which no row
// shows in scope. A macro whose rules hold no `impl` may pass one through
// from its input, and the words past the cap went unread. Each call was
// linked to the method rustc does not run. A macro's impl reaches the
// crates compiling the crate invoking it, not the crates it compiles.
#[test]
fn c10_rust_a_macro_s_impl_reaches_the_crates_compiling_its_crate() {
    let pad: Vec<String> = (0..1100).map(|i| format!("w{i}: u8")).collect();
    let long = format!(
        "pub struct V;\nimpl V {{ pub fn new() -> V {{ V }} }}\npub trait Tr {{ fn get(&self) -> u8 {{ 2 }} }}\nimpl Tr for V {{}}\n\
         macro_rules! big {{ ($t:ident) => {{ const _: () = {{ struct Pad {{ {} }} }}; impl $t {{ pub fn get(&self) -> u8 {{ 1 }} }} }}; }}\n\
         big!(V);\nmod c {{ use crate::Tr; pub fn f() -> u8 {{ crate::V::new().get() }} }}\nfn main() {{ println!(\"long {{}}\", c::f()); }}\n",
        pad.join(", ")
    );
    let files = [
        (
            "crates/iter/Cargo.toml",
            "[package]\n\
             name = \"iter\"\n",
        ),
        (
            "crates/iter/src/main.rs",
            "pub struct V;\n\
             impl V { pub fn new() -> V { V } pub fn count(&self) -> usize { 7 } }\n\
             macro_rules! mk { ($t:ident) => { impl Iterator for $t { type Item = u8; fn next(&mut self) -> Option<u8> { None } } }; }\n\
             mk!(V);\n\
             mod c { pub fn f() -> usize { crate::V::new().count() } }\n\
             fn main() { println!(\"iter {}\", c::f()); }\n",
        ),
        (
            "crates/not/Cargo.toml",
            "[package]\n\
             name = \"not\"\n",
        ),
        (
            "crates/not/src/main.rs",
            "pub struct V;\n\
             impl V { pub fn new() -> V { V } pub fn not(&self) -> u8 { 1 } }\n\
             macro_rules! mk { ($t:ident) => { impl std::ops::Not for $t { type Output = u8; fn not(self) -> u8 { 2 } } }; }\n\
             mk!(V);\n\
             mod c { use std::ops::Not; pub fn f() -> u8 { crate::V::new().not() } }\n\
             mod d { pub fn f() -> u8 { crate::V::new().not() } }\n\
             mod e { pub struct X; impl std::ops::Not for X { type Output = u8; fn not(self) -> u8 { crate::V::new().not() } } }\n\
             fn main() { println!(\"not {} {} {}\", c::f(), d::f(), !e::X); }\n",
        ),
        (
            "crates/dflt/Cargo.toml",
            "[package]\n\
             name = \"dflt\"\n",
        ),
        (
            "crates/dflt/src/main.rs",
            "pub trait Tr { fn get(self) -> u8 where Self: Sized { 2 } }\n\
             pub struct V;\n\
             impl V { pub fn new() -> V { V } pub fn get(&self) -> u8 { 1 } }\n\
             macro_rules! mk { ($t:ident) => { impl Tr for $t {} }; }\n\
             mk!(V);\n\
             mod c { pub fn f() -> u8 { crate::V::new().get() } }\n\
             mod e { use crate::Tr; pub fn f() -> u8 { crate::V::new().get() } }\n\
             fn main() { println!(\"dflt {} {}\", c::f(), e::f()); }\n",
        ),
        (
            "crates/base/Cargo.toml",
            "[package]\n\
             name = \"base\"\n",
        ),
        (
            "crates/base/src/lib.rs",
            "pub struct V;\n\
             impl V { pub fn new() -> V { V } pub fn get(&self) -> u8 { 1 } }\n\
             pub fn b() -> u8 { V::new().get() }\n",
        ),
        (
            "crates/user/Cargo.toml",
            "[package]\n\
             name = \"user\"\n\
             \n\
             [dependencies]\n\
             base = { path = \"../base\" }\n",
        ),
        (
            "crates/user/src/main.rs",
            "pub trait Tr { fn get(self) -> u8; }\n\
             macro_rules! mk { ($t:ty) => { impl Tr for $t { fn get(self) -> u8 { 2 } } }; }\n\
             mk!(base::V);\n\
             mod c { use crate::Tr; pub fn f() -> u8 { base::V::new().get() } }\n\
             mod e { pub fn f() -> u8 { base::V::new().get() } }\n\
             fn main() { println!(\"user {} {}\", c::f(), e::f()); }\n",
        ),
        (
            "crates/input/Cargo.toml",
            "[package]\n\
             name = \"input\"\n",
        ),
        (
            "crates/input/src/main.rs",
            "pub struct V;\n\
             impl V { pub fn new() -> V { V } }\n\
             pub trait Tr { fn get(&self) -> u8 { 2 } }\n\
             impl Tr for V {}\n\
             macro_rules! wrap { ($($i:item)*) => { $($i)* }; }\n\
             wrap! { impl V { pub fn get(&self) -> u8 { 1 } } }\n\
             mod c { use crate::Tr; pub fn f() -> u8 { crate::V::new().get() } }\n\
             fn main() { println!(\"input {}\", c::f()); }\n",
        ),
        (
            "crates/long/Cargo.toml",
            "[package]\n\
             name = \"long\"\n",
        ),
        ("crates/long/src/main.rs", long.as_str()),
    ];
    // cargo 1.99 printed `iter 0`, `not 2 1 2`, `dflt 1 2`, `user 2 1`,
    // `input 1` and `long 1`: `Iterator::count`, the macro's `Not::not`
    // where `use std::ops::Not` brings it or an impl of `Not` holds the
    // call, `Tr::get`'s default where `Tr` is in scope, the other crate's
    // macro's `<V as Tr>::get`, and the inherent `get` the input and the
    // long macro write. Every probe of a crate invoking a macro is
    // unresolved, those where the inherent method runs too (`d`, `c` of
    // `dflt`, `e` of `user`); `base`, which `user` compiles but which
    // compiles no macro, runs and links `V::get`.
    assert_eq!(
        calls(&files)
            .into_iter()
            .filter(|(_, target, _)| !target.ends_with("::new"))
            .collect::<Vec<_>>(),
        [
            edge("f", "crate::V::new().count", None),
            edge("f", "crate::V::new().not", None),
            edge("f", "crate::V::new().not", None),
            edge("<X as Not>::not", "crate::V::new().not", None),
            edge("f", "crate::V::new().get", None),
            edge("f", "crate::V::new().get", None),
            edge(
                "b",
                "V::new().get",
                Some("crates/base/src/lib.rs#V::get#method")
            ),
            edge("f", "base::V::new().get", None),
            edge("f", "base::V::new().get", None),
            edge("f", "crate::V::new().get", None),
            edge("f", "crate::V::new().get", None),
        ]
    );
}

// D23 (review of the fix): the word lexer read `'\"'` as a quote opening a
// string, which swallowed the `impl` after it in a macro's rules or input:
// the macro was taken to write no impl, and the trait method was linked.
// No macro's tokens are read now: every macro invoked where items go
// counts.
#[test]
fn c10_rust_a_macro_counts_whatever_its_literals_hide() {
    let head = "pub struct V;\nimpl V { pub fn new() -> V { V } }\npub trait Tr { fn get(&self); }\nimpl Tr for V { fn get(&self) {} }\n";
    let rules = format!(
        "{head}macro_rules! mk {{ ($t:ident) => {{ const _Q: char = '\\\"'; impl $t {{ pub fn get(self) {{}} }} }}; }}\n\
         mk!(V);\npub fn c1() {{ V::new().get(); }}\n"
    );
    let input = format!(
        "{head}macro_rules! pass {{ ($($i:item)*) => {{ $($i)* }}; }}\n\
         pass! {{ const _Q: char = '\\\"'; impl V {{ pub fn get(self) {{}} }} }}\npub fn c2() {{ V::new().get(); }}\n"
    );
    let files = [
        ("crates/rules/Cargo.toml", "[package]\nname = \"rules\"\n"),
        ("crates/rules/src/lib.rs", rules.as_str()),
        ("crates/input/Cargo.toml", "[package]\nname = \"input\"\n"),
        ("crates/input/src/lib.rs", input.as_str()),
    ];
    // rustc 1.99 runs the macro's `get(self)` in both, before
    // `<V as Tr>::get(&self)`.
    assert_eq!(
        calls(&files)
            .into_iter()
            .filter(|(_, target, _)| target.ends_with(".get"))
            .collect::<Vec<_>>(),
        [
            edge("c1", "V::new().get", None),
            edge("c2", "V::new().get", None),
        ]
    );
}

// D23 (review of the fix): a macro's name was looked up by path alone, as
// if every `macro_rules!` were an item of its module. A module invoking
// `mkimpl!` before defining its own `mkimpl` reaches the parent's, earlier
// in textual scope (or a `#[macro_use]` module's, or one before its file's
// `mod`), which writes an inherent `get`; the rules read the later local
// one, which writes nothing, and linked the trait method. A macro named
// after one of std's (`format!`) may be another by a `use`, and a macro
// its input invokes was not read at all. Which macro a name reaches is not
// read now: any one invoked counts.
#[test]
fn c10_rust_a_macro_counts_whichever_definition_its_name_reaches() {
    let head = "pub struct V;\nimpl V { pub fn new() -> V { V } }\npub trait Tr { fn get(&self); }\nimpl Tr for V { fn get(&self) {} }\n";
    let package = |name: &str| format!("[package]\nname = \"{name}\"\n");
    let textual = format!(
        "{head}macro_rules! mkimpl {{ ($t:ident) => {{ impl $t {{ pub fn get(self) {{}} }} }}; }}\n\
         mod m {{\n    use super::V;\n    mkimpl!(V);\n    macro_rules! mkimpl {{ ($t:ident) => {{}}; }}\n}}\n\
         pub fn c1() {{ V::new().get(); }}\n"
    );
    let macro_use = format!(
        "{head}#[macro_use]\nmod macros {{ macro_rules! mkimpl {{ ($t:ty) => {{ impl $t {{ pub fn get(self) {{}} }} }}; }} }}\n\
         mod m {{\n    mkimpl!(super::V);\n    macro_rules! mkimpl {{ ($t:ty) => {{}}; }}\n}}\npub fn c2() {{ V::new().get(); }}\n"
    );
    let in_file = format!(
        "{head}macro_rules! mk {{ ($t:ident) => {{ impl $t {{ pub fn get(self) {{}} }} }}; }}\nmod b;\npub fn c3() {{ V::new().get(); }}\n"
    );
    let renamed = format!(
        "{head}#[macro_export]\nmacro_rules! impl_maker {{ ($t:ident) => {{ impl $t {{ pub fn get(self) {{}} }} }}; }}\n\
         mod m {{\n    use crate::impl_maker as format;\n    use super::V;\n    macro_rules! wrap {{ ($t:ident) => {{ format!($t); }}; }}\n    wrap!(V);\n}}\n\
         pub fn c4() {{ V::new().get(); }}\n"
    );
    let input = format!(
        "{head}#[macro_export]\nmacro_rules! impl_maker {{ ($t:ident) => {{ impl $t {{ pub fn get(self) {{}} }} }}; }}\n\
         macro_rules! pass {{ ($($i:item)*) => {{ $($i)* }}; }}\npass! {{ impl_maker!(V); }}\npub fn c5() {{ V::new().get(); }}\n"
    );
    let plain = format!(
        "{head}macro_rules! named {{ ($n:ident) => {{ pub fn $n() -> String {{ format!(\"{{}}\", stringify!($n)) }} }}; }}\n\
         named!(helper);\nmod m {{ use crate::Tr; named!(other); pub fn c7() {{ crate::V::new().get(); }} }}\n\
         pub fn c6() {{ V::new().get(); }}\n"
    );
    let manifests: Vec<(String, String)> = [
        "textual",
        "macro_use",
        "in_file",
        "renamed",
        "input",
        "plain",
    ]
    .iter()
    .map(|name| (format!("crates/{name}/Cargo.toml"), package(name)))
    .collect();
    let mut files: Vec<(&str, &str)> = manifests
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    files.extend([
        ("crates/textual/src/lib.rs", textual.as_str()),
        ("crates/macro_use/src/lib.rs", macro_use.as_str()),
        ("crates/in_file/src/lib.rs", in_file.as_str()),
        (
            "crates/in_file/src/b.rs",
            "use crate::V;\nmk!(V);\n#[allow(unused_macros)]\nmacro_rules! mk { ($t:ident) => {}; }\n",
        ),
        ("crates/renamed/src/lib.rs", renamed.as_str()),
        ("crates/input/src/lib.rs", input.as_str()),
        ("crates/plain/src/lib.rs", plain.as_str()),
    ]);
    // cargo 1.99 runs the macro-written `get(self)` in c1 to c5, before
    // `<V as Tr>::get(&self)`, and `<V as Tr>::get` in c6 and c7, where the
    // one `named` before the calls writes no impl and its `format!` is
    // std's: unresolved too, as `named!` is invoked.
    assert_eq!(
        calls(&files)
            .into_iter()
            .filter(|(_, target, _)| target.ends_with(".get"))
            .collect::<Vec<_>>(),
        [
            edge("c1", "V::new().get", None),
            edge("c2", "V::new().get", None),
            edge("c3", "V::new().get", None),
            edge("c4", "V::new().get", None),
            edge("c5", "V::new().get", None),
            edge("c7", "crate::V::new().get", None),
            edge("c6", "V::new().get", None),
        ]
    );
}

// D23 (review of the fix): the word lexer ended a block comment at its
// first `*/`, though Rust's nest, so a quote in the rest of the comment
// opened a string that hid the `impl` after it; and it took a name for a
// macro call only when whitespace alone parted it from its `!`, so
// `inner/* c */!($t)` invoked nothing. Either way the macro was taken to
// write no impl, and the trait method was linked. No token is read now.
#[test]
fn c10_rust_a_macro_counts_whatever_its_comments_hide() {
    let head = "pub struct V;\nimpl V { pub fn new() -> V { V } }\npub trait Tr { fn get(&self); }\nimpl Tr for V { fn get(&self) {} }\n";
    let package = |name: &str| format!("[package]\nname = \"{name}\"\n");
    let nested = format!(
        "{head}macro_rules! mk {{ ($t:ident) => {{ /* outer /* inner */ \" still a comment */ impl $t {{ pub fn get(self) {{}} }} // \"\n}}; }}\n\
         mk!(V);\npub fn c1() {{ V::new().get(); }}\n"
    );
    let bang = format!(
        "{head}macro_rules! inner {{ ($t:ident) => {{ impl $t {{ pub fn get(self) {{}} }} }}; }}\n\
         macro_rules! outer {{ ($t:ident) => {{ inner/* call */!($t); }}; }}\nouter!(V);\n\
         pub struct W;\nimpl W {{ pub fn new() -> W {{ W }} }}\nimpl Tr for W {{ fn get(&self) {{}} }}\n\
         macro_rules! outer2 {{ ($t:ident) => {{ inner\n    // between the name and its bang\n    !($t); }}; }}\nouter2!(W);\n\
         pub fn c2() {{ V::new().get(); }}\npub fn c3() {{ W::new().get(); }}\n"
    );
    let input = format!(
        "{head}macro_rules! pass {{ ($($i:item)*) => {{ $($i)* }}; }}\n\
         pass! {{ /* /* */ \" */ impl V {{ pub fn get(self) {{}} }} }}\npub fn c4() {{ V::new().get(); }}\n"
    );
    let commented = format!(
        "{head}macro_rules! named {{ ($n:ident) => {{ /* impl V {{ fn get }} */ pub fn $n() -> &'static str {{ \"impl get\" }} // impl!\n}}; }}\n\
         named!(helper);\npub fn c5() {{ V::new().get(); }}\n"
    );
    let manifests: Vec<(String, String)> = ["nested", "bang", "input", "commented"]
        .iter()
        .map(|name| (format!("crates/{name}/Cargo.toml"), package(name)))
        .collect();
    let mut files: Vec<(&str, &str)> = manifests
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    files.extend([
        ("crates/nested/src/lib.rs", nested.as_str()),
        ("crates/bang/src/lib.rs", bang.as_str()),
        ("crates/input/src/lib.rs", input.as_str()),
        ("crates/commented/src/lib.rs", commented.as_str()),
    ]);
    // cargo 1.99 runs the macro-written `get(self)` in c1 to c4, before
    // `<_ as Tr>::get(&self)`, and `<V as Tr>::get` in c5, whose macro
    // writes `impl` in comments and a string only, and is invoked.
    assert_eq!(
        calls(&files)
            .into_iter()
            .filter(|(_, target, _)| target.ends_with(".get"))
            .collect::<Vec<_>>(),
        [
            edge("c1", "V::new().get", None),
            edge("c2", "V::new().get", None),
            edge("c3", "W::new().get", None),
            edge("c4", "V::new().get", None),
            edge("c5", "V::new().get", None),
        ]
    );
}

// D23 (review of the fix): an item-position `thread_local!` or
// `lazy_static!` was taken by its name alone for std's or the
// `lazy_static` crate's, which define statics: never read as a macro that
// may write impls, nor as one that may define any item of its module,
// even when the name was a `macro_rules!` of the workspace. It is std's or
// a dependency's now only when the name names no macro of the workspace,
// nor one a `use` renames.
#[test]
fn c10_rust_a_workspace_macro_named_lazy_static_may_write_impls() {
    let head = "pub struct V;\nimpl V { pub fn new() -> V { V } }\npub trait Tr { fn get(&self); fn put(&self); }\nimpl Tr for V { fn get(&self) {} fn put(&self) {} }\n";
    let package = |name: &str| format!("[package]\nname = \"{name}\"\n");
    let named = format!(
        "{head}macro_rules! lazy_static {{ ($t:ident) => {{ impl $t {{ pub fn get(&self) {{}} }} }}; }}\nlazy_static!(V);\n\
         pub struct W;\nimpl W {{ pub fn new() -> W {{ W }} }}\nimpl Tr for W {{ fn get(&self) {{}} fn put(&self) {{}} }}\n\
         mod inner {{ macro_rules! thread_local {{ ($t:ty) => {{ impl $t {{ pub fn get(&self) {{}} }} }}; }} thread_local!(crate::W); }}\n\
         pub fn c1(v: &V) {{ v.get(); }}\npub fn c2() {{ W::new().get(); }}\npub fn c3(v: &V) {{ V::get(v); }}\n"
    );
    let renamed = format!(
        "{head}mod renamed {{\n    macro_rules! mk {{ ($t:ty) => {{ impl $t {{ pub fn put(&self) {{}} }} }}; }}\n    pub(crate) use mk as lazy_static;\n}}\n\
         mod user {{ use crate::renamed::lazy_static; lazy_static!(crate::V); }}\npub fn c4() {{ V::new().put(); }}\n"
    );
    let external = format!(
        "{head}mod ext {{ use dep::other as lazy_static; lazy_static!(crate::V); }}\npub fn c5() {{ V::new().get(); }}\n"
    );
    let items = "pub fn helper() {}\nmod a {\n    use super::*;\n    macro_rules! thread_local { () => { pub fn helper() {} }; }\n    thread_local!();\n    pub fn c6() { helper(); }\n}\n";
    let statics = format!(
        "{head}std::thread_local! {{ static COUNT: u8 = 0; }}\nthread_local!(static OTHER: u8 = 1);\n\
         use lazy_static::lazy_static;\nlazy_static! {{ static ref X: u8 = 1; }}\npub fn helper() {{}}\n\
         mod b {{ use super::*; thread_local! {{ static IN_B: u8 = 0; }} pub fn c8() {{ helper(); }} }}\n\
         pub fn c7() {{ V::new().get(); }}\n"
    );
    let files = [
        ("crates/named/Cargo.toml", package("named")),
        ("crates/named/src/lib.rs", named),
        ("crates/renamed/Cargo.toml", package("renamed")),
        ("crates/renamed/src/lib.rs", renamed),
        (
            "crates/external/Cargo.toml",
            format!("{}[dependencies]\ndep = \"1\"\n", package("external")),
        ),
        ("crates/external/src/lib.rs", external),
        ("crates/items/Cargo.toml", package("items")),
        ("crates/items/src/lib.rs", items.to_owned()),
        (
            "crates/statics/Cargo.toml",
            format!(
                "{}[dependencies]\nlazy_static = \"1\"\n",
                package("statics")
            ),
        ),
        ("crates/statics/src/lib.rs", statics),
    ];
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, text)| (*path, text.as_str()))
        .collect();
    // cargo 1.99 runs the macro-written `get` in c1 to c3 and `put` in c4,
    // and the macro-written `helper` in c6, which shadows the glob's. A
    // dependency's macro renamed `lazy_static` may write anything (c5).
    // std's `thread_local!` and the `lazy_static` crate's write no impl of
    // `V` and define statics only: c7 runs `<V as Tr>::get`, c8 the
    // glob's `helper`.
    assert_eq!(
        calls(&files)
            .into_iter()
            .filter(|(src, _, _)| src.starts_with('c'))
            .filter(|(_, target, _)| !target.ends_with("::new"))
            .collect::<Vec<_>>(),
        [
            edge("c1", "v.get", None),
            edge("c2", "W::new().get", None),
            edge("c3", "V::get", None),
            edge("c4", "V::new().put", None),
            edge("c5", "V::new().get", None),
            edge("c6", "helper", None),
            edge(
                "c8",
                "helper",
                Some("crates/statics/src/lib.rs#helper#function")
            ),
            edge(
                "c7",
                "V::new().get",
                Some("crates/statics/src/lib.rs#<V as Tr>::get#method")
            ),
        ]
    );
}

/// The calls of the functions named `c…` in one world of files, `::new`
/// calls aside.
fn c_calls(files: &[(String, String)]) -> Vec<(String, String, Option<String>)> {
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    calls(&files)
        .into_iter()
        .filter(|(src, _, _)| src.starts_with('c'))
        .filter(|(_, target, _)| !target.ends_with("::new"))
        .collect()
}

/// One package named `name` with `lib` as its library, and `deps` as its
/// `[dependencies]` lines.
fn package_files(name: &str, deps: &str, lib: &str) -> [(String, String); 2] {
    [
        (
            format!("crates/{name}/Cargo.toml"),
            format!("[package]\nname = \"{name}\"\n[dependencies]\n{deps}"),
        ),
        (format!("crates/{name}/src/lib.rs"), lib.to_owned()),
    ]
}

const HEAD: &str = "pub struct V;\nimpl V { pub fn new() -> V { V } }\npub trait Tr { fn get(&self); }\nimpl Tr for V { fn get(&self) {} }\n";

const GET: &str = "($t:ident) => { impl $t { pub fn get(&self) {} } };";

// D23 (second review of the fix): a bare `lazy_static!` or `thread_local!`
// was std's or a dependency's whenever no item or `use` of its module
// bound the name, so a workspace macro of that name reached by
// `#[macro_use] extern crate`, or by a glob, was never read as one that
// may write impls; nor was a dependency's macro another name that a
// module of the workspace re-exports as `lazy_static`.
#[test]
fn c10_rust_a_statics_macro_a_crate_exports_or_a_glob_brings_may_write_impls() {
    // A crate of the workspace exports both names.
    let exported = format!(
        "#[macro_export]\nmacro_rules! lazy_static {{ {GET} }}\n#[macro_export]\nmacro_rules! thread_local {{ {GET} }}\n"
    );
    let mk = "mk = { path = \"../mk\" }\n";
    let mut routes: Vec<(String, String)> = Vec::new();
    for (name, deps, lib) in [
        ("mk", "", exported),
        (
            "macro_use",
            mk,
            format!(
                "#[macro_use]\nextern crate mk;\n{HEAD}lazy_static!(V);\npub fn c1(v: &V) {{ v.get(); }}\npub fn c2(v: &V) {{ V::get(v); }}\n"
            ),
        ),
        (
            "macro_use_tl",
            mk,
            format!(
                "#[macro_use]\nextern crate mk;\n{HEAD}thread_local!(V);\npub fn c3(v: &V) {{ v.get(); }}\n"
            ),
        ),
        (
            "glob",
            mk,
            format!("use mk::*;\n{HEAD}lazy_static!(V);\npub fn c4(v: &V) {{ v.get(); }}\n"),
        ),
    ] {
        routes.extend(package_files(name, deps, &lib));
    }
    // cargo 1.99 runs the macro-written `get` in c1 to c4.
    assert_eq!(
        c_calls(&routes),
        [
            edge("c1", "v.get", None),
            edge("c2", "V::get", None),
            edge("c3", "v.get", None),
            edge("c4", "v.get", None),
        ]
    );

    // No crate of the workspace exports either name.
    let ls = "lazy_static = \"1\"\n";
    let mut bindings: Vec<(String, String)> = Vec::new();
    for (name, deps, lib) in [
        (
            "glob_local",
            "",
            format!(
                "mod m {{\n    macro_rules! mkimpl {{ {GET} }}\n    pub(crate) use mkimpl as lazy_static;\n}}\nuse m::*;\n{HEAD}lazy_static!(V);\npub fn c5(v: &V) {{ v.get(); }}\n"
            ),
        ),
        (
            "reexport_glob",
            "dep = \"1\"\n",
            format!(
                "{HEAD}mod util {{ pub use dep::other as lazy_static; }}\nmod g {{ use crate::util::*; lazy_static!(crate::V); }}\npub fn c6(v: &V) {{ v.get(); }}\n"
            ),
        ),
        (
            "reexport_use",
            "dep = \"1\"\n",
            format!(
                "{HEAD}mod util {{ pub use dep::other as lazy_static; }}\nmod u {{ use crate::util::lazy_static; lazy_static!(crate::V); }}\npub fn c7(v: &V) {{ v.get(); }}\n"
            ),
        ),
        (
            "through_super",
            ls,
            format!(
                "use lazy_static::lazy_static;\n{HEAD}mod inner {{\n    use super::*;\n    lazy_static! {{ static ref X: u8 = 1; }}\n    pub fn c8(v: &V) {{ v.get(); }}\n}}\n\
                     mod util {{ pub use lazy_static::lazy_static; }}\nmod via {{ use crate::util::lazy_static; lazy_static! {{ static ref Y: u8 = 1; }} }}\npub fn c9(v: &V) {{ v.get(); }}\n"
            ),
        ),
        (
            "macro_use_dep",
            ls,
            format!(
                "#[macro_use]\nextern crate lazy_static;\nuse std::collections::*;\n{HEAD}lazy_static! {{ static ref X: HashMap<u8, u8> = HashMap::new(); }}\npub fn c10(v: &V) {{ v.get(); }}\n"
            ),
        ),
    ] {
        bindings.extend(package_files(name, deps, &lib));
    }
    // cargo 1.99 runs the macro-written `get` in c5. A dependency's macro
    // re-exported as `lazy_static` may write anything (c6, c7). The
    // `lazy_static` crate's, through `use super::*`, a module's re-export
    // under its own name or `#[macro_use]` beside a glob of std, writes no
    // impl of `V`: c8 to c10 run `<V as Tr>::get`. It writes a struct of
    // each static's name, which may shadow a glob's type, so its scope is
    // marked, and c8's `V`, which a glob brings there, is unresolved. A
    // glob of a dependency (std's included) may bring a macro of the name,
    // so c10 is unresolved.
    let by_trait = |name: &str| format!("crates/{name}/src/lib.rs#<V as Tr>::get#method");
    assert_eq!(
        c_calls(&bindings),
        [
            edge("c5", "v.get", None),
            edge("c6", "v.get", None),
            edge("c7", "v.get", None),
            edge("c8", "v.get", None),
            edge("c9", "v.get", Some(&by_trait("through_super"))),
            edge("c10", "v.get", None),
        ]
    );
}

// D23 (review of the blunt rule): the `lazy_static` crate's macro was
// taken to define statics only, and its scope was left unmarked; but it
// writes a struct of each static's name, which shadows a glob's type of
// that name, and `V::deref` linked the glob type's method. Its scope is
// marked now.
#[test]
fn c10_rust_lazy_static_writes_a_type_of_each_static_s_name() {
    let lib = "mod other {\n    pub struct V;\n    impl V { pub fn deref(&self) -> &Vec<u8> { unreachable!() } }\n}\n\
               #[macro_use]\nextern crate lazy_static;\nuse other::*;\nuse std::ops::Deref;\n\
               lazy_static! { static ref V: Vec<u8> = vec![1]; }\n\
               pub fn c1() { let _ = V::deref(&V); }\npub fn c2() { let _ = <V>::deref(&V); }\n";
    let files = package_files("app", "lazy_static = \"1\"\n", lib);
    // cargo 1.99 runs `<V as Deref>::deref` of the struct the macro writes
    // in c1 and c2, never `other::V::deref`.
    assert_eq!(
        c_calls(&files),
        [edge("c1", "V::deref", None), edge("c2", "<V>::deref", None)]
    );
}

// D23 (second review of the fix): a macro's rules invoking a macro were
// read by the call's last word alone. A metavariable named like one of
// std's expression macros (`$format!`, `$println! { … }`) and a path
// outside std (`m::vec!`) were taken for std's, which writes no item,
// though they invoke whatever the input or the path names; and a
// `macro_rules!` of the workspace named like one of std's counted only
// when its own rules wrote an impl, not when the macro it invokes does
// (`format!` calling `vec!`), nor when a crate exports it to
// `#[macro_use]`. Each world below holds one of them. What a macro's
// rules invoke is not read now: any macro invoked where items go counts.
#[test]
fn c10_rust_a_macro_counts_whatever_its_rules_invoke() {
    let inner = format!("macro_rules! inner {{ {GET} }}\n");
    let callers = "pub fn c1(v: &V) { v.get(); }\npub fn c2(v: &V) { V::get(v); }\n";
    let worlds: Vec<Vec<(String, String)>> = vec![
        package_files(
            "metavariable",
            "",
            &format!("{HEAD}{inner}macro_rules! outer {{ ($format:ident, $t:ident) => {{ $format!($t); }}; }}\nouter!(inner, V);\n{callers}"),
        )
        .into(),
        package_files(
            "braced",
            "",
            &format!("{HEAD}{inner}macro_rules! outer {{ ($println:ident; $t:ident) => {{ $println! {{ $t }} }}; }}\nouter!(inner; V);\n{callers}"),
        )
        .into(),
        package_files(
            "path",
            "",
            &format!("{HEAD}mod m {{\n    macro_rules! mkimpl {{ {GET} }}\n    pub(crate) use mkimpl as vec;\n}}\nmacro_rules! outer {{ ($t:ident) => {{ m::vec!($t); }}; }}\nouter!(V);\n{callers}"),
        )
        .into(),
        package_files(
            "chain",
            "",
            &format!("{HEAD}macro_rules! vec {{ {GET} }}\nmacro_rules! format {{ ($t:ident) => {{ vec!($t); }}; }}\nmacro_rules! outer {{ ($t:ident) => {{ format!($t); }}; }}\nmod user {{ outer!(V); use super::V; }}\n{callers}"),
        )
        .into(),
        [
            package_files(
                "mk",
                "",
                &format!("#[macro_export]\nmacro_rules! format {{ {GET} }}\n"),
            ),
            package_files(
                "macro_use",
                "mk = { path = \"../mk\" }\n",
                &format!("#[macro_use]\nextern crate mk;\n{HEAD}macro_rules! outer {{ ($t:ident) => {{ format!($t); }}; }}\nouter!(V);\n{callers}"),
            ),
        ]
        .concat(),
    ];
    // cargo 1.99 runs the macro-written `get` in c1 and c2 of every world.
    for world in &worlds {
        assert_eq!(
            c_calls(world),
            [edge("c1", "v.get", None), edge("c2", "V::get", None)],
            "{}",
            world[0].0
        );
    }
    // An exported macro invoking itself through `$crate::`, and std's
    // macros by a path through std, write no impl: cargo 1.99 runs
    // `<V as Tr>::get`, unresolved as the macros are invoked.
    let control = package_files(
        "control",
        "",
        &format!(
            "{HEAD}#[macro_export]\nmacro_rules! rec {{ () => {{}}; ($t:ident) => {{ $crate::rec!(); }}; }}\nrec!(V);\n\
                  macro_rules! helper {{ ($t:ident) => {{ fn helper() -> String {{ ::std::format!(\"{{:?}}\", std::vec![stringify!($t)]) }} }}; }}\nhelper!(V);\n{callers}"
        ),
    );
    assert_eq!(
        c_calls(&control),
        [edge("c1", "v.get", None), edge("c2", "V::get", None)]
    );
}

// D23 (third review of the fix): whether each `thread_local!` or
// `lazy_static!` was std's was decided once, before any module was
// marked, so a call whose name a marked module binds (`b`'s macro may
// define `lazy_static` there) was taken for std's, its module left
// unmarked, and a glob's function linked where the macro's stands.
#[test]
fn c10_rust_a_statics_call_is_read_again_once_a_module_it_names_is_marked() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "macro_rules! mkfn { () => { pub fn helper() {} }; }\npub(crate) use mkfn;\n\
             macro_rules! defs { () => { pub(crate) use crate::mkfn as lazy_static; }; }\npub(crate) use defs;\n\
             mod b {\n    use crate::defs as thread_local;\n    thread_local!();\n}\n\
             mod other {\n    pub fn helper() {}\n}\n\
             mod a {\n    use crate::b::lazy_static;\n    use crate::other::*;\n    lazy_static!();\n    pub fn c1() { helper(); }\n}\n\
             mod plain {\n    use crate::other::*;\n    pub fn c2() { helper(); }\n}\n",
        ),
    ];
    // cargo 1.99 runs the `helper` that `lazy_static!()` defines in `a`;
    // with no macro in `plain`, the glob's.
    assert_eq!(
        calls(&files),
        [
            edge("c1", "helper", None),
            edge("c2", "helper", Some("src/lib.rs#helper#function")),
        ]
    );
}

// D23 (third review of the fix): a macro invoked by the path a repetition
// spells (`$($seg)::+ !(…)`, `$($p)* !(…)`) was not read as a call, the
// token before the `!` ending no path, so a macro whose own tokens hold
// no `impl` was taken to write nothing, whatever its input names.
#[test]
fn c10_rust_a_macro_counts_whatever_a_repetition_invokes() {
    let callers = "pub fn c1(v: &V) { v.get(); }\npub fn c2(v: &V) { V::get(v); }\n";
    let mkimpl = format!("macro_rules! mkimpl {{ {GET} }}\npub(crate) use mkimpl;\n");
    for (name, outer, call) in [
        (
            "path",
            "($($seg:ident)::+ ; $t:ty) => { $($seg)::+ !($t); };",
            "outer!(mkimpl; crate::V);",
        ),
        (
            "tokens",
            "($t:ty ; $($p:tt)*) => { $($p)* !($t); };",
            "outer!(V ; crate::mkimpl);",
        ),
    ] {
        let world = package_files(
            name,
            "",
            &format!("{HEAD}{mkimpl}macro_rules! outer {{ {outer} }}\n{call}\n{callers}"),
        );
        // cargo 1.99 runs the macro-written `get` in c1 and c2.
        assert_eq!(
            c_calls(&world),
            [edge("c1", "v.get", None), edge("c2", "V::get", None)],
            "{name}"
        );
    }
}

// D23 (third review of the fix): a path through `std`, `core` or `alloc`
// was std's macro whatever its first segment named where the outer macro
// is invoked, though a `mod std` or a `use … as alloc` there takes the
// crate's place, and rustc then invokes the workspace macro.
#[test]
fn c10_rust_a_macro_counts_whatever_path_through_std_it_invokes() {
    let callers = "pub fn c1(v: &V) { v.get(); }\npub fn c2(v: &V) { V::get(v); }\n";
    let mkimpl = format!("macro_rules! mkimpl {{ {GET} }}\npub(crate) use mkimpl;\n");
    for (name, outer, user) in [
        (
            "mod_std",
            "std::vec!($t);",
            "mod std { pub(crate) use crate::mkimpl as vec; }\n    outer!(crate::V);",
        ),
        (
            "mod_core",
            "core::panic!($t);",
            "mod core { pub(crate) use crate::mkimpl as panic; }\n    outer!(crate::V);",
        ),
        (
            "use_as_alloc",
            "alloc::format!($t);",
            "use crate::shim as alloc;\n    outer!(crate::V);",
        ),
    ] {
        let world = package_files(
            name,
            "",
            &format!(
                "{HEAD}{mkimpl}mod shim {{ pub(crate) use crate::mkimpl as format; }}\n\
                 macro_rules! outer {{ ($t:ty) => {{ {outer} }}; }}\nmod user {{\n    {user}\n}}\n{callers}"
            ),
        );
        // cargo 1.99 runs the macro-written `get` in c1 and c2.
        assert_eq!(
            c_calls(&world),
            [edge("c1", "v.get", None), edge("c2", "V::get", None)],
            "{name}"
        );
    }
    // Where `std`, `core` and `alloc` are the crates (`extern crate alloc`
    // binding its own name), std's macros write no item: cargo 1.99 runs
    // `<V as Tr>::get`, unresolved as `helper!` is invoked.
    let control = package_files(
        "control",
        "",
        &format!(
            "extern crate alloc;\n{HEAD}macro_rules! helper {{ ($t:ident) => {{ fn $t() {{ let _ = ::std::format!(\"\"); let _ = core::stringify!(x); let _ = alloc::vec![1]; }} }}; }}\n\
             helper!(h1);\nmod user {{\n    helper!(h2);\n}}\n{callers}"
        ),
    );
    assert_eq!(
        c_calls(&control),
        [edge("c1", "v.get", None), edge("c2", "V::get", None)]
    );
}

// D23 (third review of the fix): a name of std's was std's when no item
// or `use` of the calling module bound it, but the expansion may bind it
// itself, beside the call or in a module it writes, and rustc then
// invokes the workspace macro.
#[test]
fn c10_rust_a_macro_counts_whatever_names_its_expansion_binds() {
    let callers = "pub fn c1(v: &V) { v.get(); }\npub fn c2(v: &V) { V::get(v); }\n";
    let mkimpl = format!("macro_rules! mkimpl {{ {GET} }}\npub(crate) use mkimpl;\n");
    for (name, rules, call) in [
        (
            "beside",
            "use crate::mkimpl as format; format!($t);",
            "mod user { outer!(crate::V); }",
        ),
        (
            "module",
            "mod inner { use crate::mkimpl as vec; vec!($t); }",
            "outer!(crate::V);",
        ),
        (
            "input",
            "$($i)* format!($t);",
            "mod user { outer!(crate::V; use crate::mkimpl as format;); }",
        ),
    ] {
        let pattern = if name == "input" {
            "$t:ty; $($i:item)*"
        } else {
            "$t:ty"
        };
        let world = package_files(
            name,
            "",
            &format!(
                "{HEAD}{mkimpl}macro_rules! outer {{ ({pattern}) => {{ {rules} }}; }}\n{call}\n{callers}"
            ),
        );
        // cargo 1.99 runs the macro-written `get` in c1 and c2.
        assert_eq!(
            c_calls(&world),
            [edge("c1", "v.get", None), edge("c2", "V::get", None)],
            "{name}"
        );
    }
}

// D23 (third review of the fix): globs were not read for `thread_local`,
// nor for a name of std's invoked by a macro's rules, as a glob's macro
// beside std's prelude is ambiguous (E0659). Under `no_implicit_prelude`,
// or in a `no_std` crate for std's own macros, no prelude holds one, and
// the glob's is the macro rustc invokes. Where the prelude is missing is
// not read now: a glob's `thread_local!` is no std's, as the rules cannot
// tell which glob may hold one.
#[test]
fn c10_rust_a_macro_counts_where_std_s_prelude_may_be_missing() {
    let callers = "pub fn c1(v: &V) { v.get(); }\npub fn c2(v: &V) { V::get(v); }\n";
    let mkimpl = format!("macro_rules! mkimpl {{ {GET} }}\npub(crate) use mkimpl;\n");
    let rename = |name: &str| format!("mod w {{ pub(crate) use crate::mkimpl as {name}; }}\n");
    let outer = |name: &str| format!("macro_rules! outer {{ ($t:ty) => {{ {name}!($t); }}; }}\n");
    let worlds: Vec<(&str, Vec<(String, String)>)> = vec![
        (
            "attribute",
            package_files(
                "attribute",
                "",
                &format!(
                    "{HEAD}{mkimpl}{}#[no_implicit_prelude]\nmod m {{\n    use crate::w::*;\n    thread_local!(crate::V);\n}}\n{callers}",
                    rename("thread_local")
                ),
            )
            .into(),
        ),
        (
            "file",
            [
                package_files(
                    "file",
                    "",
                    &format!(
                        "{HEAD}{mkimpl}{}#[no_implicit_prelude]\nmod m;\n{callers}",
                        rename("thread_local")
                    ),
                )
                .to_vec(),
                vec![(
                    "crates/file/src/m.rs".to_owned(),
                    "use crate::w::*;\nthread_local!(crate::V);\n".to_owned(),
                )],
            ]
            .concat(),
        ),
        (
            "inner",
            package_files(
                "inner",
                "",
                &format!(
                    "{HEAD}{mkimpl}{}{}mod m {{\n    #![no_implicit_prelude]\n    use crate::w::*;\n    outer!(crate::V);\n}}\n{callers}",
                    rename("format"),
                    outer("format")
                ),
            )
            .into(),
        ),
        (
            "no_std_vec",
            package_files(
                "no_std_vec",
                "",
                &format!(
                    "#![no_std]\nextern crate std;\n{HEAD}{mkimpl}{}{}mod m {{ use crate::w::*; outer!(crate::V); }}\n{callers}",
                    rename("vec"),
                    outer("vec")
                ),
            )
            .into(),
        ),
        (
            "no_std_tl",
            package_files(
                "no_std_tl",
                "",
                &format!(
                    "#![no_std]\nextern crate std;\n{HEAD}{mkimpl}{}mod m {{ use crate::w::*; thread_local!(crate::V); }}\n{callers}",
                    rename("thread_local")
                ),
            )
            .into(),
        ),
    ];
    // cargo 1.99 runs the macro-written `get` in c1 and c2 of every world.
    for (name, world) in &worlds {
        assert_eq!(
            c_calls(world),
            [edge("c1", "v.get", None), edge("c2", "V::get", None)],
            "{name}"
        );
    }
    // core's own macros stay in a `no_std` crate's prelude, beside which a
    // glob's is ambiguous: cargo 1.99 runs `<V as Tr>::get`, unresolved as
    // `outer!` is invoked.
    let control = package_files(
        "control",
        "",
        &format!(
            "#![no_std]\nextern crate std;\n{HEAD}macro_rules! outer {{ ($t:ident) => {{ pub fn $t(f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {{ write!(f, \"\") }} }}; }}\n\
             mod m {{ use core::fmt::*; outer!(helper); }}\n{callers}"
        ),
    );
    assert_eq!(
        c_calls(&control),
        [edge("c1", "v.get", None), edge("c2", "V::get", None)]
    );
}

/// A proc-macro package `pm` whose functions are `lib`.
fn proc_macro_package(lib: &str) -> [(String, String); 2] {
    [
        (
            "crates/pm/Cargo.toml".to_owned(),
            "[package]\nname = \"pm\"\n[lib]\nproc-macro = true\n".to_owned(),
        ),
        ("crates/pm/src/lib.rs".to_owned(), lib.to_owned()),
    ]
}

const STREAM: &str = "(input: proc_macro::TokenStream) -> proc_macro::TokenStream { input }";

// D23 (third review of the fix): only `macro_rules!` counted as macros of
// the workspace, so a function-like procedural macro of a workspace crate
// named `lazy_static`, `thread_local` or `format`, whichever way it was
// reached, was taken for std's or the lazy_static crate's, which write no
// impl, and a path to one (`pm::lazy_static!`), binding no macro the rules
// see, was exempt too.
#[test]
fn c10_rust_a_procedural_macro_of_the_workspace_is_a_macro_of_the_workspace() {
    use super::super::{AttributeUse, ProcKind, ProcMacro};
    let defined = facts(&extract(
        "src/lib.rs",
        &format!(
            "#[proc_macro]\npub fn lazy_static{STREAM}\n#[proc_macro_derive(Get, attributes(get))]\npub fn derive_get{STREAM}\n\
             #[proc_macro_attribute]\npub fn add_get(_: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {{ item }}\n\
             #[test]\nfn t() {{}}\n#[test]\nfn t2() {{}}\n#[rustfmt::skip]\nfn u() {{}}\n#[pm::add_get]\nfn v() {{}}\nimpl S {{\n    #[inline]\n    #[tracing::instrument]\n    fn w() {{}}\n}}\n\
             #[cfg_attr(feature = \"a,b\", inline, pm::traced(x, y))]\nfn z() {{}}\n"
        ),
    ));
    let proc = |kind, name: &str| ProcMacro {
        kind,
        name: name.to_owned(),
    };
    assert_eq!(
        defined.proc_macros,
        [
            proc(ProcKind::Bang, "lazy_static"),
            proc(ProcKind::Derive, "Get"),
            proc(ProcKind::Attribute, "add_get")
        ]
    );
    let used: Vec<(u32, &str)> = defined
        .attributes
        .iter()
        .map(|AttributeUse { scope, name, .. }| (*scope, name.as_str()))
        .collect();
    // `test` may be a `use`d macro (`use test_log::test;`), so it is kept,
    // once per scope; Rust's inert attributes and a tool's are not.
    assert_eq!(
        used,
        [
            (0, "test"),
            (0, "add_get"),
            (1, "instrument"),
            (0, "traced")
        ]
    );

    let pm = proc_macro_package(&format!(
        "#[proc_macro]\npub fn lazy_static{STREAM}\n#[proc_macro]\npub fn thread_local{STREAM}\n#[proc_macro]\npub fn format{STREAM}\n"
    ));
    let dep = "pm = { path = \"../pm\" }\n";
    let callers = "pub fn c1(v: &V) { v.get(); }\npub fn c2(v: &V) { V::get(v); }\n";
    for (name, route) in [
        ("path", "pm::lazy_static!(V);".to_owned()),
        ("use_lazy", "use pm::lazy_static;\nlazy_static!(V);".to_owned()),
        ("glob", "use pm::*;\nlazy_static!(V);".to_owned()),
        ("use_tl", "use pm::thread_local;\nthread_local!(V);".to_owned()),
        (
            "macro_use_lazy",
            "#[macro_use]\nextern crate pm;\nlazy_static!(V);".to_owned(),
        ),
        (
            "macro_use_tl",
            "#[macro_use]\nextern crate pm;\nthread_local!(V);".to_owned(),
        ),
        (
            "macro_use_format",
            "#[macro_use]\nextern crate pm;\nmacro_rules! outer { ($t:ty) => { format!($t); }; }\nouter!(V);"
                .to_owned(),
        ),
    ] {
        let world = [
            pm.to_vec(),
            package_files(name, dep, &format!("{route}\n{HEAD}{callers}")).to_vec(),
        ]
        .concat();
        // cargo 1.99 runs the `get` the procedural macro writes in c1 and c2.
        assert_eq!(
            c_calls(&world),
            [edge("c1", "v.get", None), edge("c2", "V::get", None)],
            "{name}"
        );
    }
}

// D23 (third review of the fix, minor): a derive or attribute macro of a
// workspace crate may write an impl of any name, but only a dependency's
// was a known gap; the trait's method was linked.
#[test]
fn c10_rust_a_derive_or_attribute_macro_of_the_workspace_may_write_anything() {
    let pm = proc_macro_package(&format!(
        "#[proc_macro_derive(Get)]\npub fn derive_get{STREAM}\n\
         #[proc_macro_attribute]\npub fn add_get(_: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {{ item }}\n"
    ));
    let world = [
        pm.to_vec(),
        package_files(
            "app",
            "pm = { path = \"../pm\" }\n",
            "pub trait Tr { fn get(&self); }\n#[derive(pm::Get)]\npub struct V;\nimpl Tr for V { fn get(&self) {} }\n\
             pub struct W;\nimpl Tr for W { fn get(&self) {} }\n#[pm::add_get]\nfn unrelated() {}\n\
             pub fn c1(v: &V) { v.get(); }\npub fn c2(w: &W) { w.get(); }\n",
        )
        .to_vec(),
        package_files(
            "union",
            "pm = { path = \"../pm\" }\n",
            "pub trait Tr { fn get(&self); }\n#[derive(pm::Get)]\npub union U { a: u8 }\nimpl Tr for U { fn get(&self) {} }\n\
             pub fn c4(u: &U) { u.get(); }\n",
        )
        .to_vec(),
        package_files(
            "block",
            "pm = { path = \"../pm\" }\n",
            "pub trait Tr { fn get(&self); }\npub struct W;\nimpl Tr for W { fn get(&self) {} }\n\
             fn host() {\n    #[pm::add_get]\n    fn inner() {}\n}\npub fn c5(w: &W) { w.get(); }\n",
        )
        .to_vec(),
        package_files(
            "cfg_attr",
            "pm = { path = \"../pm\" }\n",
            "pub trait Tr { fn get(&self); }\npub struct W;\nimpl Tr for W { fn get(&self) {} }\n\
             #[cfg_attr(all(), inline, pm::add_get)]\nfn unrelated() {}\npub fn c6(w: &W) { w.get(); }\n",
        )
        .to_vec(),
        package_files(
            "renamed",
            "pm = { path = \"../pm\" }\n",
            "pub trait Tr { fn get(&self); }\npub struct W;\nimpl Tr for W { fn get(&self) {} }\n\
             mod util { pub use pm::add_get as traced; }\nuse util::traced as logged;\n#[logged]\nfn unrelated() {}\npub fn c7(w: &W) { w.get(); }\n",
        )
        .to_vec(),
        package_files(
            "plain",
            "dep = \"1\"\n",
            "pub trait Tr { fn get(&self); }\n#[derive(Clone, Debug)]\npub struct X;\nimpl Tr for X { fn get(&self) {} }\n\
             #[inline]\n#[dep::instrument]\nfn instrumented() {}\npub fn c3(x: &X) { x.get(); }\n",
        )
        .to_vec(),
    ]
    .concat();
    // cargo 1.99 runs the macro-written `get` in c1, c2, c4 (a union's
    // derive), c5 (an attribute in a block) and c6 (an attribute a
    // `cfg_attr` applies) and c7 (one used by the name `use`s give it).
    // std's derives and Rust's
    // attributes write no impl of the name, and an attribute of a
    // dependency stays the known gap: c3 runs `<X as Tr>::get`.
    assert_eq!(
        c_calls(&world),
        [
            edge("c1", "v.get", None),
            edge("c2", "w.get", None),
            edge("c4", "u.get", None),
            edge("c5", "w.get", None),
            edge("c6", "w.get", None),
            edge("c7", "w.get", None),
            edge(
                "c3",
                "x.get",
                Some("crates/plain/src/lib.rs#<X as Tr>::get#method")
            ),
        ]
    );
}

// D23 (review of the statics allowance): `lazy_static!` puts a static's
// attributes on the struct it writes, std's `thread_local!` on the const,
// so a derive or an attribute macro written on a static there runs; such
// a call was taken for theirs all the same, and an attribute inside it
// was never read. A `cfg_attr` nested in another applied an attribute
// macro no rule read either.
#[test]
fn c10_rust_a_statics_call_holding_an_attribute_may_write_anything() {
    let pm = proc_macro_package(&format!(
        "#[proc_macro_derive(GetOther)]\npub fn derive_get_other{STREAM}\n\
         #[proc_macro_attribute]\npub fn add_get(_: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {{ item }}\n"
    ));
    let deps = "pm = { path = \"../pm\" }\nlazy_static = \"1\"\ndep = \"1\"\n";
    let callers = "pub fn c1(v: &V) { v.get(); }\npub fn c2(v: &V) { V::get(v); }\n";
    let mut world = pm.to_vec();
    for (name, code) in [
        (
            "lazy_derive",
            "use lazy_static::lazy_static;\npub type Other = V;\nlazy_static! { #[derive(pm::GetOther)] static ref X: u8 = 1; }",
        ),
        (
            "lazy_attr",
            "#[macro_use]\nextern crate lazy_static;\nlazy_static! { #[pm::add_get] static ref X: u8 = 1; }",
        ),
        (
            "tl_attr",
            "thread_local! { #[pm::add_get] static X: u8 = 1; }",
        ),
        (
            "tl_spaced",
            "use pm::add_get;\nthread_local! { # [add_get] static X: u8 = 1; }",
        ),
        (
            "tl_cfg_attr",
            "thread_local! { #[cfg_attr(all(), pm::add_get)] static X: u8 = 1; }",
        ),
        (
            "lazy_dep",
            "use lazy_static::lazy_static;\nlazy_static! { #[dep::add_get] static ref X: u8 = 1; }",
        ),
        (
            "nested_cfg_attr",
            "#[cfg_attr(all(), inline, cfg_attr(all(), pm::add_get))]\nfn unrelated() {}",
        ),
        (
            "tl_block",
            "fn helper() {\n    thread_local! { #[pm::add_get] static X: u8 = 1; }\n}",
        ),
        (
            "inert",
            "use lazy_static::lazy_static;\nlazy_static! {\n    /// Docs.\n    #[allow(dead_code)]\n    #[cfg_attr(all(), doc = \"x\")]\n    static ref X: u8 = 1;\n    static ref R: &'static str = r#\"#[pm::add_get]\"#;\n}\n\
             thread_local! { #[cfg(all())] static Y: u8 = 1; #[rustfmt::skip] static Z: u8 = 2; }\n\
             fn helper() {\n    thread_local! { #[allow(dead_code)] static B: u8 = 1; }\n}",
        ),
    ] {
        world.extend(package_files(
            name,
            deps,
            &format!("{code}\n{HEAD}{callers}"),
        ));
    }
    // cargo 1.99 runs the `get` the derive or the attribute writes in the
    // first eight packages, `impl Other` and `impl V` beside the struct or
    // the const the statics macro writes, in a block too; inert attributes
    // write nothing: the last runs `<V as Tr>::get`.
    let inert = Some("crates/inert/src/lib.rs#<V as Tr>::get#method");
    assert_eq!(
        c_calls(&world),
        (0..8)
            .flat_map(|_| [edge("c1", "v.get", None), edge("c2", "V::get", None)])
            .chain([edge("c1", "v.get", inert), edge("c2", "V::get", inert)])
            .collect::<Vec<_>>()
    );
}

// D23 (the blunt rule): a macro invoked where items go counts in the
// crates compiling its crate, transitively, and in no other: not in the
// crates it compiles, nor beside them. One under `cfg(test)` counts in
// its own crate only. One in an impl's or a trait's body counts as one
// among a module's items, and so does a `thread_local!` that a workspace
// crate exports to `#[macro_use]`, or a `lazy_static!` beside a macro
// whose expansion may define one; a `thread_local` macro of a crate the
// caller's does not compile changes nothing.
#[test]
fn c10_rust_a_macro_counts_in_the_crates_compiling_its_crate() {
    let get = "{ println!(\"{}:{}\", file!(), line!()); }";
    let direction = [
        package_files(
            "base",
            "",
            &format!(
                "pub struct V;\nimpl V {{ pub fn get(self) {get} pub fn put(&self) {get} }}\n\
                 pub fn c1(v: V) {{ v.get(); }}\npub fn c2(v: &V) {{ v.put(); }}\n"
            ),
        ),
        package_files(
            "user",
            "base = { path = \"../base\" }\n",
            &format!(
                "pub trait Tr {{ fn get(&self); fn put(&self); }}\n\
                 macro_rules! mk {{ ($t:ty) => {{ impl Tr for $t {{ fn get(&self) {get} fn put(&self) {get} }} }}; }}\n\
                 mk!(base::V);\npub fn c3(v: &base::V) {{ v.get(); }}\npub mod plain {{ pub fn c4(v: &base::V) {{ v.put(); }} }}\n"
            ),
        ),
        package_files(
            "side",
            "base = { path = \"../base\" }\n",
            "pub fn c5(v: base::V) { v.get(); }\npub fn c6(v: &base::V) { v.put(); }\n",
        ),
        package_files(
            "top",
            "base = { path = \"../base\" }\nuser = { path = \"../user\" }\n",
            "use user::Tr;\npub fn c7(v: &base::V) { v.get(); }\npub fn c8(v: base::V) { base::V::get(v); }\n",
        ),
    ]
    .concat();
    // cargo 1.99 runs `V::get` (c1, c5, c8), `V::put` (c2, c4, c6), and the
    // macro's `<V as Tr>::get` (c3, c7). `base` and `side` compile no
    // macro.
    let base = |name: &str| Some(format!("crates/base/src/lib.rs#V::{name}#method"));
    assert_eq!(
        c_calls(&direction),
        [
            edge("c1", "v.get", base("get").as_deref()),
            edge("c2", "v.put", base("put").as_deref()),
            edge("c3", "v.get", None),
            edge("c4", "v.put", None),
            edge("c5", "v.get", base("get").as_deref()),
            edge("c6", "v.put", base("put").as_deref()),
            edge("c7", "v.get", None),
            edge("c8", "base::V::get", None),
        ]
    );

    let cfg_test = [
        package_files(
            "tested",
            "",
            &format!(
                "pub struct V;\npub trait Tr {{ fn get(&self); }}\nimpl Tr for V {{ fn get(&self) {get} }}\n\
                 thread_local! {{ pub static COUNT: u8 = 0; }}\npub fn c1(v: &V) {{ v.get(); }}\n\
                 #[cfg(test)]\nmod t {{\n    macro_rules! mk {{ ($t:ty) => {{ impl $t {{ pub fn get(&self) {{}} }} }}; }}\n    mk!(super::V);\n}}\n"
            ),
        ),
        package_files(
            "other",
            "",
            "#[macro_export]\nmacro_rules! thread_local { ($t:ty) => { impl $t { pub fn get(&self) {} } }; }\n",
        ),
        package_files(
            "app",
            "tested = { path = \"../tested\" }\n",
            &format!(
                "use tested::{{Tr, V}};\npub struct W;\nimpl Tr for W {{ fn get(&self) {get} }}\n\
                 thread_local! {{ static LOCAL: u8 = 1; }}\npub fn c2(v: &V) {{ v.get(); }}\npub fn c3(w: &W) {{ w.get(); }}\n\
                 pub fn c4(v: &V) {{ <V as Tr>::get(v); }}\n"
            ),
        ),
        package_files("mk", "", &format!("#[macro_export]\nmacro_rules! thread_local {{ {GET} }}\n")),
        package_files(
            "used",
            "mk = { path = \"../mk\" }\n",
            &format!("#[macro_use]\nextern crate mk;\n{HEAD}thread_local!(V);\npub fn c5(v: &V) {{ v.get(); }}\n"),
        ),
        package_files(
            "bodies",
            "",
            &format!(
                "pub struct V;\npub struct W;\npub trait Tr {{ fn get(&self); fn put(&self) {get} }}\nimpl Tr for V {{ fn get(&self) {get} }}\n\
                 macro_rules! gen {{ () => {{ pub fn get(&self) {get} }}; }}\nimpl W {{ gen!(); }}\nimpl Tr for W {{ fn get(&self) {get} }}\n\
                 pub fn helper() {get}\nmacro_rules! defs {{ () => {{ macro_rules! lazy_static {{ () => {{ pub fn helper() {get} }}; }} }}; }}\n\
                 defs!();\nmod a {{\n    use super::*;\n    lazy_static!();\n    pub fn c6() {{ helper(); }}\n}}\n\
                 pub fn c7(v: &V) {{ v.get(); }}\npub fn c8(w: &W) {{ w.get(); }}\n"
            ),
        ),
        package_files(
            "trait_body",
            "",
            "pub struct V;\nmacro_rules! more { () => { fn other(&self) {} }; }\npub trait Third { more!(); }\n\
             pub trait Tr { fn get(&self); }\nimpl Tr for V { fn get(&self) {} }\npub fn c9(v: &V) { v.get(); }\n",
        ),
    ]
    .concat();
    // cargo 1.99 runs `<V as Tr>::get` (c1, c2, c4, c7, c9), `<W as
    // Tr>::get` (c3), the `get` `mk`'s `thread_local!` writes (c5), the
    // `helper` the `lazy_static!` that `defs!` defines writes (c6), and the
    // inherent `get` of `gen!` (c8). The test-only macro of `tested`
    // counts for its own callers; `other`'s `thread_local`, which `app`
    // does not compile, changes nothing.
    let tested = Some("crates/tested/src/lib.rs#<V as Tr>::get#method");
    assert_eq!(
        c_calls(&cfg_test),
        [
            edge("c1", "v.get", None),
            edge("c2", "v.get", tested),
            edge(
                "c3",
                "w.get",
                Some("crates/app/src/lib.rs#<W as Tr>::get#method")
            ),
            edge("c4", "<V as Tr>::get", tested),
            edge("c5", "v.get", None),
            edge("c6", "helper", None),
            edge("c7", "v.get", None),
            edge("c8", "w.get", None),
            edge("c9", "v.get", None),
        ]
    );
}

// D23 (the blunt rule): a derive of a dependency is a procedural macro
// that may write any item beside its type, an inherent method among them,
// which comes before a trait's: the trait's was linked where rustc runs
// the derive's. std's derives write only their own trait's impl.
#[test]
fn c10_rust_a_derive_of_a_dependency_may_write_any_method_of_its_type() {
    let world = package_files(
        "app",
        "ext = \"1\"\n",
        "pub trait Tr { fn get(&self); fn put(&self); }\n#[derive(ext::Get)]\npub struct V;\n\
         impl Tr for V { fn get(&self) {} fn put(&self) {} }\n#[derive(Clone, Debug, PartialEq)]\npub struct X;\n\
         impl Tr for X { fn get(&self) {} fn put(&self) {} }\n#[derive(serde::Serialize)]\npub enum E { A }\n\
         impl E { pub fn get(&self) {} }\npub union U { a: u8 }\n\
         pub fn c1(v: &V) { v.get(); }\npub fn c2(v: &V) { v.put(); }\npub fn c3(x: &X) { x.get(); }\n\
         pub fn c4(v: &V) { V::get(v); }\npub fn c5(x: &X) { X::put(x); }\npub fn c6(e: &E) { e.get(); }\n\
         pub fn c7(v: &V) { <V as Tr>::put(v); }\n",
    );
    // cargo 1.99, with `ext::Get` writing `impl V { pub fn get(&self) }`,
    // runs the derive's `get` (c1, c4), `<V as Tr>::put` (c2, c7), `<X as
    // Tr>::get` (c3), `<X as Tr>::put` (c5) and `E::get` (c6).
    let x = |name: &str| Some(format!("crates/app/src/lib.rs#<X as Tr>::{name}#method"));
    assert_eq!(
        c_calls(&world),
        [
            edge("c1", "v.get", None),
            edge("c2", "v.put", None),
            edge("c3", "x.get", x("get").as_deref()),
            edge("c4", "V::get", None),
            edge("c5", "X::put", x("put").as_deref()),
            edge("c6", "e.get", None),
            edge("c7", "<V as Tr>::put", None),
        ]
    );
}

/// `depth` `cfg_attr`s nested in one another around `inner`.
fn nested_cfg_attr(depth: usize, inner: &str) -> String {
    format!(
        "#[{}{inner}{}]",
        "cfg_attr(all(), ".repeat(depth),
        ")".repeat(depth)
    )
}

// D23 (fourth review of the fix): attributes were read from their text,
// so a comment in one (`#[/* c */ pm::add_get]`), a raw identifier
// (`#[pm::r#add_get]`, `#[r#cfg_attr(…)]`, `use pm::add_get as
// r#traced;`) or a `use` naming an attribute macro `derive` hid a
// workspace attribute macro, and its probes were linked. Nested
// `cfg_attr`s cost the square of their depth.
#[test]
fn c10_rust_an_attribute_is_read_from_its_tree() {
    use super::super::{AttributeUse, UNREAD_NAME};
    let defined = facts(&extract(
        "src/lib.rs",
        &format!(
            "#[/* c */ pm::add_get]\nfn a() {{}}\n#[pm:: /* c */ r#traced]\nfn b() {{}}\n#[ // c\npm::logged]\nfn c() {{}}\n\
             #[r#cfg_attr(all(), inline, cfg_attr(feature = \"a,b\", pm::nested))]\nfn d() {{}}\n\
             #[cfg_attr(all(), /// Docs.\n pm::hidden)]\nfn e() {{}}\n#[derive(Debug)]\nstruct S;\n\
             #[inline]\n#[rustfmt::skip]\n#[doc = \"x\"]\n{}\nfn f() {{}}\n{}\nfn g() {{}}\n",
            nested_cfg_attr(64, "inline"),
            nested_cfg_attr(65, "inline"),
        ),
    ));
    let used: Vec<&str> = defined
        .attributes
        .iter()
        .map(|AttributeUse { name, .. }| name.as_str())
        .collect();
    // A doc comment in an attribute, a `#[doc]` there, and `cfg_attr`s
    // nested past `MAX_NESTING` are not read: such an attribute may be any
    // macro, once per scope. `derive` may be one a `use` names so.
    assert_eq!(
        used,
        [
            "add_get",
            "traced",
            "logged",
            "nested",
            UNREAD_NAME,
            "derive"
        ]
    );

    let pm = proc_macro_package(
        "#[proc_macro_attribute]\npub fn add_get(_: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream { item }\n",
    );
    let deps = "pm = { path = \"../pm\" }\n";
    let callers = "pub fn c1(v: &V) { v.get(); }\npub fn c2(v: &V) { V::get(v); }\n";
    let mut world = pm.to_vec();
    for (name, code) in [
        (
            "comment",
            "#[/* c */ pm::add_get]\nfn helper() {}".to_owned(),
        ),
        (
            "comment_mid",
            "#[pm:: /* c */ add_get]\nfn helper() {}".to_owned(),
        ),
        (
            "line_comment",
            "#[ // c\npm::add_get]\nfn helper() {}".to_owned(),
        ),
        ("raw", "#[pm::r#add_get]\nfn helper() {}".to_owned()),
        (
            "raw_cfg_attr",
            "#[r#cfg_attr(all(), pm::add_get)]\nfn helper() {}".to_owned(),
        ),
        (
            "raw_alias",
            "use pm::add_get as r#traced;\n#[r#traced]\nfn helper() {}".to_owned(),
        ),
        (
            "raw_source",
            "use pm::r#add_get as logged;\n#[logged]\nfn helper() {}".to_owned(),
        ),
        (
            "deep",
            format!("{}\npub struct Helper;", nested_cfg_attr(65, "pm::add_get")),
        ),
        (
            "control",
            "#[/* c */ inline]\n#[cfg_attr(all(), allow(dead_code))]\nfn helper() {}\n\
             #[derive(\n    Debug, // for logs (tests)\n    Clone,\n)]\npub struct L;"
                .to_owned(),
        ),
    ] {
        world.extend(package_files(
            name,
            deps,
            &format!("{code}\n{HEAD}{callers}"),
        ));
    }
    // cargo 1.99 runs the `get` `pm::add_get` writes in the first eight
    // packages; the last applies no macro and runs `<V as Tr>::get`.
    let control = Some("crates/control/src/lib.rs#<V as Tr>::get#method");
    assert_eq!(
        c_calls(&world),
        (0..8)
            .flat_map(|_| [edge("c1", "v.get", None), edge("c2", "V::get", None)])
            .chain([edge("c1", "v.get", control), edge("c2", "V::get", control)])
            .collect::<Vec<_>>()
    );

    // A `use` naming an attribute macro `derive` shadows std's: cargo 1.99
    // runs the `get` it writes. Every `#[derive]` of the workspace may then
    // be that macro.
    let world = [
        pm.to_vec(),
        package_files(
            "shadow",
            deps,
            &format!("use pm::add_get as derive;\n#[derive]\nfn helper() {{}}\n{HEAD}{callers}"),
        )
        .to_vec(),
    ]
    .concat();
    assert_eq!(
        c_calls(&world),
        [edge("c1", "v.get", None), edge("c2", "V::get", None)]
    );
}

// D23 (fourth review of the fix): a derive list was read from its text,
// up to its first `)`, in the first `derive(` only, `r#` kept, so a
// comment holding a `)`, a second `derive(…)` of a `cfg_attr` or a raw
// identifier hid a workspace derive, whose probes were linked, and a
// dependency's, whose type's probes were linked too. The name a
// `proc_macro_derive` gives was read from text as well.
#[test]
fn c10_rust_a_derive_list_is_read_from_its_tree() {
    use super::super::{ItemKind, ProcKind, ProcMacro, UNREAD_NAME};
    let defined = facts(&extract(
        "src/lib.rs",
        &format!(
            "#[derive(\n    Debug, // for logs (tests)\n    pm::GetOther,\n)]\npub struct A;\n\
             #[derive(Debug /* ) */, pm::r#Get)]\npub enum B {{ X }}\n\
             #[cfg_attr(all(), derive(Clone), cfg_attr(all(), derive(ext::Make)))]\npub union C {{ a: u8 }}\n\
             #[derive(Debug, <X>)]\npub struct D;\n#[cfg_attr(all(), /// Docs.\n derive(Debug))]\npub struct E;\n\
             #[proc_macro_derive(/* c */ Get, attributes(get))]\npub fn derive_get{STREAM}\n\
             #[cfg_attr(all(), proc_macro_derive(r#Make))]\npub fn derive_make{STREAM}\n\
             #[proc_macro_attribute]\npub fn r#traced{STREAM}\n#[cfg_attr(all(), /// Docs.\n proc_macro)]\npub fn odd{STREAM}\n"
        ),
    ));
    let derives: Vec<(&str, Vec<&str>)> = defined
        .items
        .iter()
        .filter_map(|item| match &item.kind {
            ItemKind::Struct { derives, .. }
            | ItemKind::Enum { derives, .. }
            | ItemKind::Union { derives } => Some((
                item.name.as_str(),
                derives.iter().map(String::as_str).collect(),
            )),
            _ => None,
        })
        .collect();
    // A part that is no path, or a list the rules cannot read, may derive
    // anything.
    assert_eq!(
        derives,
        [
            ("A", vec!["Debug", "GetOther"]),
            ("B", vec!["Debug", "Get"]),
            ("C", vec!["Clone", "Make"]),
            ("D", vec!["Debug", UNREAD_NAME]),
            ("E", vec![UNREAD_NAME]),
        ]
    );
    let proc = |kind, name: &str| ProcMacro {
        kind,
        name: name.to_owned(),
    };
    // An attribute the rules cannot read on a function may define a macro
    // of each kind, a derive of any name.
    assert_eq!(
        defined.proc_macros,
        [
            proc(ProcKind::Derive, "Get"),
            proc(ProcKind::Derive, "Make"),
            proc(ProcKind::Attribute, "traced"),
            proc(ProcKind::Bang, "odd"),
            proc(ProcKind::Attribute, "odd"),
            proc(ProcKind::Derive, UNREAD_NAME),
        ]
    );

    let pm = proc_macro_package(&format!(
        "#[proc_macro_derive(GetOther)]\npub fn derive_get_other{STREAM}\n"
    ));
    let deps = "pm = { path = \"../pm\" }\next = \"1\"\n";
    let callers = "pub fn c1(v: &V) { v.get(); }\npub fn c2(v: &V) { V::get(v); }\n";
    let mut world = pm.to_vec();
    for (name, code) in [
        (
            "comment",
            "#[derive(\n    Debug, // for logs (tests)\n    pm::GetOther,\n)]\npub struct L;",
        ),
        (
            "block_comment",
            "#[derive(Debug /* ) */, pm::GetOther)]\npub struct L;",
        ),
        (
            "cfg_attr",
            "#[cfg_attr(all(), derive(Debug), derive(pm::GetOther))]\npub struct L;",
        ),
        (
            "nested",
            "#[cfg_attr(all(), cfg_attr(all(), derive(Debug), derive(pm::GetOther)))]\npub struct L;",
        ),
        ("raw", "#[derive(pm::r#GetOther)]\npub struct L;"),
        (
            "raw_source",
            "use pm::r#GetOther as Go;\n#[derive(Go)]\npub struct L;",
        ),
        (
            "control",
            "#[derive(\n    Debug, // for logs (tests)\n    Clone,\n)]\npub struct L;",
        ),
    ] {
        world.extend(package_files(
            name,
            deps,
            &format!("pub type Other = V;\n{code}\n{HEAD}{callers}"),
        ));
    }
    world.extend(package_files(
        "dependency",
        deps,
        "pub trait Tr { fn get(&self); }\n#[cfg_attr(all(), derive(Debug), derive(ext::Make))]\npub struct W;\n\
         impl Tr for W { fn get(&self) {} }\npub fn c3(w: &W) { w.get(); }\npub fn c4(w: &W) { W::get(w); }\n",
    ));
    // cargo 1.99 runs the `get` `pm::GetOther` writes on `Other` in the
    // first six packages, and the one `ext::Make` writes on `W` in c3 and
    // c4; std's derives write none: the control runs `<V as Tr>::get`.
    let control = Some("crates/control/src/lib.rs#<V as Tr>::get#method");
    assert_eq!(
        c_calls(&world),
        (0..6)
            .flat_map(|_| [edge("c1", "v.get", None), edge("c2", "V::get", None)])
            .chain([edge("c1", "v.get", control), edge("c2", "V::get", control)])
            .chain([edge("c3", "w.get", None), edge("c4", "W::get", None)])
            .collect::<Vec<_>>()
    );
}

// derive-default-cfg-attr (final review of the D23 fix): every derive a
// cfg_attr applies is read now, its predicate unread, and `T::default()`
// was typed `T` whenever the list held `Default`. Where the predicate is
// false, `default` is another trait's (a blanket impl here), and the
// methods of `T` were linked on what it returns.
#[test]
fn c10_rust_only_a_plain_derive_of_default_types_a_constructor() {
    let files = [
        (
            "Cargo.toml",
            "[package]\nname = \"app\"\n[features]\nextra = []\n",
        ),
        (
            "src/lib.rs",
            "pub struct Other;\nimpl Other { pub fn get(&self) {} }\n\
             mod mk {\n    pub trait Mk { fn default() -> super::Other; }\n    impl<T> Mk for T { fn default() -> super::Other { super::Other } }\n}\n\
             #[cfg(not(feature = \"extra\"))]\nuse mk::Mk;\n\
             #[cfg_attr(feature = \"extra\", derive(Clone), derive(Default))]\npub struct V;\nimpl V { pub fn get(&self) {} }\n\
             #[cfg_attr(feature = \"extra\", derive(Clone /* ) */, r#Default))]\npub struct W;\nimpl W { pub fn get(&self) {} }\n\
             #[cfg_attr(feature = \"extra\", derive(Default))]\npub struct X;\nimpl X { pub fn get(&self) {} }\n\
             pub fn c1() { let v = V::default(); v.get(); }\npub fn c2() { W::default().get(); }\n\
             pub fn c3() { X::default().get(); }\n\
             pub mod plain {\n    #[derive(Clone, Default)]\n    pub struct P;\n    impl P { pub fn get(&self) {} }\n    \
             pub fn c4() { let p = P::default(); p.get(); }\n}\n",
        ),
    ];
    // cargo 1.99 runs `Other::get` in c1 to c3 without `extra`, and
    // `P::get` in c4: a plain derive applies in every build, and no
    // other `default` is in scope beside it (rustc would reject the call
    // as ambiguous, E0034).
    let method_calls: Vec<_> = calls(&files)
        .into_iter()
        .filter(|(_, target, _)| target.ends_with(".get"))
        .collect();
    assert_eq!(
        method_calls,
        [
            edge("c1", "v.get", None),
            edge("c2", "W::default().get", None),
            edge("c3", "X::default().get", None),
            edge("c4", "p.get", Some("src/lib.rs#P::get#method")),
        ]
    );
}
