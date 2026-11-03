// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Shared typed recovery hints for retrieval commands.
//!
//! A retrieval answer can be incomplete for many reasons: the request was
//! malformed, the index is unavailable, the snapshot drifted, a cap cut the
//! result list, or the answer is a valid empty. Each of those is a distinct
//! *cause* with a distinct *next action*. This module is the single typed
//! representation of that cause → action mapping, shared by the daemon (which
//! derives hints from the result) and the CLI (which renders them).
//!
//! The hint is **data for the agent, never an automatically executed
//! command**. It carries a typed `NextCall` (argv, not a shell fragment) or a
//! `Continuation` token (next offset + snapshot binding), so the caller can
//! render a runnable command with correct quoting without re-deriving the
//! request.
//!
//! This is consistency work, not a second error framework: `Warning` mirrors
//! caps, `Epistemics` attests completeness, `evaluate`'s `NextAction` carries
//! predicate-specific actions, and `RecoveryHint` is the cross-command action
//! channel. None of the others change.

use serde::{Deserialize, Serialize};

/// Why a retrieval answer is not the complete answer the caller wanted.
///
/// The taxonomy is closed: every retrieval gap maps to exactly one variant.
/// The daemon derives the cause from the same markers it uses for
/// `Epistemics`, so the two channels never disagree about *what* happened —
/// they describe it at different levels (completeness vs. actionability).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cause", rename_all = "snake_case")]
pub enum RecoveryCause {
    /// The request itself is malformed; the named argument must be fixed.
    /// The answer is not a retrieval result at all.
    InvalidInput {
        /// The argument that failed validation.
        argument: String,
        /// What the argument must look like.
        expected: String,
    },
    /// The answer is complete and empty: a valid zero-hit result, not an
    /// error. Distinct from `IndexUnavailable` (the index is missing) and
    /// `RowCap` (more rows exist but were cut).
    EmptyResult,
    /// The index or graph is unavailable; it must be built first. The answer
    /// is not a retrieval result.
    IndexUnavailable,
    /// The snapshot the answer was computed against has drifted. The answer
    /// is stale, not wrong.
    StaleSnapshot,
    /// The input's coverage is unsupported; no retrieval can answer it.
    UnsupportedCoverage {
        /// Why the coverage is unsupported.
        detail: String,
    },
    /// Several candidates match; one must be picked. The answer is a list of
    /// candidates, not a single resolution.
    AmbiguousSymbol,
    /// The row cap cut the result list; more rows exist. The answer is a
    /// prefix of the full result.
    RowCap {
        /// The row limit that fired.
        limit: u64,
    },
    /// The byte cap cut the result list; more bytes exist. The answer is a
    /// prefix of the full result.
    ByteCap,
    /// The depth cap cut the traversal (evaluate). The answer is a bounded
    /// view of the full graph.
    DepthCap {
        /// The depth limit that fired.
        current: u64,
    },
    /// The time budget cut the traversal (evaluate). The answer is a bounded
    /// view of the full graph.
    TimeCap {
        /// The time budget that fired, in milliseconds.
        current: u64,
    },
}

/// Whether the proposed next action writes state or contacts a service.
///
/// The agent must know this before running the suggested command: a
/// read-only continuation is always safe to retry, while a state-writing or
/// service-contacting action is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideEffect {
    /// Re-running the retrieval is read-only.
    None,
    /// The suggested action writes repository state.
    WritesState,
    /// The suggested action contacts a service.
    ContactsService,
}

impl SideEffect {
    /// The human-readable label for this side effect, used in rendered text.
    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "read-only",
            Self::WritesState => "writes state",
            Self::ContactsService => "contacts a service",
        }
    }
}

/// What to do when the snapshot a continuation is bound to has drifted.
///
/// A continuation is valid only for the snapshot it was computed against. When
/// that snapshot moves, the continuation's `next_offset` no longer points at
/// the same row, so the only safe action is to restart from the beginning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnDrift {
    /// Restart from the beginning; never promise no gaps.
    Restart,
}

