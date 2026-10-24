// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `pixel-proto`: the shared wire contract (`specs/ops.md`).
//!
//! Holds only typed contracts: the response envelope, error codes, the
//! snapshot token, epistemics, budgets, recovery hints, the brief-gate facts
//! and meaning results, and the 15-variant `Op` enum. No I/O and no business
//! logic. It depends on nothing PIXEL-internal.

pub mod budget;
pub mod envelope;
pub mod epistemics;
pub mod error;
pub mod evidence;
pub mod facts;
pub mod meaning;
pub mod op;
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
pub use meaning::{MeaningHit, MeaningPool, MeaningResult, MeaningUnavailableReason};
pub use op::{EditPhase, Op};
pub use recovery::{
    Continuation, NextCall, OnDrift, RecoveryCause, RecoveryHint, SideEffect, SnapshotBinding,
};
pub use snapshot::{Snapshot, SnapshotInfo, SnapshotToken};
pub use warning::Warning;
