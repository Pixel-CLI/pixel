// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The response envelope: "Envelope v2" from `PLAN.md`'s Part A design,
//! extending usable-git's v1 envelope (`ok`/`result`/`error`) with
//! `op`/`protocol`/`requestId`/`snapshot`/`epistemics`/`budget`/`warnings`.
//!
//! Field casing intentionally mirrors `PLAN.md`'s literal JSON example
//! rather than a single blanket rule: `requestId` is camelCase, but
//! `epistemics`'s inner fields (`closed_world`, `lower_bound`,
//! `staleness_ms`) are snake_case, and `budget`'s inner `byteCap` is
//! camelCase again. Each field that needs a non-default name carries an
//! explicit `#[serde(rename = "...")]` so the wire format matches the plan
//! verbatim instead of drifting under a container-level case rule.

use serde::{Deserialize, Serialize};

use crate::budget::BudgetInfo;
use crate::epistemics::Epistemics;
use crate::error::PixelError;
use crate::snapshot::SnapshotInfo;
use crate::warning::Warning;

/// Schema version of this envelope crate's wire contract. Distinct from
/// `pixel_daemon::api::PROTOCOL_VERSION`, which versions the daemon's
/// Unix-socket NDJSON request/response wire format — the two evolve
/// independently and must never be conflated.
pub const ENVELOPE_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub ok: bool,
    pub op: String,
    pub protocol: u32,
    #[serde(default, rename = "requestId", skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SnapshotInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epistemics: Option<Epistemics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<BudgetInfo>,
    #[serde(default)]
    pub result: Option<T>,
    #[serde(default)]
    pub error: Option<PixelError>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<Warning>,
}

impl<T> Envelope<T> {
    /// Build a success envelope: `ok: true`, `result` populated, `error`
    /// absent, everything else defaulted to `None`/empty.
    pub fn success(op: impl Into<String>, result: T) -> Self {
        Envelope {
            ok: true,
            op: op.into(),
            protocol: ENVELOPE_PROTOCOL_VERSION,
            request_id: None,
            snapshot: None,
            epistemics: None,
            budget: None,
            result: Some(result),
            error: None,
            warnings: Vec::new(),
        }
    }

    /// Build a failure envelope: `ok: false`, `error` populated, `result`
    /// absent, everything else defaulted to `None`/empty.
    pub fn failure(op: impl Into<String>, error: PixelError) -> Self {
        Envelope {
            ok: false,
            op: op.into(),
            protocol: ENVELOPE_PROTOCOL_VERSION,
            request_id: None,
            snapshot: None,
            epistemics: None,
            budget: None,
            result: None,
            error: Some(error),
            warnings: Vec::new(),
        }
    }

    /// Check the structural invariants every envelope on the wire must
    /// satisfy, independent of the op:
    ///
    /// - `protocol` equals this crate's `ENVELOPE_PROTOCOL_VERSION`;
    /// - `op` is non-empty;
    /// - `ok: true` carries a `result` and no `error`;
    /// - `ok: false` carries an `error` and no `result`;
    /// - every `error.message` is non-empty.
    ///
    /// The daemon asserts this on every response it builds (debug builds)
    /// and the CLI contract tests assert it on real output, so an op cannot
    /// ship an envelope that claims success without a payload or failure
    /// without a reason.
    pub fn validate(&self) -> Result<(), String> {
        if self.protocol != ENVELOPE_PROTOCOL_VERSION {
            return Err(format!(
                "protocol {} != {ENVELOPE_PROTOCOL_VERSION}",
                self.protocol
            ));
        }
        if self.op.is_empty() {
            return Err("op is empty".into());
        }
        match (self.ok, self.result.is_some(), &self.error) {
            (true, false, _) => Err("ok envelope has no result".into()),
            (true, true, Some(_)) => Err("ok envelope also carries an error".into()),
            (false, _, None) => Err("failure envelope has no error".into()),
            (false, true, Some(_)) => Err("failure envelope also carries a result".into()),
            (false, false, Some(e)) if e.message.trim().is_empty() => {
                Err("failure envelope has an empty error message".into())
            }
            _ => Ok(()),
        }
    }

