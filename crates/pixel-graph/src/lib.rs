// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! pixel-graph — symbols, imports, tiered call resolution, analyses.
//!
//! Module ownership (parallel development contract — do not fold modules
//! into each other):
//! - `store`: SQLite persistence + shared row types (THE schema contract)
//! - `extract`: tree-sitter per-file symbol/import/call-site extraction
//! - `imports`: import-spec → file resolution
//! - `resolve`: tiered edge resolution + epistemic envelope
//! - `build`: whole-repo build/update orchestration
//! - `impact`, `trace`, `process`, `cluster`, `changes`: analyses
//! - `predicate`: bounded reachability with witnesses (`pixel evaluate`)

pub mod build;
pub mod changes;
pub mod cluster;
pub mod concept;
pub mod concept_resolve;
pub mod cycles;
pub mod extract;
pub mod impact;
pub mod imports;
pub mod plan;
pub mod predicate;
pub mod process;
pub mod rename;
pub mod resolve;
pub mod review;
pub mod store;
pub mod targets;
pub mod trace;

pub use impact::split_ident_words;
pub use store::{
    EdgeKind, EdgeRow, Envelope, FileRow, GraphStore, StoreError, SymbolKind, SymbolRow, Tier,
};
