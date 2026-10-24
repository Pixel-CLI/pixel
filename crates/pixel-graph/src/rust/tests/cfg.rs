// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `#[cfg]`: what a predicate makes absent, and when an item under one
//! may be the target of a call that does not carry it (F2). Every
//! expectation was checked with rustc 1.99, without and with
//! `--cfg 'feature="x"'`, on unix.

use super::*;

const APP: &str = "[package]\nname = \"app\"\n";

// F2 (Astra's `cfg_inherent`): an inherent method under `cfg(any())` was
// linked, though it never exists and the trait method runs.
#[test]
fn c10_rust_cfg_an_item_under_a_false_predicate_is_absent() {
    let files = [
        ("Cargo.toml", "[package]\nname = \"probe\"\n"),
        (
            "src/main.rs",
            "struct S;\nimpl S {\n    #[cfg(any())]\n    fn tag(&self) -> &'static str { \"inactive inherent\" }\n}\n\
             trait Tag { fn tag(&self) -> &'static str; }\nimpl Tag for S {\n    fn tag(&self) -> &'static str { \"active trait\" }\n}\n\
             fn run(s: S) -> &'static str { s.tag() }\nfn main() { println!(\"{}\", run(S)); }\n\
             trait Other { fn other(&self) {} }\nimpl Other for S {}\n#[cfg(not(all()))]\nimpl S { fn other(&self) {} }\n\
             mod m { pub fn helper() {} }\nuse m::*;\n#[cfg(false)]\nfn helper() {}\n#[cfg(all(unix, any()))]\nmod gone { pub fn helper() {} }\n\
             fn more(s: S) { s.other(); helper(); }\n",
        ),
    ];
    assert_eq!(
        calls(&files),
        [
            edge("run", "s.tag", Some("src/main.rs#<S as Tag>::tag#method")),
            edge("more", "s.other", Some("src/main.rs#Other::other#method")),
            edge("more", "helper", Some("src/main.rs#helper#function")),
        ]
    );
}

// F2: a method only one configuration has, competing with one that every
// configuration has, is unresolved unless the caller is under the same
// predicate.
#[test]
fn c10_rust_cfg_a_conditional_method_with_a_competitor_is_unresolved() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub struct S;\npub trait Tag {\n    fn tag(&self) -> u8;\n}\nimpl Tag for S {\n    fn tag(&self) -> u8 { 1 }\n}\n\
             impl S {\n    #[cfg(feature = \"x\")]\n    pub fn tag(&self) -> u8 { 2 }\n    #[cfg(feature = \"x\")]\n    pub fn only(&self) -> u8 { 3 }\n}\n\
             pub fn plain(s: &S) {\n    s.tag();\n    s.only();\n}\n#[cfg(feature = \"x\")]\npub fn gated(s: &S) {\n    s.tag();\n}\n\
             pub trait ExtTag {\n    fn ext_tag(&self) {}\n}\nimpl ExtTag for S {}\n#[cfg(feature = \"x\")]\nmod ext;\n\
             pub fn outside(s: &S) {\n    s.ext_tag();\n}\n",
        ),
        (
            "src/ext.rs",
            "impl super::S {\n    pub fn ext_tag(&self) {}\n}\npub fn inside(s: &super::S) {\n    s.ext_tag();\n}\n",
        ),
    ];
    // `s.only()` compiles only where `x` holds, and is that method there.
    // The inherent method of `ext.rs` exists only where `mod ext;` does.
    assert_eq!(
        calls(&files),
        [
            edge("plain", "s.tag", None),
            edge("plain", "s.only", Some("src/lib.rs#S::only#method")),
            edge("gated", "s.tag", Some("src/lib.rs#S::tag#method")),
            edge("outside", "s.ext_tag", None),
            edge("inside", "s.ext_tag", Some("src/ext.rs#S::ext_tag#method")),
        ]
    );
}