    /// Attach the `requestId` of the request this envelope answers.
    ///
    /// No production path calls this: a write op takes its `request_id` as an
    /// option and keys its journal/recovery records on it, but every response
    /// is built with `Envelope::success`/`Envelope::failure`, so the envelope
    /// field never reaches the wire. Its absence means "not reported", never
    /// "no request id".
    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }

    pub fn with_snapshot(mut self, snapshot: SnapshotInfo) -> Self {
        self.snapshot = Some(snapshot);
        self
    }

    pub fn with_epistemics(mut self, epistemics: Epistemics) -> Self {
        self.epistemics = Some(epistemics);
        self
    }

    /// Attach the byte-cap bookkeeping of a response that ran into a cap.
    ///
    /// No production path calls this either: the caps that bite today report
    /// themselves inside the op's own result (`search`'s `byte_cap` and
    /// `next_offset`, `history`/`diff`/`review`'s `byte_cap`/`truncated`/
    /// `next_cursor`, `plan`'s file cap) or as envelope `warnings`. A failure
    /// envelope carries no budget at all — a cap never fails a command — so
    /// its absence means "no cap information", never "no cap fired".
    pub fn with_budget(mut self, budget: BudgetInfo) -> Self {
        self.budget = Some(budget);
        self
    }

    pub fn with_warnings(mut self, warnings: Vec<Warning>) -> Self {
        self.warnings = warnings;
        self
    }

    /// Set all three Envelope v2 metadata fields at once: `snapshot`,
    /// `epistemics`, and `budget`. Each argument is `Option`-al — passing
    /// `None` leaves that field untouched. This is the convenience helper
    /// for daemon call sites that want to attach all repo-state metadata
    /// in one chained call instead of three separate `with_*` calls.
    pub fn with_metadata(
        mut self,
        snapshot: Option<SnapshotInfo>,
        epistemics: Option<Epistemics>,
        budget: Option<BudgetInfo>,
    ) -> Self {
        if let Some(s) = snapshot {
            self.snapshot = Some(s);
        }
        if let Some(e) = epistemics {
            self.epistemics = Some(e);
        }
        if let Some(b) = budget {
            self.budget = Some(b);
        }
        self
    }
}

/// Convenience specialization for `Envelope<serde_json::Value>`: the result
/// payload, or `Value::Null` when absent (failure envelopes carry no result).
/// This is the successor to the old ad-hoc `Response::data` field — callers
/// that previously read `resp.data` now read `resp.data()`.
impl Envelope<serde_json::Value> {
    pub fn data(&self) -> &serde_json::Value {
        self.result.as_ref().unwrap_or(&serde_json::Value::Null)
    }

    /// Consuming counterpart to [`Envelope::data`]: takes ownership of the
    /// result payload instead of cloning it. Same null-fallback behavior —
    /// `Value::Null` when the envelope carries no result (e.g. a failure
    /// envelope) — but avoids a deep clone of the JSON tree for call sites
    /// that already own the envelope and don't need it afterward.
    pub fn into_data(self) -> serde_json::Value {
        self.result.unwrap_or(serde_json::Value::Null)
    }

