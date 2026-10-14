// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The indexer: parses files with tree-sitter into symbols and edges. Each
//! language is one grammar and one rule module (`ruby`, `rust`). It returns
//! rows and writes nothing (ADR-0013): the binary hands them to the store.
//!
//! A run is `tree::scan`, then `link::plan` against the current generation,
//! `extract::extract` of the planned files within an optional deadline, and
//! `link::link`, which decides every site of the generation again.

pub mod codec;
pub mod extract;
pub mod guard;
pub mod lang;
pub mod link;
pub mod ruby;
pub mod rust;
pub mod text;
pub mod tree;

#[cfg(test)]
mod grammar;
#[cfg(test)]
mod truth;

pub use extract::{Extracted, extract};
pub use lang::extractor;
pub use link::{Plan, link, plan};
pub use tree::{
    Known, Repository, Tree, TreeError, TreeFile, changes, diff, discover, root, scan,
    scan_discovered, scan_with,
};
