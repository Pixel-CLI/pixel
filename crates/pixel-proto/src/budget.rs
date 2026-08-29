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
}
