//! pixel-ops — safe git mutation infrastructure + read ops.
//!
//! Ports usable-git's crash-safety model (snapshot store, repository lock,
//! operation journal) to Rust, and implements the 11 ops (inspect, review,
//! history, diff, publish, push, ship, branch, update, sync) plus
//! reconcile/excavate/rescue-apply.
//!
//! The crash-safety model: every mutation runs under a repository lock,
//! journals each phase durably (temp → fsync → rename → dir fsync), and
//! can resume from any crash point. The crash matrix tests verify
//! zero-lost-work at every kill point.

pub mod journal;
pub mod lock;
pub mod snapshot;
pub mod durable;
pub mod recovery;
pub mod fingerprint;
pub mod inspect;
pub mod review;
pub mod history;
pub mod diff;
pub mod publish;
pub mod push;
pub mod branch;
pub mod update;
pub mod sync;
pub mod ship;
pub mod reconcile;
pub mod rewrite;
pub mod provenance;
pub mod branches;
pub mod envfile;

pub use journal::{OperationJournal, JournalRecord, JournalPhase, JournalOperation, BeginOutcome};
pub use lock::{RepositoryLock, RepositoryBusyError};
pub use snapshot::{SnapshotStore, SnapshotRecord, snapshot_token};
