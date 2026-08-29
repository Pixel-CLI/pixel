//! pixel-facts — M3 / Engine 2: history-wide fact & diff ingest, search,
//! lifecycle, and rescue-v2 discovery. Owns `.pixel/history.db` plus trigram
//! history segments, with a dedicated low-priority ingest thread that never
//! blocks queries.

pub mod store;
pub mod ingest;
pub mod search;
pub mod lifecycle;
pub mod excavate;
pub mod poison;

pub use store::{FactsError, FactsStore, IndexState};
