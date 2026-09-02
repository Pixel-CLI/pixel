//! pixel-index — sparse n-gram text index for the gitpixel sidecar.
//!
//! Phase 0 surface: gram extraction (`gram`), window weighting (`weights`),
//! and posting-list algebra (`posting`). Shards, git anchoring, overlay,
//! planner, and verification land in later phases (see the project plan).

pub mod gram;
pub mod lock;
pub mod posting;
pub mod weights;

pub use gram::{GramExtractor, GramHit, SparseGramExtractor, TrigramExtractor};
pub use lock::BuildLock;
pub use posting::GramQuery;
pub use weights::{Crc32Weigher, Weigher};
pub mod delta;
pub mod gitsync;
pub mod index;
pub mod indexset;
pub mod overlay;
pub mod plan;
pub mod shard;
pub mod verify;
