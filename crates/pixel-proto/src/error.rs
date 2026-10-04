// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Error code + error payload contracts.
//!
//! `ErrorCode` reproduces usable-git's 18-value `errorCodeSchema`
//! (`reference/usable-git/packages/usable-git/src/contracts/v1.ts`) verbatim,
//! plus 4 pixel-specific codes for states usable-git never had to model
//! (background index build in progress, ambiguous symbol resolution, no
//! index present yet, and a name/uid/path lookup that came up empty).

use serde::{Deserialize, Serialize};

/// The `error.code` a failure envelope carries: programmatic handling for a
/// caller that must decide what to do next (retry, fetch and reconcile,
/// widen the query) instead of reading the message text.
///
/// A code a caller can receive today is one of the ten the daemon's
/// classifier (`pixel_daemon::api::classify_error`) produces:
///
/// - `InvalidInput`, the default for anything unclassified;
/// - `NotFound` (`no symbol named …`) and `BusyRepository` (`repository is
///   busy…`), the two message shapes the daemon's own call sites write;
/// - `NonFastForward`, `StaleState`, `UnsupportedState`, `RefExists`,
///   `GitFailed` and `NetworkAmbiguity`, which `pixel-ops` names in the
///   `"<CODE>: …"` prefix of the messages it types itself;
/// - `InvariantViolation`, only from the daemon transport's serialize-failure
///   fallback, which no operation reaches.
///
/// The other twelve variants are declared for operations that do not exist
/// yet (`NothingToCommit`, `HookFailed`, `SigningFailed`, `IdentityMissing`,
/// `AuthFailed`, `LeaseRejected`, `RecoveryConflict`, `InvalidRepository`,
/// `InvalidPath`) or for states this codebase reaches another way without an
/// error (`IndexBuilding` and `NotIndexed` build lazily, `Ambiguous` answers
/// with a candidate set): no message classifies to them, so their absence
/// carries no signal and a caller must not branch on them being possible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    // -- usable-git v1 codes, verbatim --------------------------------
    InvalidInput,
    InvalidRepository,
    InvalidPath,
    UnsupportedState,
    StaleState,
    BusyRepository,
    NothingToCommit,
    HookFailed,
    SigningFailed,
    IdentityMissing,
    AuthFailed,
    NonFastForward,
    LeaseRejected,
    NetworkAmbiguity,
    RecoveryConflict,
    InvariantViolation,
    RefExists,
    GitFailed,
    // -- pixel-specific additions -------------------------------------
    /// The repo daemon's index/graph is still (re)building; the answer is
    /// not yet available and the caller should retry.
    IndexBuilding,
    /// A name/uid resolved to more than one candidate; see the response's
    /// `ambiguity` payload for the disambiguation set.
    Ambiguous,
    /// No index exists yet for this repository (never built, or explicitly
    /// removed) and the operation requires one.
    NotIndexed,
    /// A name/uid/path lookup came up empty (e.g. "no symbol named X", "no
    /// symbol with uid X"). Distinguishes "you asked about something that
    /// doesn't exist" from a malformed request (`InvalidInput`).
    NotFound,
}

/// A single operation error: `{code, message, details?, ambiguity?}`.
///
/// `ambiguity` is the first-class LLM-handoff payload described in
/// `PLAN.md`'s Envelope v2 design (e.g. conflict hunks with base/ours/theirs,
/// or ranked disambiguation candidates). Its shape is deliberately left as
/// `serde_json::Value` — no op produces it yet, so there is nothing to type
/// concretely against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PixelError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ambiguity: Option<serde_json::Value>,
}

impl PixelError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        PixelError {
            code,
            message: message.into(),
            details: None,
            ambiguity: None,
        }
    }

    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn with_ambiguity(mut self, ambiguity: serde_json::Value) -> Self {
        self.ambiguity = Some(ambiguity);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `ErrorCode` variant must round-trip through serde_json and
    /// produce exactly the expected SCREAMING_SNAKE_CASE wire string. This
    /// pins both usable-git's 18 verbatim codes and the 3 pixel additions.
    #[test]
    fn error_code_round_trips_with_expected_wire_strings() {
        let cases: &[(ErrorCode, &str)] = &[
            (ErrorCode::InvalidInput, "\"INVALID_INPUT\""),
            (ErrorCode::InvalidRepository, "\"INVALID_REPOSITORY\""),
            (ErrorCode::InvalidPath, "\"INVALID_PATH\""),
            (ErrorCode::UnsupportedState, "\"UNSUPPORTED_STATE\""),
            (ErrorCode::StaleState, "\"STALE_STATE\""),
            (ErrorCode::BusyRepository, "\"BUSY_REPOSITORY\""),
            (ErrorCode::NothingToCommit, "\"NOTHING_TO_COMMIT\""),
            (ErrorCode::HookFailed, "\"HOOK_FAILED\""),
            (ErrorCode::SigningFailed, "\"SIGNING_FAILED\""),
            (ErrorCode::IdentityMissing, "\"IDENTITY_MISSING\""),
            (ErrorCode::AuthFailed, "\"AUTH_FAILED\""),
            (ErrorCode::NonFastForward, "\"NON_FAST_FORWARD\""),
            (ErrorCode::LeaseRejected, "\"LEASE_REJECTED\""),
            (ErrorCode::NetworkAmbiguity, "\"NETWORK_AMBIGUITY\""),
            (ErrorCode::RecoveryConflict, "\"RECOVERY_CONFLICT\""),
            (ErrorCode::InvariantViolation, "\"INVARIANT_VIOLATION\""),
            (ErrorCode::RefExists, "\"REF_EXISTS\""),
            (ErrorCode::GitFailed, "\"GIT_FAILED\""),
            (ErrorCode::IndexBuilding, "\"INDEX_BUILDING\""),
            (ErrorCode::Ambiguous, "\"AMBIGUOUS\""),
            (ErrorCode::NotIndexed, "\"NOT_INDEXED\""),
            (ErrorCode::NotFound, "\"NOT_FOUND\""),
        ];
        assert_eq!(
            cases.len(),
            22,
            "expected 18 usable-git codes + 4 pixel codes"
        );
        for (code, expected) in cases {
            let serialized = serde_json::to_string(code).unwrap();
            assert_eq!(&serialized, expected, "serialize({code:?})");
            let round_tripped: ErrorCode = serde_json::from_str(expected).unwrap();
            assert_eq!(round_tripped, *code, "deserialize({expected})");
        }
    }

    #[test]
    fn pixel_error_omits_absent_optional_fields() {
        let error = PixelError::new(ErrorCode::NonFastForward, "ref moved");
        let value = serde_json::to_value(&error).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"code": "NON_FAST_FORWARD", "message": "ref moved"})
        );
    }
}