    /// The error message string, or a fallback when the envelope carries no
    /// error. Successor to the old `Response::error: Option<String>` field.
    pub fn error_message(&self) -> String {
        self.error
            .as_ref()
            .map_or_else(|| "unknown error".to_string(), |e| e.message.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;
    use serde_json::json;

    /// A response that crosses the daemon socket is printed from the parsed
    /// copy, an in-process one from the value itself: the same query must
    /// print the same bytes either way, so every float must parse back to
    /// the bits it was written from. `list-areas` printed cohesion 5/11 as
    /// `0.45454545454545453` in process and `0.4545454545454546` through
    /// the daemon before serde_json's `float_roundtrip`.
    #[test]
    fn floats_survive_the_wire_bit_for_bit() {
        let mut floats = vec![5.0 / 11.0, 18.0 / 41.0, 12.0 / 13.0, 26.0 / 27.0];
        for den in 2..200u32 {
            for num in 1..den {
                floats.push(f64::from(num) / f64::from(den));
            }
        }
        let sent = Envelope::success("clusters", json!({ "cohesion": floats }));
        let line = serde_json::to_string(&sent).unwrap();
        let back: Envelope<serde_json::Value> = serde_json::from_str(&line).unwrap();
        let parsed: Vec<u64> = back.result.unwrap()["cohesion"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap().to_bits())
            .collect();
        let written: Vec<u64> = floats.iter().map(|f| f.to_bits()).collect();
        let drifted = written.iter().zip(&parsed).filter(|(w, p)| w != p).count();
        assert_eq!(
            drifted,
            0,
            "{drifted} of {} floats changed on the wire",
            floats.len()
        );
    }

    /// Golden snapshot (M0 gate from `PLAN.md`: "golden envelope snapshots
    /// frozen"). This is a literal `assert_eq!` against a hand-written JSON
    /// value, not a snapshot-testing crate, so any accidental field rename
    /// or shape drift fails loudly and specifically rather than silently
    /// updating a stored fixture.
    #[test]
    fn golden_success_envelope() {
        let envelope: Envelope<serde_json::Value> =
            Envelope::success("ping", json!({"pong": true}))
                .with_request_id("req-1")
                .with_snapshot(SnapshotInfo {
                    token: Some("abcdef012345".into()),
                    head: Some("deadbeefcafefeed0000000000000000deadbee".into()),
                    branch: Some("main".into()),
                    dirty: vec!["src/a.rs".into()],
                    dirty_count: None,
                })
                .with_epistemics(Epistemics {
                    closed_world: true,
                    lower_bound: false,
                    basis: "index".into(),
                    staleness_ms: Some(120),
                    confidence: None,
                    extraction_limits: Vec::new(),
                })
                .with_budget(BudgetInfo {
                    byte_cap: 1024,
                    used: 10,
                    truncated: false,
                    cursor: None,
                });

        let actual = serde_json::to_value(&envelope).unwrap();
        let expected = json!({
            "ok": true,
            "op": "ping",
            "protocol": 1,
            "requestId": "req-1",
            "snapshot": {
                "token": "abcdef012345",
                "head": "deadbeefcafefeed0000000000000000deadbee",
                "branch": "main",
                "dirty": ["src/a.rs"],
            },
            "epistemics": {
                "closed_world": true,
                "lower_bound": false,
                "basis": "index",
                "staleness_ms": 120,
            },
            "budget": {
                "byteCap": 1024,
                "used": 10,
                "truncated": false,
            },
            "result": {"pong": true},
            "error": null,
        });
        assert_eq!(actual, expected);
    }

    #[test]
    fn golden_failure_envelope() {
        let envelope: Envelope<serde_json::Value> = Envelope::failure(
            "push",
            PixelError::new(ErrorCode::NonFastForward, "ref moved"),
        )
        .with_request_id("req-2");

        let actual = serde_json::to_value(&envelope).unwrap();
        let expected = json!({
            "ok": false,
            "op": "push",
            "protocol": 1,
            "requestId": "req-2",
            "result": null,
            "error": {
                "code": "NON_FAST_FORWARD",
                "message": "ref moved",
            },
        });
        assert_eq!(actual, expected);
    }

    #[test]
    fn envelope_round_trips_through_json() {
        let envelope: Envelope<serde_json::Value> = Envelope::success("status", json!({"a": 1}));
        let text = serde_json::to_string(&envelope).unwrap();
        let parsed: Envelope<serde_json::Value> = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, envelope);
    }

    #[test]
    fn warnings_field_is_omitted_when_empty() {
        let envelope: Envelope<serde_json::Value> = Envelope::success("status", json!(null));
        let value = serde_json::to_value(&envelope).unwrap();
        assert!(value.get("warnings").is_none());
    }

    /// Envelope v2: when snapshot/epistemics/budget are `None` they are
    /// omitted entirely from the serialized JSON (not `null`), preserving
    /// backward compat with v1 clients that don't know about these fields.
    #[test]
    fn v2_fields_omitted_when_none() {
        let envelope: Envelope<serde_json::Value> = Envelope::success("search", json!({"hits": 0}));
        let value = serde_json::to_value(&envelope).unwrap();
        assert!(value.get("snapshot").is_none());
        assert!(value.get("epistemics").is_none());
        assert!(value.get("budget").is_none());
    }

    /// Envelope v2: all three metadata fields serialize with the correct
    /// shapes and casing when populated, including `dirty` as a file list
    /// and `staleness_ms` as an optional integer.
    #[test]
    fn v2_fields_serialize_with_correct_shapes() {
        let envelope: Envelope<serde_json::Value> =
            Envelope::success("inspect", json!({"head": "abc"}))
                .with_snapshot(SnapshotInfo {
                    token: None,
                    head: Some("abc123".into()),
                    branch: Some("feature/x".into()),
                    dirty: vec!["src/main.rs".into(), "README.md".into()],
                    dirty_count: None,
                })
                .with_epistemics(Epistemics {
                    closed_world: false,
                    lower_bound: true,
                    basis: "graph".into(),
                    staleness_ms: None,
                    confidence: None,
                    extraction_limits: Vec::new(),
                })
                .with_budget(BudgetInfo {
                    byte_cap: 8192,
                    used: 4096,
                    truncated: true,
                    cursor: Some("page-2".into()),
                });

        let value = serde_json::to_value(&envelope).unwrap();

        // snapshot: token omitted (None), dirty is a file list — the
        // `inspect`/`review` shape; retrieval ops ship `dirty_count` instead
        // (see `SnapshotInfo::compact`).
        assert_eq!(value["snapshot"]["head"], "abc123");
        assert!(value["snapshot"].get("dirty_count").is_none());
        assert_eq!(value["snapshot"]["branch"], "feature/x");
        assert!(value["snapshot"].get("token").is_none());
        assert_eq!(
            value["snapshot"]["dirty"],
            json!(["src/main.rs", "README.md"])
        );

        // epistemics: staleness_ms omitted (None)
        assert_eq!(value["epistemics"]["closed_world"], false);
        assert_eq!(value["epistemics"]["lower_bound"], true);
        assert_eq!(value["epistemics"]["basis"], "graph");
        assert!(value["epistemics"].get("staleness_ms").is_none());

        // budget: cursor present, byteCap is camelCase
        assert_eq!(value["budget"]["byteCap"], 8192);
        assert_eq!(value["budget"]["used"], 4096);
        assert_eq!(value["budget"]["truncated"], true);
        assert_eq!(value["budget"]["cursor"], "page-2");
    }

    /// Envelope v2: `with_metadata` sets all three fields in one call.
    #[test]
    fn with_metadata_sets_all_three() {
        let envelope: Envelope<serde_json::Value> = Envelope::success("diff", json!({}))
            .with_metadata(
                Some(SnapshotInfo {
                    token: None,
                    head: Some("abc".into()),
                    branch: None,
                    dirty: vec![],
                    dirty_count: None,
                }),
                Some(Epistemics::default()),
                Some(BudgetInfo {
                    byte_cap: 100,
                    used: 50,
                    truncated: false,
                    cursor: None,
                }),
            );

        assert!(envelope.snapshot.is_some());
        assert!(envelope.epistemics.is_some());
        assert!(envelope.budget.is_some());
    }

    /// Envelope v2: `with_metadata` with all `None` leaves fields untouched.
    #[test]
    fn with_metadata_none_leaves_untouched() {
        let envelope: Envelope<serde_json::Value> =
            Envelope::success("ping", json!({})).with_metadata(None, None, None);

        assert!(envelope.snapshot.is_none());
        assert!(envelope.epistemics.is_none());
        assert!(envelope.budget.is_none());
    }
}

#[cfg(test)]
mod validate_tests {
    use super::*;
    use crate::error::{ErrorCode, PixelError};
    use serde_json::json;

