// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! C-10's property test for Rust: on generated programs, everything the
//! rule file writes is true of the tree it read. Every signature is at the
//! line given, every site's line holds the name it calls, and a resolved
//! edge reaches a definition of the name written (the generator writes no
//! alias, so a different name would be a guessed edge).

use proptest::prelude::*;

use crate::text;

use super::*;

const VALUES: [&str; 4] = ["run", "push", "open", "helper"];
const TYPES: [&str; 3] = ["Store", "Walker", "Runner"];
const MODULES: [&str; 3] = ["a", "b", "util"];

#[derive(Clone, Debug)]
enum Stmt {
    Call(usize),
    Path(usize, usize),
    Method(usize, usize),
    Ctor(usize, usize),
}

#[derive(Clone, Debug)]
enum Piece {
    Fn(usize, Vec<Stmt>, bool),
    Struct(usize),
    Impl(usize, Vec<(usize, Vec<Stmt>)>),
    Trait(usize, Vec<usize>),
    Mod(usize, Vec<Piece>),
    Use(usize, usize),
}

fn stmt() -> impl Strategy<Value = Stmt> {
    prop_oneof![
        (0..VALUES.len()).prop_map(Stmt::Call),
        (0..MODULES.len(), 0..VALUES.len()).prop_map(|(m, v)| Stmt::Path(m, v)),
        (0..TYPES.len(), 0..VALUES.len()).prop_map(|(t, v)| Stmt::Method(t, v)),
        (0..TYPES.len(), 0..VALUES.len()).prop_map(|(t, v)| Stmt::Ctor(t, v)),
    ]
}

fn piece() -> impl Strategy<Value = Piece> {
    let leaf = prop_oneof![
        (
            0..VALUES.len(),
            prop::collection::vec(stmt(), 0..4),
            any::<bool>()
        )
            .prop_map(|(n, body, one_line)| Piece::Fn(n, body, one_line)),
        (0..TYPES.len()).prop_map(Piece::Struct),
        (
            0..TYPES.len(),
            prop::collection::vec((0..VALUES.len(), prop::collection::vec(stmt(), 0..3)), 0..3)
        )
            .prop_map(|(t, methods)| Piece::Impl(t, methods)),
        (0..TYPES.len(), prop::collection::vec(0..VALUES.len(), 0..3))
            .prop_map(|(t, methods)| Piece::Trait(t, methods)),
        (0..MODULES.len(), 0..VALUES.len()).prop_map(|(m, v)| Piece::Use(m, v)),
    ];
    leaf.prop_recursive(2, 12, 4, |inner| {
        (0..MODULES.len(), prop::collection::vec(inner, 0..4))
            .prop_map(|(m, items)| Piece::Mod(m, items))
    })
}

fn render_stmt(stmt: &Stmt, out: &mut String) {
    match stmt {
        Stmt::Call(v) => out.push_str(&format!("    {}();\n", VALUES[*v])),
        Stmt::Path(m, v) => {
            out.push_str(&format!("    crate::{}::{}();\n", MODULES[*m], VALUES[*v]))
        }
        Stmt::Method(t, v) => out.push_str(&format!(
            "    let x: {} = make();\n    x\n        .{}();\n",
            TYPES[*t], VALUES[*v]
        )),
        Stmt::Ctor(t, v) => out.push_str(&format!("    {}::{}();\n", TYPES[*t], VALUES[*v])),
    }
}

fn render(piece: &Piece, out: &mut String) {
    match piece {
        Piece::Fn(name, body, one_line) if *one_line && body.is_empty() => {
            out.push_str(&format!("pub fn {}() {{}}\n", VALUES[*name]));
        }
        Piece::Fn(name, body, _) => {
            out.push_str(&format!("pub fn {}(\n    arg: u8,\n) {{\n", VALUES[*name]));
            body.iter().for_each(|s| render_stmt(s, out));
            out.push_str("}\n");
        }
        Piece::Struct(t) => out.push_str(&format!(
            "#[derive(Debug)]\npub struct {}(u8);\n",
            TYPES[*t]
        )),
        Piece::Impl(t, methods) => {
            out.push_str(&format!("impl {} {{\n", TYPES[*t]));
            for (name, body) in methods {
                out.push_str(&format!("    pub fn {}(&self) {{\n", VALUES[*name]));
                body.iter().for_each(|s| render_stmt(s, out));
                out.push_str("    }\n");
            }
            out.push_str("}\n");
        }
        Piece::Trait(t, methods) => {
            out.push_str(&format!("pub trait {}Ext {{\n", TYPES[*t]));
            for name in methods {
                out.push_str(&format!("    fn {}(&self);\n", VALUES[*name]));
            }
            out.push_str("}\n");
        }
        Piece::Mod(m, items) => {
            out.push_str(&format!("pub mod {} {{\n    use super::*;\n", MODULES[*m]));
            items.iter().for_each(|p| render(p, out));
            out.push_str("}\n");
        }
        Piece::Use(m, v) => out.push_str(&format!("use crate::{}::{};\n", MODULES[*m], VALUES[*v])),
    }
}

/// The last name of a path or a method call as written (`x.run` → `run`).
fn written_name(target: &str) -> &str {
    target.rsplit(['.', ':']).next().unwrap_or(target)
}

/// The name a symbol defines (`<B as Tr>::q` → `q`, `E::V` → `V`).
fn defined_name(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    #[test]
    fn c10_rust_property_symbols_sites_and_edges_are_true_of_the_tree(
        pieces in prop::collection::vec(piece(), 1..6),
    ) {
        let mut source = String::new();
        pieces.iter().for_each(|p| render(p, &mut source));
        let files = [("Cargo.toml", "[package]\nname = \"app\"\n"), ("src/lib.rs", source.as_str())];
        let extraction = extract("src/lib.rs", &source);
        let lines = text::line_count(source.as_bytes());
        for def in &extraction.defs {
            let at = text::line(&source, def.line).unwrap_or_default();
            prop_assert!(at.contains(&def.signature), "{def:?} not at line {}: {at:?}", def.line);
            prop_assert!(def.line <= def.end_line && def.end_line <= lines, "{def:?}");
        }
        for site in &extraction.sites {
            let at = text::line(&source, site.line).unwrap_or_default();
            let name = written_name(&site.target);
            prop_assert!(at.contains(name), "{site:?} not at line {}: {at:?}", site.line);
        }
        for linked in link(&files).into_iter().filter(|l| l.kind != EdgeKind::Import) {
            let Some(dst) = &linked.dst else { continue };
            let defined = dst.split('#').nth(1).map(defined_name).unwrap_or_default();
            let written = written_name(&linked.target);
            prop_assert_eq!(defined, written, "{:?}", linked);
        }
    }
}
