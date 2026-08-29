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
/// an empty `basis`, and `staleness_ms: 0`. Ops that could not close the
/// world (e.g. graph resolution gave up) must override this explicitly
/// rather than rely on the default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Epistemics {
    pub closed_world: bool,
    pub lower_bound: bool,
    pub basis: Vec<String>,
    pub staleness_ms: u64,
}

impl Default for Epistemics {
    fn default() -> Self {
        Epistemics {
            closed_world: true,
            lower_bound: false,
            basis: Vec::new(),
            staleness_ms: 0,
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
        assert_eq!(epistemics.staleness_ms, 0);
    }

    #[test]
    fn serializes_with_snake_case_fields() {
        let value = serde_json::to_value(Epistemics::default()).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "closed_world": true,
                "lower_bound": false,
                "basis": [],
                "staleness_ms": 0,
            })
        );
    }
}
