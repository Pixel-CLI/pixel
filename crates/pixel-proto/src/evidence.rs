use serde::{Deserialize, Serialize};

/// How completely a source was searched for a query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceCoverage {
    Complete,
    Partial,
    Unavailable,
    Stale,
}

/// The strongest deterministic method used to produce evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceBasis {
    Exact,
    CompilerPrecise,
    Syntactic,
    Lexical,
    Heuristic,
}

/// A limit that prevented a source from returning its entire result set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapHit {
    pub name: String,
    pub limit: u64,
    /// What could not be returned because this cap was hit.
    pub omitted_domain: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continuation: Option<String>,
}

/// Source-native facts needed to state deterministic query certainty safely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceEpistemics {
    pub source: String,
    pub required: bool,
    pub coverage: SourceCoverage,
    pub basis: EvidenceBasis,
    /// The source snapshot that was queried, when the source exposes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    /// Measured source age. `None` means the source did not prove freshness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freshness_ms: Option<u64>,
    /// Source-specific exclusions applied while answering the query.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclusions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caps: Vec<CapHit>,
}

impl SourceEpistemics {
    pub fn establishes_closed_world(sources: &[Self]) -> bool {
        let required: Vec<_> = sources.iter().filter(|source| source.required).collect();
        !required.is_empty()
            && required.iter().all(|source| {
                source.coverage == SourceCoverage::Complete
                    && source.freshness_ms == Some(0)
                    && source.exclusions.is_empty()
                    && source.caps.is_empty()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(coverage: SourceCoverage) -> SourceEpistemics {
        SourceEpistemics {
            source: "facts".into(),
            required: true,
            coverage,
            basis: EvidenceBasis::Exact,
            snapshot: Some("head:abc".into()),
            freshness_ms: Some(0),
            exclusions: vec![],
            caps: vec![],
        }
    }

    #[test]
    fn closed_world_requires_complete_uncapped_required_sources() {
        assert!(SourceEpistemics::establishes_closed_world(&[source(
            SourceCoverage::Complete
        )]));
        assert!(!SourceEpistemics::establishes_closed_world(&[source(
            SourceCoverage::Partial
        )]));

        let mut capped = source(SourceCoverage::Complete);
        capped.caps.push(CapHit {
            name: "rows".into(),
            limit: 20,
            omitted_domain: "remaining rows".into(),
            continuation: Some("next".into()),
        });
        assert!(!SourceEpistemics::establishes_closed_world(&[capped]));
    }

    #[test]
    fn closed_world_rejects_vacuous_success() {
        assert!(!SourceEpistemics::establishes_closed_world(&[]));
        let mut optional = source(SourceCoverage::Complete);
        optional.required = false;
        assert!(!SourceEpistemics::establishes_closed_world(&[optional]));
    }

    #[test]
    fn stale_or_excluded_source_is_not_complete() {
        let mut stale = source(SourceCoverage::Complete);
        stale.freshness_ms = Some(1);
        assert!(!SourceEpistemics::establishes_closed_world(&[stale]));

        let mut excluded = source(SourceCoverage::Complete);
        excluded.exclusions.push("generated files".into());
        assert!(!SourceEpistemics::establishes_closed_world(&[excluded]));
    }

    #[test]
    fn schema_is_stable_json() {
        let json = serde_json::to_value(source(SourceCoverage::Complete)).unwrap();
        assert_eq!(json["coverage"], "complete");
        assert_eq!(json["basis"], "exact");
    }
}
