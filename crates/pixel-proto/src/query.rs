use serde::{Deserialize, Serialize};

/// Explicit, source-aware retrieval intents understood by `pixel query`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QueryKind {
    Auto,
    Locate,
    Scope,
    Impact,
    HistoryRecovery,
    Status,
}

/// Whether the query compiler proved one recipe or can only rank candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryStatus {
    Resolved,
    Ranked,
    Partial,
    Unresolved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryPlan {
    pub recipe: String,
    pub operations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryResult {
    pub intent: String,
    pub status: QueryStatus,
    pub plan: Vec<QueryPlan>,
    #[serde(default)]
    pub evidence: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<String>,
}

/// Compiles only unmistakable V1 phrasings. Ambiguous prose is deliberately
/// returned as ranked plans rather than triggering broad retrieval.
pub fn compile_query(intent: &str, explicit: QueryKind) -> QueryResult {
    let normalized = intent.trim();
    let inferred = match explicit {
        QueryKind::Auto if normalized.starts_with("where is `") && normalized.ends_with('`') => {
            Some(QueryKind::Locate)
        }
        QueryKind::Auto if normalized.starts_with("what files implement ") => Some(QueryKind::Scope),
        QueryKind::Auto if normalized.starts_with("show impact of ") => Some(QueryKind::Impact),
        QueryKind::Auto if normalized.starts_with("restore ") || normalized.contains("working before") => {
            Some(QueryKind::HistoryRecovery)
        }
        QueryKind::Auto if normalized == "status" || normalized == "what changed" => Some(QueryKind::Status),
        QueryKind::Auto => None,
        kind => Some(kind),
    };
    let plan = match inferred {
        Some(QueryKind::Locate) => vec![QueryPlan { recipe: "locate.v1".into(), operations: vec!["resolve".into(), "search_on_unresolved".into()] }],
        Some(QueryKind::Scope) => vec![QueryPlan { recipe: "scope.v1".into(), operations: vec!["targets".into()] }],
        Some(QueryKind::Impact) => vec![QueryPlan { recipe: "impact.v1".into(), operations: vec!["impact".into()] }],
        Some(QueryKind::HistoryRecovery) => vec![QueryPlan { recipe: "history_recovery.v1".into(), operations: vec!["excavate".into()] }],
        Some(QueryKind::Status) => vec![QueryPlan { recipe: "status.v1".into(), operations: vec!["inspect".into(), "review".into(), "changes".into()] }],
        Some(QueryKind::Auto) | None => vec![
            QueryPlan { recipe: "locate.v1".into(), operations: vec!["resolve".into()] },
            QueryPlan { recipe: "scope.v1".into(), operations: vec!["targets".into()] },
        ],
    };
    QueryResult {
        intent: normalized.into(),
        status: if inferred.is_some() { QueryStatus::Resolved } else { QueryStatus::Ranked },
        plan,
        evidence: vec![],
        bundle: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_only_exact_auto_intents() {
        let result = compile_query("where is `login_user`?", QueryKind::Auto);
        assert_eq!(result.status, QueryStatus::Ranked);

        let result = compile_query("where is `login_user`", QueryKind::Auto);
        assert_eq!(result.status, QueryStatus::Resolved);
        assert_eq!(result.plan[0].recipe, "locate.v1");
    }

    #[test]
    fn explicit_kind_overrides_ambiguous_text() {
        let result = compile_query("explain login", QueryKind::Impact);
        assert_eq!(result.status, QueryStatus::Resolved);
        assert_eq!(result.plan[0].operations, ["impact"]);
    }
}
