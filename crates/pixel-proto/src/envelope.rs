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

use crate::budget::Budget;
use crate::epistemics::Epistemics;
use crate::error::PixelError;
use crate::snapshot::Snapshot;
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
    #[serde(default)]
    pub snapshot: Option<Snapshot>,
    #[serde(default)]
    pub epistemics: Option<Epistemics>,
    #[serde(default)]
    pub budget: Option<Budget>,
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

    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }

    pub fn with_snapshot(mut self, snapshot: Snapshot) -> Self {
        self.snapshot = Some(snapshot);
        self
    }

    pub fn with_epistemics(mut self, epistemics: Epistemics) -> Self {
        self.epistemics = Some(epistemics);
        self
    }

    pub fn with_budget(mut self, budget: Budget) -> Self {
        self.budget = Some(budget);
        self
    }

    pub fn with_warnings(mut self, warnings: Vec<Warning>) -> Self {
        self.warnings = warnings;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;
    use crate::snapshot::SnapshotToken;
    use serde_json::json;

    /// Golden snapshot (M0 gate from `PLAN.md`: "golden envelope snapshots
    /// frozen"). This is a literal `assert_eq!` against a hand-written JSON
    /// value, not a snapshot-testing crate, so any accidental field rename
    /// or shape drift fails loudly and specifically rather than silently
    /// updating a stored fixture.
    #[test]
    fn golden_success_envelope() {
        let envelope: Envelope<serde_json::Value> = Envelope::success(
            "ping",
            json!({"pong": true}),
        )
        .with_request_id("req-1")
        .with_snapshot(Snapshot {
            token: Some(SnapshotToken::parse("abcdef012345").unwrap()),
            head: Some("deadbeefcafefeed0000000000000000deadbee".into()),
            branch: Some("main".into()),
            dirty: false,
        })
        .with_epistemics(Epistemics::default())
        .with_budget(Budget {
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
                "dirty": false,
            },
            "epistemics": {
                "closed_world": true,
                "lower_bound": false,
                "basis": [],
                "staleness_ms": 0,
            },
            "budget": {
                "byteCap": 1024,
                "used": 10,
                "truncated": false,
                "cursor": null,
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
            "snapshot": null,
            "epistemics": null,
            "budget": null,
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
}
