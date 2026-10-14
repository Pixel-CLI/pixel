// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The `Graph` read trait: what the chain and the commands read from a
//! generation. The store implements it over SQLite (ADR-0013).

use std::fmt;

use crate::rows::{Edge, FileRow, Provenance, Symbol};

/// Reads one published generation. Unresolved edges are never returned
/// as callers or callees (C-10); an index that is corrupt or incompatible
/// is an error, never served (C-19).
pub trait Graph {
    /// The generation's provenance (C-11).
    fn provenance(&self) -> Result<Provenance, GraphError>;

    /// Every indexed file, sorted by path.
    fn files(&self) -> Result<Vec<FileRow>, GraphError>;

    /// The languages of the indexed files, by name (`Lang::as_str`).
    fn languages(&self) -> Result<std::collections::BTreeSet<String>, GraphError> {
        Ok(self
            .files()?
            .iter()
            .map(|file| file.lang.as_str().to_owned())
            .collect())
    }

    /// The symbols of one file, in line order.
    fn symbols(&self, file: &str) -> Result<Vec<Symbol>, GraphError>;

    /// The resolved edges whose target is `uid`.
    fn callers(&self, uid: &str) -> Result<Vec<Edge>, GraphError>;

    /// The resolved edges whose source is `uid`.
    fn callees(&self, uid: &str) -> Result<Vec<Edge>, GraphError>;

    /// The symbols whose qualified name, or whose last segment
    /// ([`leaf`](crate::leaf)), equals `token` ignoring ASCII case, sorted
    /// by uid. The chain looks a prompt's tokens up here
    /// (`design/minimap-selection.md`, step 2).
    fn named(&self, token: &str) -> Result<Vec<Symbol>, GraphError>;

    /// How many files have `word` as a whole sub-word of their path or of
    /// one of their symbols' names, ignoring ASCII case: a word's document
    /// frequency, which weighs a plain word of a prompt (step 5).
    fn files_with_word(&self, word: &str) -> Result<usize, GraphError>;

    /// The files holding `term` ([`terms`](crate::terms)) in their path or
    /// in one of their symbols' names or signatures, each with its count
    /// of it and its own count of terms: what BM25 reads for a prompt's
    /// plain words (ADR-0030). Sorted by path. A graph without postings
    /// has none, and the chain then picks by names alone.
    fn postings(&self, term: &str) -> Result<Vec<Posting>, GraphError> {
        let _ = term;
        Ok(Vec::new())
    }

    /// How many files hold postings, and their mean count of terms.
    fn corpus(&self) -> Result<Corpus, GraphError> {
        Ok(Corpus::default())
    }
}

/// One file's count of one term (ADR-0030).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Posting {
    pub file: String,
    /// The term's count in the file's document: path, symbol names and
    /// signatures.
    pub count: u32,
    /// The document's count of terms.
    pub length: u32,
}

/// The documents BM25 ranks: one per file with at least one term.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Corpus {
    pub documents: usize,
    pub mean_length: f64,
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
