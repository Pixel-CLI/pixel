//! Epistemics contract: how honest is this answer about its own completeness.
//!
//! Mirrors gitpixel's "epistemic envelope" concept (closed-world vs
//! lower-bound results) generalized to every op, per `PLAN.md`'s Envelope v2
//! design.

use serde::{Deserialize, Serialize};

use crate::SourceEpistemics;

/// `{closed_world, lower_bound, basis, staleness_ms, confidence}`.
///
/// The default deliberately makes no completeness claim. Producers must use
/// source-native evidence to establish a closed world.
///
/// Envelope v2 shape: `basis` is a single descriptive `String` (e.g.
/// `"graph"` or `"index"`) and `staleness_ms` is `Option<u64>` — `None`
/// means "not stale / unknown", a present value is the measured staleness.
/// `confidence` is an optional epistemic confidence label such as
/// `"resolved"`, `"ranked"`, or `"unresolved"` for retrieval ops that
/// distinguish single-match certainty from ordered candidates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Epistemics {
    pub closed_world: bool,
    pub lower_bound: bool,
    #[serde(default)]
    pub basis: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staleness_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<String>,
}

impl Default for Epistemics {
    fn default() -> Self {
        Epistemics {
            closed_world: false,
            lower_bound: true,
            basis: String::new(),
            staleness_ms: None,
            confidence: None,
        }
    }
}

impl Epistemics {
    pub fn from_sources(basis: impl Into<String>, sources: &[SourceEpistemics]) -> Self {
        let closed_world = SourceEpistemics::establishes_closed_world(sources);
        Self {
            closed_world,
            lower_bound: !closed_world,
            basis: basis.into(),
            staleness_ms: sources.iter().filter_map(|source| source.freshness_ms).max(),
            confidence: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_an_explicitly_bounded_answer() {
        let epistemics = Epistemics::default();
        assert!(!epistemics.closed_world);
        assert!(epistemics.lower_bound);
        assert!(epistemics.basis.is_empty());
        assert_eq!(epistemics.staleness_ms, None);
        assert_eq!(epistemics.confidence, None);
    }

    #[test]
    fn serializes_with_snake_case_fields() {
        let value = serde_json::to_value(Epistemics::default()).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "closed_world": false,
                "lower_bound": true,
                "basis": "",
            })
        );
    }

    #[test]
    fn derives_completeness_only_from_complete_fresh_sources() {
        let source = SourceEpistemics {
            source: "facts".into(),
            required: true,
            coverage: crate::SourceCoverage::Complete,
            basis: crate::EvidenceBasis::Exact,
            snapshot: Some("head:abc".into()),
            freshness_ms: Some(0),
            exclusions: vec![],
            caps: vec![],
        };
        let epistemics = Epistemics::from_sources("facts", &[source]);
        assert!(epistemics.closed_world);
        assert!(!epistemics.lower_bound);
    }

    #[test]
    fn staleness_ms_serializes_when_present() {
        let epistemics = Epistemics {
            closed_world: false,
            lower_bound: true,
            basis: "graph".into(),
            staleness_ms: Some(5_000),
            confidence: None,
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