// F2: items and `use`s of one name under different predicates are
// ambiguous, and stay so where the caller carries one of them (its item, an
// enclosing module, a file module's `mod` or the statement itself): a
// macro the rules do not read may write the name between the site and
// them (cfg-pair-attribute-macro-fn, final review of the F2 fix).
#[test]
fn c10_rust_cfg_variants_are_ambiguous_whatever_the_caller_carries() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "#[cfg(unix)]\npub fn sys() {}\n#[cfg(windows)]\npub fn sys() {}\nmod a { pub fn pick() {} }\nmod b { pub fn pick() {} }\n\
             #[cfg(unix)]\nuse a::pick;\n#[cfg(windows)]\nuse b::pick;\npub fn any_os() {\n    sys();\n    pick();\n}\n\
             #[cfg(unix)]\npub fn on_unix() {\n    sys();\n    pick();\n}\n#[cfg(unix)]\nmod unix_only {\n    pub fn g() { super::sys(); }\n}\n\
             pub fn statement() {\n    #[cfg(unix)]\n    sys();\n}\n#[cfg(unix)]\nmod imp;\n",
        ),
        ("src/imp.rs", "pub fn h() {\n    super::sys();\n}\n"),
    ];
    assert_eq!(
        calls(&files),
        [
            edge("any_os", "sys", None),
            edge("any_os", "pick", None),
            edge("on_unix", "sys", None),
            edge("on_unix", "pick", None),
            edge("g", "super::sys", None),
            edge("statement", "sys", None),
            edge("h", "super::sys", None),
        ]
    );
}

// cfg-pair-block-macro-fn (final review of the F2 fix): a macro invoked
// where a statement goes may define a function in its block, which the
// rules do not read, and that shadows the module's: rustix's `syscall!`
// writes `fn fchmod` inside `fchmod`, whose call was linked to `fchmod`
// itself, picked by the caller's `#[cfg]` among two (main abstains: two
// candidates). `#[cfg]`s pick no binding among several, along a `use`
// too, whether a block around the site invokes a macro or not.
#[test]
fn c10_rust_cfg_variants_are_not_told_apart_where_a_block_invokes_a_macro() {
    let files = [
        (
            "Cargo.toml",
            "[package]\nname = \"app\"\n[features]\nalt = []\n",
        ),
        (
            "src/lib.rs",
            "macro_rules! local_fn { ($n:ident) => { fn $n() {} }; }\n\
             #[cfg(not(feature = \"alt\"))]\nfn work() {}\n#[cfg(feature = \"alt\")]\nfn work() {}\n\
             mod m {\n    #[cfg(not(feature = \"alt\"))]\n    pub fn job() {}\n    #[cfg(feature = \"alt\")]\n    pub fn job() {}\n}\n\
             use m::job;\nfn single() {}\n\
             #[cfg(not(feature = \"alt\"))]\npub fn c1() { local_fn!(work); work(); }\n\
             #[cfg(not(feature = \"alt\"))]\npub fn c2() { work(); }\n\
             #[cfg(not(feature = \"alt\"))]\npub fn c3() { if true { local_fn!(job); job(); } job(); }\n\
             #[cfg(not(feature = \"alt\"))]\npub fn c4() { println!(\"x\"); work(); }\n\
             pub fn c5() { println!(\"x\"); single(); }\n",
        ),
    ];
    // cargo 1.99 runs the macro's `work` (c1), the module's (c2, c4), the
    // macro's `job` then `m::job` (c3) and `single` (c5). A name one
    // binding gives is still that binding (c5); two are ambiguous without
    // a macro too (c2, cfg-pair-attribute-macro-fn).
    assert_eq!(
        calls(&files),
        [
            edge("c1", "work", None),
            edge("c2", "work", None),
            edge("c3", "job", None),
            edge("c3", "job", None),
            edge("c4", "work", None),
            edge("c5", "single", Some("src/lib.rs#single#function")),
        ]
    );
}

