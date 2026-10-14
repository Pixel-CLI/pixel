// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! A local bound by what `?`, `.unwrap()` or `.expect(…)` takes out of a
//! call to a type's associated function (pixel-mvp#108): typed when the
//! function returns std's `Result` or `Option` of the type, written
//! directly or through an alias of the workspace; unresolved when the
//! rules cannot prove it (C-10).

use super::*;

const APP: &str = "[package]\nname = \"app\"\n";

/// The method calls of `files` (a `.` in the target), as [`edges_of`].
fn method_calls(files: &[(&str, &str)]) -> Vec<(String, String, Option<String>)> {
    calls(files)
        .into_iter()
        .filter(|(_, target, _)| target.contains('.') && !target.ends_with(".unwrap"))
        .filter(|(_, target, _)| !target.ends_with(".expect"))
        .collect()
}

const GET: Option<&str> = Some("src/lib.rs#Store::get#method");

// Pixel-CLI/pixel @ 85bede7d: `let idx = ResolveIndex::build(store)?;
// idx.decide_from(…)` and `let verifier = Verifier::new(p)?;
// verifier.search_file(…)` were unresolved, v1 linked them: the type is
// the `Ok` of what the function returns.
#[test]
fn c10_rust_unwrapping_a_result_or_option_of_self_types_the_local() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub struct Error;\npub struct Store;\nimpl Store {\n    \
             pub fn build(p: &str) -> Result<Self, Error> { Ok(Store) }\n    \
             pub fn find(p: &str) -> Option<Store> { None }\n    \
             pub fn full(p: &str) -> std::result::Result<crate::Store, Error> { Ok(Store) }\n    \
             pub fn rooted(p: &str) -> ::core::option::Option<Self> { None }\n    \
             pub fn get(&self) {}\n}\n\
             pub fn a() -> Result<(), Error> { let s = Store::build(\"x\")?; s.get(); Ok(()) }\n\
             pub fn b() { let s = Store::build(\"x\").unwrap(); s.get(); }\n\
             pub fn c() { let s = Store::find(\"x\").expect(\"found\"); s.get(); }\n\
             pub fn d() -> Option<()> { Store::find(\"x\")?.get(); Some(()) }\n\
             pub fn e() -> Result<(), Error> { let s = Store::full(\"x\")?; s.get(); Ok(()) }\n\
             pub fn f() -> Option<()> { let s = Store::rooted(\"x\")?; s.get(); Some(()) }\n",
        ),
    ];
    assert_eq!(
        method_calls(&files),
        [
            edge("a", "s.get", GET),
            edge("b", "s.get", GET),
            edge("c", "s.get", GET),
            edge("d", "Store::find(\"x\")?.get", GET),
            edge("e", "s.get", GET),
            edge("f", "s.get", GET),
        ]
    );
}

// Most crates write `Result<Self>` through their own alias of std's
// `Result` (`pub type Result<T> = std::result::Result<T, Error>;`, nine of
// them in Pixel-CLI/pixel): the alias is expanded, the argument written on
// it taking its parameter's place.
#[test]
fn c10_rust_unwrapping_through_a_crate_alias_of_result_types_the_local() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub mod error;\npub mod store;\npub mod other;\n",
        ),
        (
            "src/error.rs",
            "pub struct Error;\npub type Result<T> = std::result::Result<T, Error>;\n\
             pub type Defaulted<T, E = Error> = core::result::Result<T, E>;\n",
        ),
        (
            "src/store.rs",
            "use crate::error::Result;\npub struct Store;\nimpl Store {\n    \
             pub fn open() -> Result<Self> { todo!() }\n    \
             pub fn reopen() -> crate::error::Defaulted<Store> { todo!() }\n    \
             pub fn get(&self) {}\n}\n",
        ),
        (
            "src/other.rs",
            "use crate::store::Store;\n\
             pub fn a() -> crate::error::Result<()> { let s = Store::open()?; s.get(); Ok(()) }\n\
             pub fn b() { let s = Store::reopen().unwrap(); s.get(); }\n",
        ),
    ];
    let get = Some("src/store.rs#Store::get#method");
    assert_eq!(
        method_calls(&files),
        [edge("a", "s.get", get), edge("b", "s.get", get)]
    );
}

