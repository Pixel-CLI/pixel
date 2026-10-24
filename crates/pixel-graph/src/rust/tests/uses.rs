// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `use` leaves: one import site and one binding per path a `use` names
//! (v1's U-1 to U-8, port spec 15.2).

use super::super::{Leaf, ScopeKind, Use, Vis};
use super::*;

fn path(text: &str) -> Vec<String> {
    text.split("::").map(str::to_owned).collect()
}

fn leaf(scope: u32, at: &str, kind: Leaf, local: Option<&str>) -> Use {
    Use {
        scope,
        vis: Vis::Private,
        path: path(at),
        leaf: kind,
        local: local.map(str::to_owned),
        cfg: Vec::new(),
    }
}

#[test]
fn a_use_binds_the_item_names_it_brings_into_scope() {
    let source = [
        "use crate::push::push;",
        "use crate::push::{PushOptions, push as leased};",
        "use crate::a::{b::{c, d}, e, self};",
        "use std::collections::*;",
        "use super::Walker;",
        "use a::{self as b, Tr as _};",
    ]
    .join("\n");
    let x = extract("src/ship.rs", &source);
    let targets: Vec<String> = sites(&x).into_iter().map(|s| s.2).collect();
    assert_eq!(
        targets,
        [
            "crate::push::push",
            "crate::push::PushOptions",
            "crate::push::push",
            "crate::a::b::c",
            "crate::a::b::d",
            "crate::a::e",
            "crate::a::self",
            "std::collections::*",
            "super::Walker",
            "a::self",
            "a::Tr",
        ]
    );
    // v1 bound nothing for `self` in a group; decision 1 binds the module.
    // Only the alias is in scope: `leased()` calls `push`, `push()` does not.
    assert_eq!(
        facts(&x).uses,
        [
            leaf(0, "crate::push::push", Leaf::Name, Some("push")),
            leaf(
                0,
                "crate::push::PushOptions",
                Leaf::Name,
                Some("PushOptions")
            ),
            leaf(0, "crate::push::push", Leaf::Name, Some("leased")),
            leaf(0, "crate::a::b::c", Leaf::Name, Some("c")),
            leaf(0, "crate::a::b::d", Leaf::Name, Some("d")),
            leaf(0, "crate::a::e", Leaf::Name, Some("e")),
            leaf(0, "crate::a", Leaf::SelfModule, Some("a")),
            leaf(0, "std::collections", Leaf::Glob, None),
            leaf(0, "super::Walker", Leaf::Name, Some("Walker")),
            leaf(0, "a", Leaf::SelfModule, Some("b")),
            leaf(0, "a::Tr", Leaf::Name, None),
        ]
    );
}

#[test]
fn a_use_binds_in_its_module_or_block() {
    let source = [
        "use crate::left::push;", // 1: the file
        "mod a {",                // 2
        "    use crate::right::publish;",
        "    pub fn f() { publish(); }",
        "}",
        "mod tests {",
        "    use super::*;",
        "    mod deep { fn g() {} }",
        "}",
        "fn h() {",
        "    use crate::inner::x;", // 11: the block of `h`
        "    x();",
        "    mod m { use super::*; }",
        "}",
    ]
    .join("\n");
    let x = extract("src/ship.rs", &source);
    let facts = facts(&x);
    let scope_of = |text: &str| {
        let found = facts
            .uses
            .iter()
            .find(|u| u.path == path(text))
            .expect(text);
        match &facts.scopes[found.scope as usize].kind {
            ScopeKind::File => "file".to_owned(),
            ScopeKind::Module { item } => format!("mod {}", facts.items[*item as usize].name),
            ScopeKind::Block => "block".to_owned(),
            other => format!("{other:?}"),
        }
    };
    // v1 stored line ranges minus the inline modules that do not see the
    // names (U-6 to U-8); the MVP stores the scope, and resolution walks
    // scopes as Rust does: a module never sees its parent's names but by a
    // glob, and `super` from a block is its module's parent.
    assert_eq!(scope_of("crate::left::push"), "file");
    assert_eq!(scope_of("crate::right::publish"), "mod a");
    assert_eq!(scope_of("crate::inner::x"), "block");
    let globs: Vec<String> = facts
        .uses
        .iter()
        .filter(|u| u.leaf == Leaf::Glob)
        .map(|u| match &facts.scopes[u.scope as usize].kind {
            ScopeKind::Module { item } => facts.items[*item as usize].name.clone(),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(globs, ["tests", "m"]);
}

#[test]
fn a_use_path_ignores_the_whitespace_inside_it() {
    let x = extract("src/ship.rs", "use crate::{\n    left ::push,\n};\n");
    assert_eq!(
        sites(&x),
        [(EdgeKind::Import, 2, "crate::left::push".to_owned())]
    );
    assert_eq!(facts(&x).uses[0].path, path("crate::left::push"));
}

#[test]
fn an_alias_binds_the_alias_to_the_source_path() {
    let x = extract("src/ship.rs", "pub use crate::push::push as leased;\n");
    assert_eq!(
        facts(&x).uses,
        [Use {
            vis: Vis::Public,
            ..leaf(0, "crate::push::push", Leaf::Name, Some("leased"))
        }]
    );
}

#[test]
fn a_leading_double_colon_and_an_extern_crate_are_read() {
    let x = extract(
        "src/lib.rs",
        "use ::std::fmt;\nextern crate serde as sd;\nextern crate self as me;\n",
    );
    assert_eq!(
        facts(&x).uses,
        [leaf(0, "::std::fmt", Leaf::Name, Some("fmt"))]
    );
    let crates: Vec<(String, String)> = facts(&x)
        .items
        .into_iter()
        .map(|i| match i.kind {
            super::super::ItemKind::ExternCrate { krate } => (i.name, krate),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        crates,
        [
            ("sd".to_owned(), "serde".to_owned()),
            ("me".to_owned(), "self".to_owned())
        ]
    );
}

// D40: `use ::{util::go}` lost its leading `::`, so the leaf bound the local
// module `util` that shadows the extern crate.
#[test]
fn c10_rust_a_group_after_a_leading_double_colon_names_an_extern_crate() {
    let files = [
        ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
        ("crates/util/Cargo.toml", "[package]\nname = \"util\"\n"),
        ("crates/util/src/lib.rs", "pub fn go() {}\n"),
        (
            "crates/app/Cargo.toml",
            "[package]\nname = \"app\"\n\n[dependencies]\nutil = { path = \"../util\" }\n",
        ),
        (
            "crates/app/src/main.rs",
            "mod util { pub fn go() {} }\nuse ::{util::go};\nuse ::util::{go as again};\nfn main() { go(); again(); }\n",
        ),
    ];
    // `cargo run` (rustc 1.99) runs the extern crate's `go` twice.
    let extern_go = Some("crates/util/src/lib.rs#go#function");
    assert_eq!(
        imports(&files),
        [
            edge("crates/app/src/main.rs", "::util::go", extern_go),
            edge("crates/app/src/main.rs", "::util::go", extern_go),
        ]
    );
    assert_eq!(
        calls(&files),
        [
            edge("main", "go", extern_go),
            edge("main", "again", extern_go)
        ]
    );
}
