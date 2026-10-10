// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel-proto` — the shared contract crate for the `pixel` project.
//!
//! Holds ONLY typed contracts: the response envelope, error codes, the
//! snapshot token type, and a minimal `Op` enum mirroring the current
//! daemon wire format. No I/O, no business logic, no daemon/CLI code — see
//! `PLAN.md`'s Part A1 crate layout for how this fits into the workspace.
//! Every other pixel crate that needs the wire contract depends on this
//! one; this crate depends on nothing pixel-internal.

pub mod budget;
pub mod commands;
pub mod envelope;
pub mod epistemics;
pub mod error;
pub mod evaluate;
pub mod evidence;
pub mod facts;
pub mod op;
pub mod query;
pub mod recovery;
pub mod snapshot;
pub mod warning;

pub use budget::{Budget, BudgetInfo};
pub use envelope::{ENVELOPE_PROTOCOL_VERSION, Envelope};
pub use epistemics::Epistemics;
pub use error::{ErrorCode, PixelError};
pub use evidence::{CapHit, EvidenceBasis, SourceCoverage, SourceEpistemics};
pub use facts::{
    CoFile, KeywordEvidence, Relevance, TargetsFactsInputs, TargetsFactsResult,
    TargetsFactsUnavailableReason,
};
pub use op::Op;
pub use query::{QueryKind, QueryPlan, QueryResult, QueryStatus, compile_query};
pub use recovery::{
    Continuation, NextCall, OnDrift, RecoveryCause, RecoveryHint, SideEffect, SnapshotBinding,
};
pub use snapshot::{Snapshot, SnapshotInfo, SnapshotToken};
pub use warning::Warning;
