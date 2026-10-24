// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Tests of the Rust rule file: extraction (v1's tests, sections 15.1 and
//! 15.2 of the port spec, with the MVP's mapping), resolution one class of
//! input at a time (C-10), and v1's end-to-end cases (15.3 to 15.6) with the
//! MVP's expectations. A test that proves C-10 is named `c10_rust_…`.
//!
//! The helpers below run the rule file and project its rows; each test
//! builds its fixture in its own body.

mod cfg;
mod endtoend;
mod extraction;
mod fixtures;
mod generics;
mod macros;
mod manifest;
mod properties;
mod receivers;
mod resolution;
mod unwrapping;
mod uses;

use pixel_model::{EdgeKind, Lang, SymbolKind};

use super::{Context, Facts, Rust};
use crate::extract::rows;
use crate::lang::{Extraction, Rules, World};

/// What the rule file extracts from one file.
fn extract(path: &str, src: &str) -> Extraction {
    Rust.extract(path, src.as_bytes())
}

/// `(kind, name)` of every definition, in walk order.
fn defs(extraction: &Extraction) -> Vec<(SymbolKind, String)> {
    extraction
        .defs
        .iter()
        .map(|d| (d.kind, d.name.clone()))
        .collect()
}

/// `(kind, line, target)` of every site, in walk order.
fn sites(extraction: &Extraction) -> Vec<(EdgeKind, u32, String)> {
    extraction
        .sites
        .iter()
        .map(|s| (s.kind, s.line, s.target.clone()))
        .collect()
}

fn contexts(extraction: &Extraction) -> Vec<Context> {
    extraction
        .sites
        .iter()
        .map(|s| Context::decode(&s.context).expect("a context this version wrote"))
        .collect()
}

fn facts(extraction: &Extraction) -> Facts {
    Facts::decode(&extraction.facts)
}

/// One decided edge: the name of its source symbol, its kind and line, its
/// target as written, and the uid of what it resolved to.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Linked {
    src: String,
    kind: EdgeKind,
    line: u32,
    target: String,
    dst: Option<String>,
}

/// Extracts every file as the driver does and resolves the generation.
fn link(files: &[(&str, &str)]) -> Vec<Linked> {
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
    edges
        .iter()
        .zip(decided)
        .map(|(edge, dst)| Linked {
            src: symbols
                .iter()
                .find(|s| s.uid == edge.src)
                .map(|s| s.name.clone())
                .unwrap_or_default(),
            kind: edge.kind,
            line: edge.line,
            target: edge.target.clone(),
            dst: dst.map(|i| symbols[i].uid.clone()),
        })
        .collect()
}

/// `(source, target, resolved uid)` of the edges of one kind.
fn edges_of(kind: EdgeKind, linked: &[Linked]) -> Vec<(String, String, Option<String>)> {
    linked
        .iter()
        .filter(|l| l.kind == kind)
        .map(|l| (l.src.clone(), l.target.clone(), l.dst.clone()))
        .collect()
}

fn calls(files: &[(&str, &str)]) -> Vec<(String, String, Option<String>)> {
    edges_of(EdgeKind::Call, &link(files))
}

fn imports(files: &[(&str, &str)]) -> Vec<(String, String, Option<String>)> {
    edges_of(EdgeKind::Import, &link(files))
}

fn references(files: &[(&str, &str)]) -> Vec<(String, String, Option<String>)> {
    edges_of(EdgeKind::Reference, &link(files))
}

/// An expected edge, in the shape of [`edges_of`].
fn edge(src: &str, target: &str, dst: Option<&str>) -> (String, String, Option<String>) {
    (src.to_owned(), target.to_owned(), dst.map(str::to_owned))
}