// cfg-pair-attribute-macro-fn (final review of the F2 fix, round 2): an
// attribute macro on the caller, one on an item of its body, a derive
// there, or one beside a glob writes `fn work`, which rustc then calls
// over the one the caller's `#[cfg]`s picked among two (cargo 1.99, a
// procedural macro `pm` writing it): the rows do not show it, and the two
// stay ambiguous, through a glob and along a path too. An enum's variants
// do as well.
#[test]
fn c10_rust_cfg_variants_are_ambiguous_where_a_macro_may_write_the_name() {
    let files = [
        (
            "Cargo.toml",
            "[package]\nname = \"app\"\n[features]\nalt = []\n",
        ),
        (
            "src/lib.rs",
            "#[cfg(not(feature = \"alt\"))]\npub fn work() {}\n#[cfg(feature = \"alt\")]\npub fn work() {}\n\
             #[cfg(not(feature = \"alt\"))]\npub fn c1() {\n    #[pm::inject_item]\n    fn helper() {}\n    work();\n}\n\
             #[cfg(not(feature = \"alt\"))]\npub fn c2() {\n    #[derive(pm::MkWork)]\n    struct Z;\n    work();\n}\n\
             #[cfg(not(feature = \"alt\"))]\n#[pm::inject_body]\npub fn c3() {\n    work();\n}\n\
             mod m4 {\n    use super::*;\n    #[derive(pm::MkWork)]\n    struct Z;\n    \
             #[cfg(not(feature = \"alt\"))]\n    pub fn c4() {\n        work();\n    }\n}\n\
             mod mid {\n    pub use super::*;\n    #[derive(pm::MkWorkPub)]\n    pub struct Z;\n}\n\
             #[cfg(not(feature = \"alt\"))]\npub fn c5() {\n    mid::work();\n}\n\
             pub enum E {\n    #[cfg(not(feature = \"alt\"))]\n    V(u8),\n    #[cfg(feature = \"alt\")]\n    V(u16),\n}\n\
             #[cfg(not(feature = \"alt\"))]\npub fn c6() -> E {\n    E::V(1)\n}\n",
        ),
    ];
    assert_eq!(
        calls(&files),
        [
            edge("c1", "work", None),
            edge("c2", "work", None),
            edge("c3", "work", None),
            edge("c4", "work", None),
            edge("c5", "mid::work", None),
            edge("c6", "E::V", None),
        ]
    );
}

// method-probe-cfg-pick-macro (final review of the F2 fix, round 3): the
// probe's steps picked among methods under exclusive `#[cfg]`s by the
// caller's. A macro the rules do not read writes a method rustc takes
// first: `by_value!` a trait whose `work(self)` comes before `V`'s
// `work(&self)`; an inherent `work` of `W` written in any body would come
// before `Tw`'s, everywhere (an impl is never scoped). Main sees two
// candidates and abstains, and so does the probe now, on a value, a
// reference, a path through the type and `dyn`. A path naming the trait
// still picks: no macro adds an impl of `Tw` for `W` or an item to `Tw`
// (cargo 1.99, without and with `--features alt`).
#[test]
fn c10_rust_cfg_methods_are_ambiguous_whatever_the_caller_carries() {
    let files = [
        (
            "Cargo.toml",
            "[package]\nname = \"app\"\n[features]\nalt = []\n",
        ),
        (
            "src/lib.rs",
            "pub struct V;\n#[cfg(not(feature = \"alt\"))]\nimpl V {\n    pub fn work(&self) {}\n}\n\
             #[cfg(feature = \"alt\")]\nimpl V {\n    pub fn work(&self) {}\n}\n\
             pub trait Tw {\n    fn work(&self);\n}\npub struct W;\n\
             #[cfg(not(feature = \"alt\"))]\nimpl Tw for W {\n    fn work(&self) {}\n}\n\
             #[cfg(feature = \"alt\")]\nimpl Tw for W {\n    fn work(&self) {}\n}\n\
             pub trait Dy {\n    #[cfg(not(feature = \"alt\"))]\n    fn work(&self) {}\n    \
             #[cfg(feature = \"alt\")]\n    fn work(&self) {}\n}\n\
             macro_rules! by_value {\n    () => {\n        trait L {\n            fn work(self);\n        }\n        \
             impl L for crate::V {\n            fn work(self) {}\n        }\n    };\n}\n\
             #[cfg(not(feature = \"alt\"))]\npub fn c1() {\n    by_value!();\n    let v: V = V;\n    v.work();\n}\n\
             #[cfg(not(feature = \"alt\"))]\npub fn c2(v: &V, w: &W, d: &dyn Dy) {\n    v.work();\n    V::work(v);\n    \
             w.work();\n    W::work(w);\n    d.work();\n}\n\
             #[cfg(not(feature = \"alt\"))]\npub fn c3(w: &W) {\n    <W as Tw>::work(w);\n    Tw::work(w);\n}\n",
        ),
    ];
    let tw = Some("src/lib.rs#<W as Tw>::work#method");
    assert_eq!(
        calls(&files),
        [
            edge("c1", "v.work", None),
            edge("c2", "v.work", None),
            edge("c2", "V::work", None),
            edge("c2", "w.work", None),
            edge("c2", "W::work", None),
            edge("c2", "d.work", None),
            edge("c3", "<W as Tw>::work", tw),
            edge("c3", "Tw::work", tw),
        ]
    );
}

