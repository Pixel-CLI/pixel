// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! A generation as the indexer hands it to the store, through the binary
//! (ADR-0013): one file's rows, the rows a run keeps, and the delta it
//! publishes.

use crate::rows::{Edge, FileRow, FileStat, Provenance, Symbol};

/// One extracted file, as the store keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRows {
    pub file: FileRow,
    /// The rule file's declarations for resolution (`Extraction::facts`).
    pub facts: String,
    pub symbols: Vec<Symbol>,
    /// Sites, undecided until `link`.
    pub edges: Vec<Edge>,
}

/// The rows of the current generation that the plan keeps.
#[derive(Clone, Debug, Default)]
pub struct Kept {
    /// Each kept file with its facts.
    pub files: Vec<(FileRow, String)>,
    pub symbols: Vec<Symbol>,
    /// As stored, with their current `dst`.
    pub edges: Vec<Edge>,
}

/// The next generation, as changes to the current one. The store applies
/// it in one transaction.
#[derive(Clone, Debug)]
pub struct Delta {
    pub provenance: Provenance,
    /// Paths whose rows go.
    pub drop: Vec<String>,
    /// New rows, every site decided.
    pub add: Vec<FileRows>,
    /// Kept sites whose decision changed: the edge as stored, its new `dst`.
    pub redirect: Vec<(Edge, Option<String>)>,
    /// Files the deadline did not reach, counted, sorted: one whose content
    /// changed keeps its previous rows; any other, new or whose rows
    /// another extractor version read, is absent from the generation, and
    /// then no site of its language is resolved in it (C-10). Part of the
    /// generation id.
    pub stale: Vec<String>,
    /// Every file of the generation, with its hash, sorted by path.
    pub files: Vec<FileRow>,
    /// The stat each file of the tree was hashed under, by path, `None`
    /// where it is not to be trusted: the store keeps it beside the file's
    /// hash, and the next scan takes the hash from there while the stat
    /// holds (ADR-0022 §2).
    pub stats: Vec<(String, Option<FileStat>)>,
}