    fn err(msg: &str) -> PixelError {
        PixelError::new(ErrorCode::InvalidInput, msg)
    }

    #[test]
    fn constructors_produce_valid_envelopes() {
        let ok: Envelope<serde_json::Value> = Envelope::success("search", json!({}));
        assert_eq!(ok.validate(), Ok(()));
        let failed: Envelope<serde_json::Value> = Envelope::failure("search", err("bad regex"));
        assert_eq!(failed.validate(), Ok(()));
    }

    /// A success without a payload would make a caller read `null` as an
    /// answer. A failure without an error would make it retry blind. Both
    /// shapes are unrepresentable through the constructors, but a
    /// hand-built or deserialized envelope can carry them — validate must
    /// catch each one by name.
    #[test]
    fn mixed_ok_and_error_states_are_rejected() {
        let mut e: Envelope<serde_json::Value> = Envelope::success("op", json!(1));
        e.error = Some(err("x"));
        assert!(e.validate().unwrap_err().contains("also carries an error"));

        let mut e: Envelope<serde_json::Value> = Envelope::success("op", json!(1));
        e.result = None;
        assert!(e.validate().unwrap_err().contains("no result"));

        let mut e: Envelope<serde_json::Value> = Envelope::failure("op", err("x"));
        e.error = None;
        assert!(e.validate().unwrap_err().contains("no error"));

        let mut e: Envelope<serde_json::Value> = Envelope::failure("op", err("x"));
        e.result = Some(json!(1));
        assert!(e.validate().unwrap_err().contains("also carries a result"));

        let e: Envelope<serde_json::Value> = Envelope::failure("op", err("  "));
        assert!(e.validate().unwrap_err().contains("empty error message"));
    }

