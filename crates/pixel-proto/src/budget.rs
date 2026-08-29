//! Budget contract: `{byteCap, used, truncated, cursor}` from `PLAN.md`'s
//! Envelope v2. Note the field is `byteCap` (camelCase) on the wire even
//! though sibling envelope sections (e.g. `epistemics`) are snake_case —
//! Envelope v2 mixes both, and this crate reproduces the literal field
//! names rather than imposing a blanket case rule.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Budget {
    #[serde(rename = "byteCap")]
    pub byte_cap: usize,
    pub used: usize,
    pub truncated: bool,
    #[serde(default)]
    pub cursor: Option<String>,
}

/// Envelope v2 `budget` field: `{byteCap, used, truncated, cursor}`.
///
/// Identical wire shape to [`Budget`] — the separate type exists so the v2
/// envelope can carry a distinct contract name even though the fields match
/// the v1 budget exactly. `byteCap` remains camelCase on the wire (Envelope
/// v2 mixes casing; see the crate-level docs in `envelope.rs`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetInfo {
    #[serde(rename = "byteCap")]
    pub byte_cap: usize,
    pub used: usize,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_cap_serializes_as_camel_case() {
        let budget = Budget {
            byte_cap: 1024,
            used: 10,
            truncated: false,
            cursor: None,
        };
        let value = serde_json::to_value(&budget).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"byteCap": 1024, "used": 10, "truncated": false, "cursor": null})
        );
    }

    #[test]
    fn budget_info_omits_none_cursor() {
        let budget = BudgetInfo {
            byte_cap: 4096,
            used: 100,
            truncated: true,
            cursor: None,
        };
        let value = serde_json::to_value(&budget).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"byteCap": 4096, "used": 100, "truncated": true})
        );
    }

    #[test]
    fn budget_info_serializes_cursor_when_present() {
        let budget = BudgetInfo {
            byte_cap: 4096,
            used: 100,
            truncated: true,
            cursor: Some("offset-42".into()),
        };
        let value = serde_json::to_value(&budget).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"byteCap": 4096, "used": 100, "truncated": true, "cursor": "offset-42"})
        );
    }
}
