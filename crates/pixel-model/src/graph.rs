// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The `Graph` read trait: what the chain and the commands read from a
//! generation. The store implements it over SQLite.

use std::fmt;

use crate::rows::{FileRow, Provenance, Symbol};

/// Reads one published generation. Unresolved edges are never returned
/// as callers or callees (C-10); an index that is corrupt or incompatible
/// is an error, never served (C-19).
pub trait Graph {
    /// The generation's provenance (C-11).
    fn provenance(&self) -> Result<Provenance, GraphError>;

    /// Every indexed file, sorted by path.
    fn files(&self) -> Result<Vec<FileRow>, GraphError>;

    /// The symbols of one file, in line order.
    fn symbols(&self, file: &str) -> Result<Vec<Symbol>, GraphError>;

    /// The symbols whose qualified name, or whose last segment
    /// ([`leaf`](crate::leaf)), equals `token` ignoring ASCII case, sorted
    /// by uid. The chain looks a prompt's tokens up here
    /// (`design/minimap-selection.md`, step 2).
    fn named(&self, token: &str) -> Result<Vec<Symbol>, GraphError>;
}

/// The changes of the working tree against `HEAD`, by file: the lines of
/// its hunks, as the new file numbers them. An untracked file is one hunk
/// over all its lines.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Diff {
    pub files: Vec<DiffFile>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffFile {
    /// Relative to the repository root, `/`-separated.
    pub path: String,
    /// Inclusive line ranges, 1-based; a deletion is the line after it.
    pub hunks: Vec<(u32, u32)>,
    /// The part of `hunks` that holds added lines, as the new file numbers
    /// them: a pure deletion adds none. An untracked file is added whole.
    pub added: Vec<(u32, u32)>,
}

/// Why a generation cannot be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphError(pub String);

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GraphError {}