/// The repository snapshot a continuation is valid for.
///
/// `head` is the commit the answer was computed against; `dirty_count` is the
/// number of dirty files at answer time; `index_generation` is a monotonic
/// counter that changes whenever the indexed content or ordering changes.
/// The index shifts when any of these move, so a continuation bound to this
/// snapshot is invalid once they change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotBinding {
    /// HEAD the answer was computed against.
    pub head: Option<String>,
    /// Dirty file count at answer time; the index shifts when it moves.
    pub dirty_count: u64,
    /// Monotonic index-generation counter; changes when indexed content or
    /// ordering changes even if HEAD and dirty_count stay the same.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_generation: Option<u64>,
}

/// A continuation token for a paged answer, bound to a snapshot.
///
/// The token is the structured argument for the next page: `next_offset` is
/// where the next page starts, `snapshot` is the tree state the offset is
/// valid for, and `on_drift` is the explicit result of that state moving.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Continuation {
    /// The offset the next page starts at.
    pub next_offset: u64,
    /// The snapshot this continuation is valid for.
    pub snapshot: SnapshotBinding,
    /// The explicit result of snapshot drift.
    pub on_drift: OnDrift,
}

/// The suggested next call as typed argv, preferred over a shell fragment.
///
/// `argv` is the command to run, as an argument vector (no shell parsing
/// needed). `purpose` is one line saying what the next call does. The caller
/// renders `argv` with correct quoting for display; the agent can copy the
/// rendered command or invoke `argv` directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NextCall {
    /// The command to run, as argv (no shell parsing needed).
    pub argv: Vec<String>,
    /// One line saying what the next call does.
    pub purpose: String,
}

/// One typed recovery hint: what is incomplete about this answer, and what a
/// valid next call looks like.
///
/// A hint is data for the agent, never an automatically executed command. It
/// carries a typed [`NextCall`] (argv, not a shell fragment) or a
/// [`Continuation`] token (next offset + snapshot binding), so the caller can
/// render a runnable command with correct quoting without re-deriving the
/// request.
///
/// The hint is attached only when the answer has a gap: a complete, uncapped
/// answer carries no hint, so responses without gaps stay byte-identical.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryHint {
    /// What is incomplete (or wrong) about this answer.
    pub cause: RecoveryCause,
    /// The retrieval ops this hint applies to.
    pub applies_to: Vec<String>,
    /// The suggested next call, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_call: Option<NextCall>,
    /// Continuation token and snapshot binding, when the answer is a page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<Continuation>,
    /// Whether the proposed action writes state or contacts a service.
    pub side_effect: SideEffect,
}

impl RecoveryHint {
    /// Whether this hint suggests a runnable next call.
    ///
    /// A hint is actionable only when it carries a typed [`NextCall`] with
    /// enough information to execute the next call. A [`Continuation`] alone
    /// is not actionable: it records where the next page starts and which
    /// snapshot it is valid for, but not the filters, ordering, or paths
    /// that produced the page. The caller must combine the continuation with
    /// the original request to build a runnable command.
    pub const fn is_actionable(&self) -> bool {
        self.next_call.is_some()
    }

    /// The human-readable label for this hint's side effect.
    pub const fn side_effect_label(&self) -> &'static str {
        self.side_effect.label()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hint() -> RecoveryHint {
        RecoveryHint {
            cause: RecoveryCause::RowCap { limit: 50 },
            applies_to: vec!["search".to_string()],
            next_call: Some(NextCall {
                argv: vec![
                    "search-content".to_string(),
                    "foo".to_string(),
                    ".".to_string(),
                    "--offset".to_string(),
                    "50".to_string(),
                ],
                purpose: "continue the page".to_string(),
            }),
            continuation: Some(Continuation {
                next_offset: 50,
                snapshot: SnapshotBinding {
                    head: Some("abc123".to_string()),
                    dirty_count: 0,
                    index_generation: Some(1),
                },
                on_drift: OnDrift::Restart,
            }),
            side_effect: SideEffect::None,
        }
    }

