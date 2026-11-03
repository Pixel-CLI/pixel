// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The indexer: parses files with tree-sitter into symbols and edges. Each
//! language is one grammar and one rule module (`ruby`, `rust`). It returns
//! rows and writes nothing (ADR-0013): the binary hands them to the store.
//! The one write it asks for is git's, to the copy of git's index the
//! store placed in the state directory (`git_index`, ADR-0039).
//!
//! A run is `tree::scan`, then `link::plan` against the current generation,
//! `extract::extract` of the planned files within an optional deadline, and
//! `link::link`, which decides every site of the generation again.

// The imported crate's lint baseline, kept from its own manifest: unsafe code
// forbidden, and none of the workspace's adopted pedantic lints that it did
// not enable. Tightening it is a separate change.
#![forbid(unsafe_code)]
#![allow(
    clippy::items_after_statements,
    clippy::map_unwrap_or,
    clippy::redundant_closure_for_method_calls,
    clippy::stable_sort_primitive
)]

mod bounded;
pub mod codec;
pub mod extract;
pub mod git_index;
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
    scan_discovered, scan_discovered_in, scan_with,
};
