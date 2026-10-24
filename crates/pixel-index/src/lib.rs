// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! pixel-index: the git-anchored trigram text index (`specs/index-history.md`).
//!
//! A base shard at a pinned commit, a delta shard for the files changed
//! since, and an in-memory overlay for the dirty tree; mmapped shards;
//! regexes planned into trigram queries and every candidate verified.

pub mod cache;
pub mod delta;
pub mod gram;
pub mod history;
pub mod index;
pub mod indexset;
pub mod lock;
pub mod overlay;
pub mod path_filter;
pub mod plan;
pub mod posting;
pub mod semantic;
pub mod shard;
pub mod verify;

pub use gram::{GramExtractor, GramHit, TrigramExtractor};
pub use lock::BuildLock;
pub use posting::GramQuery;