// told-glob-reask-quadratic (final review of the F2 fix, round 3): a glob
// stage the caller's `#[cfg]`s tell apart deep in a chain of globs answers
// what it answers where `#[cfg]`s are not read, or nothing, as it did when
// each stage asked again; the walk's first stage now asks once. `a1`'s
// gated glob brings the `work` its other glob does, `b1`'s another `job`
// (cargo 1.99, without and with `--features x`).
#[test]
fn c10_rust_cfg_a_glob_deep_in_a_chain_is_told_apart_as_a_near_one() {
    let files = [
        (
            "Cargo.toml",
            "[package]\nname = \"app\"\n[features]\nx = []\n",
        ),
        (
            "src/lib.rs",
            "mod a0 {\n    pub use crate::a1::*;\n}\nmod a1 {\n    pub use crate::a2::*;\n    \
             #[cfg(feature = \"x\")]\n    pub use crate::same::*;\n}\nmod a2 {\n    pub fn work() {}\n}\n\
             mod same {\n    pub use crate::a2::work;\n}\nmod b0 {\n    pub use crate::b1::*;\n}\n\
             mod b1 {\n    pub use crate::b2::*;\n    #[cfg(feature = \"x\")]\n    pub use crate::other::*;\n}\n\
             mod b2 {\n    pub fn job() {}\n}\nmod other {\n    pub fn job() {}\n}\nuse a0::*;\n\
             pub fn c1() {\n    work();\n    a0::work();\n}\n\
             #[cfg(not(feature = \"x\"))]\npub fn c2() {\n    b0::job();\n}\n\
             #[cfg(not(feature = \"x\"))]\npub fn c3() {\n    use crate::b0::*;\n    job();\n}\n",
        ),
    ];
    let work = Some("src/lib.rs#work#function");
    assert_eq!(
        calls(&files),
        [
            edge("c1", "work", work),
            edge("c1", "a0::work", work),
            edge("c2", "b0::job", None),
            edge("c3", "job", None),
        ]
    );
}

// F2: a name only one configuration defines is unresolved where another
// candidate (a glob's, the prelude's) takes its place in the others, and
// resolved where nothing does.
#[test]
fn c10_rust_cfg_a_conditional_name_with_an_alternative_is_unresolved() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "mod other {\n    pub fn helper() {}\n}\nuse other::*;\n#[cfg(feature = \"x\")]\nfn helper() {}\n\
             #[cfg(feature = \"x\")]\nfn drop<T>(_: T) {}\n#[cfg(feature = \"x\")]\nfn alone() {}\n\
             pub fn f() {\n    helper();\n    drop(1);\n    alone();\n}\n",
        ),
    ];
    assert_eq!(
        calls(&files),
        [
            edge("f", "helper", None),
            edge("f", "drop", None),
            edge("f", "alone", Some("src/lib.rs#alone#function")),
        ]
    );
}

// F2 (review of the fix): a function only one configuration defines, named
// like a value of the std prelude (`size_of` and the three others
// `std::mem` added in 1.80), was linked, though without the feature rustc
// calls std's (it printed `4` and `(8, 1, 100)`, with the feature `100`).
#[test]
fn c10_rust_cfg_a_conditional_function_named_like_a_prelude_value_is_unresolved() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/main.rs",
            "#[cfg(feature = \"x\")]\nfn size_of<T>() -> usize { 100 }\nfn run() -> usize { size_of::<u32>() }\n\
             #[cfg(feature = \"x\")]\nfn gated() -> usize { size_of::<u32>() }\nmod m {\n    #[cfg(feature = \"x\")]\n    \
             pub fn align_of<T>() -> usize { 100 }\n    #[cfg(feature = \"x\")]\n    pub fn size_of_val<T>(_t: &T) -> usize { 100 }\n    \
             pub fn align_of_val<T>(_t: &T) -> usize { 100 }\n    pub fn run() -> (usize, usize, usize) {\n        \
             (align_of::<u64>(), size_of_val(&1u8), align_of_val(&1u8))\n    }\n}\n",
        ),
    ];
    assert_eq!(
        calls(&files),
        [
            edge("run", "size_of", None),
            edge("gated", "size_of", Some("src/main.rs#size_of#function")),
            edge("run", "align_of", None),
            edge("run", "size_of_val", None),
            edge(
                "run",
                "align_of_val",
                Some("src/main.rs#align_of_val#function")
            ),
        ]
    );
}

