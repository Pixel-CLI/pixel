// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Semantic search over code chunks (`specs/semantic.md`).
//!
//! A natural-language question is embedded and its chunks ranked by fusing
//! the semantic rank with a lexical BM25 rank ([`rank`]). The pieces are
//! pure and cheap: the [`chunk`]er reads symbol spans handed to it (the graph
//! store already parsed the file once), the [`store`] keeps vectors on disk
//! keyed by chunk content, the [`embed`] trait hides the model. Only the
//! `model2vec` backend is behind a Cargo feature, since it alone pulls a
//! tokenizer and tensor stack; without it the binary can still read, rank and
//! serve vectors but cannot embed, and says so.

pub mod chunk;
pub mod embed;
pub mod pool;
pub mod rank;
pub mod store;

use std::fmt;

/// Why a semantic operation failed. A message only: every caller degrades to
/// the lexical leads and reports the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticError(pub String);

impl fmt::Display for SemanticError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SemanticError {}

impl From<std::io::Error> for SemanticError {
    fn from(error: std::io::Error) -> Self {
        Self(error.to_string())
    }
}

impl From<serde_json::Error> for SemanticError {
    fn from(error: serde_json::Error) -> Self {
        Self(error.to_string())
    }
}

/// Shorthand for the module's results.
pub type Result<T> = std::result::Result<T, SemanticError>;
