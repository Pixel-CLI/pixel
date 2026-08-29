//! `Op`: a type-level mirror of `pixel_daemon::api::Request`
//! (`crates/pixel-daemon/src/api.rs`), reproduced here so the shared
//! contract crate carries the wire-format definition rather than the daemon
//! crate.
//!
//! This is **not yet wired into `pixel-daemon`** — `Request` there remains
//! the live type the daemon dispatches on. Swapping the daemon over to this
//! `Op` (and re-deriving CLI args / MCP tool schemas from it, per `PLAN.md`
//! A2) is a separate future step. Until then, this enum's only job is to
//! exist, compile, and round-trip identically to `Request`'s current wire
//! format so it is ready to be swapped in without a contract change.
//!
//! Variants, field shapes, and the `#[serde(tag = "op", rename_all =
//! "snake_case")]` wire convention are copied verbatim from `Request`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Ping,
    /// Transcript-corpus operation, served only by a recall daemon (a repo
    /// daemon answers it with an "unsupported" error). `action` selects the
    /// recall op ("search" | "ask"); `params` is its argument object.
    Recall {
        action: String,
        #[serde(default)]
        params: Value,
    },
    Search {
        pattern: String,
        #[serde(default)]
        json: bool,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        offset: Option<usize>,
        /// Repo-relative path prefixes to restrict the search to (rg-style
        /// multi-path invocations). None/empty = whole repo.
        #[serde(default)]
        paths: Option<Vec<String>>,
    },
    /// Sniper target list: task description in, closed prioritized file
    /// list (P0/P1/P2) out.
    Targets {
        task: String,
        #[serde(default)]
        limit: Option<usize>,
    },
    Symbol {
        name: String,
    },
    Context {
        uid: String,
        #[serde(default)]
        budget_tokens: Option<usize>,
    },
    Impact {
        uid_or_name: String,
        direction: String,
        #[serde(default)]
        depth: Option<u32>,
    },
    Uses {
        uid_or_name: String,
        /// "callers" | "callees"
        role: String,
        #[serde(default)]
        offset: Option<usize>,
    },
    Trace {
        from: String,
        to: String,
    },
    Processes {
        #[serde(default)]
        offset: Option<usize>,
    },
    Clusters {
        #[serde(default)]
        offset: Option<usize>,
    },
    Changes {
        #[serde(default)]
        base: Option<String>,
        #[serde(default)]
        offset: Option<usize>,
    },
    Graph {},
    Status {},
    Shutdown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ping_serializes_as_bare_tag() {
        let value = serde_json::to_value(Op::Ping).unwrap();
        assert_eq!(value, json!({"op": "ping"}));
    }

    #[test]
    fn search_serializes_with_snake_case_tag_and_fields() {
        let op = Op::Search {
            pattern: "fn main".into(),
            json: true,
            limit: Some(50),
            offset: None,
            paths: Some(vec!["src".into()]),
        };
        let value = serde_json::to_value(&op).unwrap();
        assert_eq!(
            value,
            json!({
                "op": "search",
                "pattern": "fn main",
                "json": true,
                "limit": 50,
                "offset": null,
                "paths": ["src"],
            })
        );
    }

    #[test]
    fn targets_omits_defaulted_limit_on_deserialize() {
        let op: Op = serde_json::from_value(json!({"op": "targets", "task": "fix the bug"}))
            .unwrap();
        assert_eq!(
            op,
            Op::Targets {
                task: "fix the bug".into(),
                limit: None,
            }
        );
    }

    #[test]
    fn impact_round_trips() {
        let op = Op::Impact {
            uid_or_name: "foo#1".into(),
            direction: "upstream".into(),
            depth: Some(3),
        };
        let text = serde_json::to_string(&op).unwrap();
        let parsed: Op = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, op);
    }

    #[test]
    fn graph_and_status_serialize_as_empty_object_variants() {
        assert_eq!(serde_json::to_value(Op::Graph {}).unwrap(), json!({"op": "graph"}));
        assert_eq!(serde_json::to_value(Op::Status {}).unwrap(), json!({"op": "status"}));
    }

    #[test]
    fn shutdown_serializes_as_bare_tag() {
        let value = serde_json::to_value(Op::Shutdown).unwrap();
        assert_eq!(value, json!({"op": "shutdown"}));
    }
}