// What the rules cannot prove is a `Result` or an `Option` of the type
// leaves the local untyped: a dependency's alias (anyhow's `Result`,
// which the rules do not read), std's own `io::Result` (not read either),
// a type of the workspace named `Result` and an alias of it, an alias of
// an alias, a function returning the type itself or a derived `default()`
// (their own `unwrap` is another method), a `Result` of a reference, and
// the value before it is unwrapped.
#[test]
fn c10_rust_unwrapping_what_is_not_proven_a_result_of_the_type_is_unresolved() {
    let files = [
        (
            "Cargo.toml",
            "[package]\nname = \"app\"\n[dependencies]\nanyhow = \"1\"\n",
        ),
        (
            "src/lib.rs",
            "pub mod own {\n    pub enum Result<T, E> { Ok(T), Err(E) }\n    \
             impl<T, E> Result<T, E> { pub fn unwrap(self) -> crate::Other { crate::Other } }\n}\n\
             pub type Res<T> = std::result::Result<T, ()>;\npub type Again<T> = Res<T>;\n\
             pub type Mine<T> = own::Result<T, ()>;\n\
             #[derive(Default)]\npub struct Conf;\nimpl Conf { pub fn unwrap(self) -> Other { Other } pub fn get(&self) {} }\n\
             pub struct Other;\nimpl Other { pub fn get(&self) {} }\n\
             pub struct Store;\nimpl Store {\n    \
             pub fn dep() -> anyhow::Result<Self> { todo!() }\n    \
             pub fn io() -> std::io::Result<Self> { todo!() }\n    \
             pub fn own() -> own::Result<Self, ()> { todo!() }\n    \
             pub fn again() -> Again<Self> { todo!() }\n    \
             pub fn mine() -> Mine<Self> { todo!() }\n    \
             pub fn plain() -> Store { Store }\n    \
             pub fn unwrap(self) -> Other { Other }\n    \
             pub fn borrowed(&self) -> Result<&Self, ()> { Ok(self) }\n    \
             pub fn wrapped() -> Result<Self, ()> { Ok(Store) }\n    \
             pub fn get(&self) {}\n}\n\
             pub fn a() -> anyhow::Result<()> { let s = Store::dep()?; s.get(); Ok(()) }\n\
             pub fn b() -> std::io::Result<()> { let s = Store::io()?; s.get(); Ok(()) }\n\
             pub fn c() { let s = Store::own().unwrap(); s.get(); }\n\
             pub fn d() { let s = Store::again().unwrap(); s.get(); }\n\
             pub fn e() { let s = Store::plain().unwrap(); s.get(); }\n\
             pub fn f() { let s = Store::wrapped(); s.is_ok(); }\n\
             pub fn g() { let s = Store::borrowed(&Store).unwrap(); s.get(); }\n\
             pub fn h() { let s = Store::mine().unwrap(); s.get(); }\n\
             pub fn i() { let c = Conf::default().unwrap(); c.get(); }\n",
        ),
    ];
    assert_eq!(
        method_calls(&files),
        [
            edge("a", "s.get", None),
            edge("b", "s.get", None),
            edge("c", "s.get", None),
            edge("d", "s.get", None),
            edge("e", "s.get", None),
            edge("f", "s.is_ok", None),
            edge("g", "s.get", None),
            edge("h", "s.get", None),
            edge("i", "c.get", None),
        ]
    );
}

// A `Result` whose name a dependency's binding takes is that dependency's
// type, not std's; so is an `Option` of the workspace, and a path through a
// module of the workspace named `std`.
#[test]
fn c10_rust_unwrapping_a_result_a_binding_rebinds_is_unresolved() {
    let files = [
        (
            "Cargo.toml",
            "[package]\nname = \"app\"\n[dependencies]\nanyhow = \"1\"\n",
        ),
        (
            "src/lib.rs",
            "pub mod a {\n    use anyhow::{Error, Result};\n    pub struct Store;\n    impl Store {\n        \
             pub fn open() -> Result<Self, Error> { todo!() }\n        pub fn get(&self) {}\n    }\n    \
             pub fn f() -> Result<()> { let s = Store::open()?; s.get(); Ok(()) }\n}\n\
             pub mod b {\n    pub struct Option<T>(T);\n    pub struct Store;\n    impl Store {\n        \
             pub fn open() -> Option<Self> { todo!() }\n        pub fn get(&self) {}\n    }\n    \
             pub fn f() { let s = Store::open().unwrap(); s.get(); }\n}\n\
             pub mod c {\n    pub mod std { pub mod result { pub struct Result<T, E>(T, E); } }\n    \
             pub struct Store;\n    impl Store {\n        \
             pub fn open() -> std::result::Result<Self, ()> { todo!() }\n        pub fn get(&self) {}\n    }\n    \
             pub fn f() { let s = Store::open().unwrap(); s.get(); }\n}\n",
        ),
    ];
    assert_eq!(
        method_calls(&files),
        [
            edge("f", "s.get", None),
            edge("f", "s.get", None),
            edge("f", "s.get", None),
        ]
    );
}

// An `async fn` returns a future of its written return type: a call to
// one is not the type, whatever it returns.
#[test]
fn c10_rust_an_async_constructor_does_not_type_the_local() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub struct Store;\nimpl Store {\n    pub async fn new() -> Self { Store }\n    \
             pub fn make() -> Self { Store }\n    pub fn get(&self) {}\n}\n\
             pub fn f() { let s = Store::new(); s.get(); let t = Store::make(); t.get(); }\n",
        ),
    ];
    assert_eq!(
        method_calls(&files),
        [edge("f", "s.get", None), edge("f", "t.get", GET)]
    );
}
