// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

#![forbid(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::disallowed_macros
)]
//! Shared types of pixel-mvp: file, symbol, edge, provenance, and the
//! `Graph` read trait (ADR-0013). It depends on nothing and does no I/O.
//!
//! Clippy holds the ban, after macro expansion and name resolution, so a
//! rename, a glob import, a macro or a module read from elsewhere does not
//! hide a call: `clippy.toml` beside the manifest lists the `std` items
//! that reach the world (`std::fs`, `std::net`, `std::process`, `std::env`,
//! the standard streams and the print macros, `std::os`'s sockets and file
//! calls, the calls of `Path` that read the file system, threads, the clock,
//! a caught panic), and the attribute above forbids lifting those lints
//! anywhere in the crate. The workspace lint `unsafe_code = "forbid"` closes
//! FFI and assembly. Clippy itself compiles with `--cfg clippy`, so code
//! behind `#[cfg(not(clippy))]` would slip past it while every other build
//! runs it; the workspace test closes that, reading the compiler's own
//! dependency info to check each pure crate compiles only sources inside its
//! own directory and gates no code on the `clippy` cfg. What it does not
//! catch: a `std` item missing from the list (`HashMap`'s random seed is
//! one), and a build that does not run clippy; CI's lint job does. The
//! workspace test in `crates/pixel/tests/workspace.rs` checks that the
//! attribute, the workspace lints and the list are in place.
pub mod classify;
mod generation;
mod graph;
pub mod hash;
mod pick;
mod rows;
mod terms;

pub use generation::{Delta, FileRows, Kept};
pub use graph::{Diff, DiffFile, Graph, GraphError};
pub use pick::PluginPick;
pub use terms::{Language, Lexicon, fold, language, prompt_terms, stem, terms};

pub use rows::{
    Edge, EdgeKind, FileRow, FileStat, Lang, Provenance, Symbol, SymbolKind, leaf, sub_words,
    uid_path, utc,
};