    #[test]
    fn cause_serializes_as_snake_case_tag() {
        let hint = hint();
        let v = serde_json::to_value(&hint).unwrap();
        assert_eq!(v["cause"]["cause"], serde_json::json!("row_cap"));
        assert_eq!(v["cause"]["limit"], serde_json::json!(50));
    }

    #[test]
    fn empty_result_cause_has_no_payload() {
        let hint = RecoveryHint {
            cause: RecoveryCause::EmptyResult,
            applies_to: vec!["symbol".to_string()],
            next_call: None,
            continuation: None,
            side_effect: SideEffect::None,
        };
        let v = serde_json::to_value(&hint).unwrap();
        assert_eq!(v["cause"]["cause"], serde_json::json!("empty_result"));
    }

    #[test]
    fn invalid_input_cause_carries_argument_and_expected() {
        let hint = RecoveryHint {
            cause: RecoveryCause::InvalidInput {
                argument: "--limit".to_string(),
                expected: "a positive integer".to_string(),
            },
            applies_to: vec!["search".to_string()],
            next_call: None,
            continuation: None,
            side_effect: SideEffect::None,
        };
        let v = serde_json::to_value(&hint).unwrap();
        assert_eq!(v["cause"]["cause"], serde_json::json!("invalid_input"));
        assert_eq!(v["cause"]["argument"], serde_json::json!("--limit"));
        assert_eq!(
            v["cause"]["expected"],
            serde_json::json!("a positive integer")
        );
    }

    #[test]
    fn side_effect_labels() {
        assert_eq!(SideEffect::None.label(), "read-only");
        assert_eq!(SideEffect::WritesState.label(), "writes state");
        assert_eq!(SideEffect::ContactsService.label(), "contacts a service");
    }

    #[test]
    fn hint_with_next_call_is_actionable() {
        let hint = RecoveryHint {
            next_call: Some(NextCall {
                argv: vec!["find-code".to_string(), "foo".to_string()],
                purpose: "search for the name".to_string(),
            }),
            ..hint()
        };
        assert!(hint.is_actionable());
    }

    #[test]
    fn hint_with_continuation_only_is_not_actionable() {
        let hint = RecoveryHint {
            next_call: None,
            ..hint()
        };
        assert!(!hint.is_actionable());
    }

    #[test]
    fn hint_with_neither_next_call_nor_continuation_is_not_actionable() {
        let hint = RecoveryHint {
            next_call: None,
            continuation: None,
            ..hint()
        };
        assert!(!hint.is_actionable());
    }

    #[test]
    fn side_effect_label_delegates_to_side_effect() {
        let hint = RecoveryHint {
            side_effect: SideEffect::WritesState,
            ..hint()
        };
        assert_eq!(hint.side_effect_label(), "writes state");
    }

    #[test]
    fn hint_round_trips_through_json() {
        let hint = hint();
        let v = serde_json::to_value(&hint).unwrap();
        let back: RecoveryHint = serde_json::from_value(v).unwrap();
        assert_eq!(hint, back);
    }

    #[test]
    fn hint_skips_none_fields() {
        let hint = RecoveryHint {
            cause: RecoveryCause::EmptyResult,
            applies_to: vec!["search".to_string()],
            next_call: None,
            continuation: None,
            side_effect: SideEffect::None,
        };
        let v = serde_json::to_value(&hint).unwrap();
        assert!(v.get("next_call").is_none());
        assert!(v.get("continuation").is_none());
    }

    #[test]
    fn on_drift_serializes_as_restart() {
        let hint = hint();
        let v = serde_json::to_value(&hint).unwrap();
        assert_eq!(v["continuation"]["on_drift"], serde_json::json!("restart"));
    }

    #[test]
    fn snapshot_binding_carries_head_dirty_count_and_index_generation() {
        let hint = hint();
        let v = serde_json::to_value(&hint).unwrap();
        assert_eq!(
            v["continuation"]["snapshot"]["head"],
            serde_json::json!("abc123")
        );
        assert_eq!(
            v["continuation"]["snapshot"]["dirty_count"],
            serde_json::json!(0)
        );
        assert_eq!(
            v["continuation"]["snapshot"]["index_generation"],
            serde_json::json!(1)
        );
    }
}
