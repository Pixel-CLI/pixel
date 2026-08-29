//! Epistemics contract: how honest is this answer about its own completeness.
//!
//! Mirrors gitpixel's "epistemic envelope" concept (closed-world vs
//! lower-bound results) generalized to every op, per `PLAN.md`'s Envelope v2
//! design.

use serde::{Deserialize, Serialize};

/// `{closed_world, lower_bound, basis, staleness_ms}`.
///
/// The default is the common case: a complete, fresh answer with nothing
/// left out and no staleness — `closed_world: true`, `lower_bound: false`,
/// an empty `basis`, and `staleness_ms: None`. Ops that could not close the
/// world (e.g. graph resolution gave up) must override this explicitly
/// rather than rely on the default.
///
/// Envelope v2 shape: `basis` is a single descriptive `String` (e.g.
/// `"graph"` or `"index"`) and `staleness_ms` is `Option<u64>` — `None`
/// means "not stale / unknown", a present value is the measured staleness.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Epistemics {
    pub closed_world: bool,
    pub lower_bound: bool,
    #[serde(default)]
    pub basis: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staleness_ms: Option<u64>,
}

impl Default for Epistemics {
    fn default() -> Self {
        Epistemics {
            closed_world: true,
            lower_bound: false,
            basis: String::new(),
            staleness_ms: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_the_complete_fresh_answer() {
        let epistemics = Epistemics::default();
        assert!(epistemics.closed_world);
        assert!(!epistemics.lower_bound);
        assert!(epistemics.basis.is_empty());
        assert_eq!(epistemics.staleness_ms, None);
    }

    #[test]
    fn serializes_with_snake_case_fields() {
        let value = serde_json::to_value(Epistemics::default()).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "closed_world": true,
                "lower_bound": false,
                "basis": "",
            })
        );
    }

    #[test]
    fn staleness_ms_serializes_when_present() {
        let epistemics = Epistemics {
            closed_world: false,
            lower_bound: true,
            basis: "graph".into(),
            staleness_ms: Some(5_000),
        };
        let value = serde_json::to_value(&epistemics).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "closed_world": false,
                "lower_bound": true,
                "basis": "graph",
                "staleness_ms": 5000,
            })
        );
    }
}
