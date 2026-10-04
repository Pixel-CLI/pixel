// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

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

pub mod branch;
pub mod branches;
pub mod diff;
pub mod durable;
pub mod envfile;
pub mod fingerprint;
pub mod history;
pub mod inspect;
pub mod journal;
pub mod lock;
pub mod provenance;
pub mod publish;
pub mod push;
pub mod reconcile;
pub mod recovery;
pub mod repo;
pub mod review;
pub mod rewrite;
pub mod ship;
pub mod snapshot;
pub mod sync;
pub mod update;

pub use journal::{BeginOutcome, JournalOperation, JournalPhase, JournalRecord, OperationJournal};
pub use lock::{RepositoryBusyError, RepositoryLock};
pub use repo::repo_identity;
pub use snapshot::{SnapshotRecord, SnapshotStore, snapshot_token};