// F2 (fourth review of the D23 fix): a predicate was read from its text,
// so a comment in it gave tokens rustc never sees: `all(unix /* ,false,
// */)` was false, and the inherent method under it, which rustc runs on
// unix, was left out for the trait's. A raw `r#cfg` was no `cfg`, and
// the method under `r#cfg(any())`, which never exists, was linked.
#[test]
fn c10_rust_cfg_a_predicate_its_text_would_misread_is_unknown() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub struct S;\npub trait Tag {\n    fn tag(&self) -> u8;\n    fn other(&self) -> u8 { 1 }\n}\nimpl Tag for S {\n    fn tag(&self) -> u8 { 1 }\n}\n\
             #[cfg(all(unix /* ,false, */))]\nimpl S {\n    pub fn tag(&self) -> u8 { 2 }\n}\n\
             #[r#cfg(any())]\nimpl S {\n    pub fn other(&self) -> u8 { 2 }\n}\n\
             #[cfg(any(\n    unix, // the one tested\n    windows,\n))]\npub fn here(s: &S) {\n    s.tag();\n}\n\
             pub fn plain(s: &S) {\n    s.tag();\n    s.other();\n}\n",
        ),
    ];
    // rustc 1.99 on unix runs `S::tag` and `<S as Tag>::other`: either
    // impl may or may not exist as the rules read it. The comment of the
    // last predicate hides nothing either way.
    assert_eq!(
        calls(&files),
        [
            edge("here", "s.tag", None),
            edge("plain", "s.tag", None),
            edge("plain", "s.other", None),
        ]
    );
    let items = facts(&extract("src/lib.rs", files[1].1)).items;
    let here = items.iter().find(|item| item.name == "here").expect("here");
    assert_eq!(here.cfg, ["cfg(any(unix,//theonetestedwindows,))"]);
}

#[test]
fn the_conjuncts_of_a_site_come_from_every_attribute_around_it() {
    let x = extract(
        "src/lib.rs",
        "#![cfg(feature = \"a\")]\n#[cfg(all(unix, not(test)))]\nmod m {\n    #![cfg(feature = \"b\")]\n    #[cfg(test)]\n    pub fn f(x: u8) {\n        #[cfg(feature = \"c\")]\n        g();\n        match x {\n            #[cfg(feature = \"d\")]\n            1 => h(),\n            _ => k(),\n        }\n    }\n}\n",
    );
    let file = ["feature=\"a\""];
    let module = ["feature=\"b\"", "not(test)", "unix"];
    let site = |more: &[&str]| -> Vec<String> {
        let mut all: Vec<String> = file
            .iter()
            .chain(&module)
            .chain(&["test"])
            .chain(more)
            .map(ToString::to_string)
            .collect();
        all.sort();
        all
    };
    let conjuncts: Vec<Vec<String>> = contexts(&x).into_iter().map(|c| c.cfg).collect();
    assert_eq!(
        conjuncts,
        [
            site(&["feature=\"c\""]),
            site(&["feature=\"d\""]),
            site(&[])
        ]
    );
    let items = facts(&x).items;
    assert_eq!((items[1].name.as_str(), &items[1].cfg), ("f", &site(&[])));
    // The module's own item carries its outer attribute, not its inner one.
    let mut m = vec![
        "feature=\"a\"".to_owned(),
        "not(test)".to_owned(),
        "unix".to_owned(),
    ];
    m.sort();
    assert_eq!((items[0].name.as_str(), &items[0].cfg), ("m", &m));
}