    #[test]
    fn protocol_and_op_are_checked() {
        let mut e: Envelope<serde_json::Value> = Envelope::success("op", json!(1));
        e.protocol = ENVELOPE_PROTOCOL_VERSION + 1;
        assert!(e.validate().unwrap_err().starts_with("protocol"));

        let e: Envelope<serde_json::Value> = Envelope::success("", json!(1));
        assert_eq!(e.validate(), Err("op is empty".to_string()));
    }

    /// A wire line from an older or foreign producer goes through serde,
    /// not the constructors, so the deserialized path must be validated
    /// the same way.
    // `unwrap_response` in the CLI reads the payload and the error message
    // through these accessors: a success envelope must hand back its result
    // untouched, a failure envelope must degrade to `Null` plus the daemon's
    // message, and a malformed failure (no error) must still print something.
    #[test]
    fn value_accessors_expose_result_or_error() {
        let ok = Envelope::success("inspect", json!({"head": "abc"}));
        assert_eq!(ok.data(), &json!({"head": "abc"}));
        assert_eq!(ok.error_message(), "unknown error");
        assert_eq!(ok.into_data(), json!({"head": "abc"}));

        let failed = Envelope::<serde_json::Value>::failure("inspect", err("no repo"));
        assert_eq!(failed.data(), &serde_json::Value::Null);
        assert_eq!(failed.error_message(), "no repo");
        assert_eq!(failed.into_data(), serde_json::Value::Null);
    }

    #[test]
    fn deserialized_wire_line_is_validated() {
        let line = r#"{"ok":true,"op":"ping","protocol":1,"result":null,"error":null}"#;
        let e: Envelope<serde_json::Value> = serde_json::from_str(line).unwrap();
        assert!(e.validate().unwrap_err().contains("no result"));
    }
}
