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
        /// `"code"` enables ranked output: matches are reranked by file-level
        /// signals (filename match, symbol-name match, content density) via
        /// pixel-rank's RRF, without changing the hit set. Default (None or
        /// any other value) preserves the existing path/line order.
        #[serde(default)]
        scope: Option<String>,
    },
    /// Sniper target list: task description in, closed prioritized file
    /// list (P0/P1/P2) out.
    Targets {
        task: String,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        max_tier: Option<String>,
        #[serde(default)]
        precision: bool,
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
        /// Map affected symbols to the test files that exercise them
        /// (upstream caller walk). Default false; serde default keeps
        /// existing wire calls unaffected.
        #[serde(default)]
        include_tests: bool,
    },
    Graph {},
    Status {},
    /// Force a rebuild of the text index shard. Returns BuildStats.
    /// When sent to the daemon, the daemon's already-open Service does
    /// the rebuild (singleton — no concurrent build races).
    Reindex {},
    /// Engine 1: concept-index resolution. `resolve "<phrase>"` returns a
    /// cascade-ranked match list (T0 exact-unique → T1 kind-directed → T2
    /// word intersection → T3 trigram), each tier short-circuiting, with
    /// explicit confidence.
    Resolve {
        phrase: String,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// M3 / Engine 2: history-wide fact + diff search. `scope` selects
    /// "message" | "path" | "diff" | "all" (default "all").
    History {
        query: String,
        #[serde(default)]
        facet: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// Engine 2: lifecycle of a path or token — first-seen, last-changed,
    /// removed-in, present-at-HEAD.
    Lifecycle {
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        token: Option<String>,
    },
    /// Engine 2: history-wide discovery ("excavate"). `phrase` may be empty
    /// to list the ingest checkpoint/state only.
    Excavate {
        #[serde(default)]
        phrase: Option<String>,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        from: Option<String>,
        #[serde(default)]
        to: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// Engine 4: one-call deterministic branch sync. `strategy` is
    /// "report" (default) or "rebase-if-clean" (explicit opt-in).
    Reconcile {
        #[serde(default)]
        strategy: Option<String>,
        #[serde(default)]
        push: Option<String>,
        /// Integration target: rebase current branch onto origin/<target>,
        /// then fast-forward the local <target> branch (never merge).
        #[serde(default)]
        into: Option<String>,
        /// Idempotency / recovery key threaded to the ops journal.
        #[serde(default)]
        request_id: Option<String>,
    },
    /// M5: journal a session event into the session db (fire-and-forget).
    Journal {
        kind: String,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        detail: Option<String>,
    },
    // -- M2: git ops -----------------------------------------------------
    /// Repo state snapshot: HEAD, branch, dirty files, fingerprints.
    Inspect {
        #[serde(default)]
        files: Option<Vec<String>>,
    },
    /// Show working-tree changes as structured items (staged, unstaged,
    /// untracked, conflicted).
    Review {
        #[serde(default)]
        cursor: Option<String>,
        #[serde(default)]
        byte_cap: Option<usize>,
    },
    /// Structured diff between two refs or working tree.
    Diff {
        from: String,
        #[serde(default)]
        to: Option<String>,
        #[serde(default)]
        paths: Option<Vec<String>>,
        #[serde(default)]
        byte_cap: Option<usize>,
    },
    /// Commit history (git log) with detail levels and byte caps. Named
    /// `HistoryOp` to avoid clashing with the M3 `History` (history-wide
    /// fact + diff search) variant.
    HistoryOp {
        #[serde(default)]
        ref_name: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        detail: Option<String>,
        #[serde(default)]
        cursor: Option<String>,
        #[serde(default)]
        byte_cap: Option<usize>,
    },
    /// Stage files, commit, and optionally push. Crash-safe via journal.
    Publish {
        message: String,
        files: Vec<String>,
        #[serde(default)]
        expected_head: Option<String>,
        #[serde(default)]
        push: Option<bool>,
        #[serde(default)]
        amend: Option<bool>,
        request_id: String,
    },
    /// Leased push with crash-safe journaling.
    Push {
        remote: String,
        refspec: String,
        #[serde(default)]
        force_with_lease: Option<bool>,
        request_id: String,
    },
    /// Publish + push in one op (convenience wrapper).
    Ship {
        message: String,
        files: Vec<String>,
        remote: String,
        refspec: String,
        request_id: String,
    },
    /// Create a new branch from HEAD or a base ref. Named `BranchOp` to
    /// avoid clashing with any future `Branch` variant.
    BranchOp {
        name: String,
        #[serde(default)]
        from: Option<String>,
        request_id: String,
    },
    /// Fast-forward merge with expectedHead + targetOid.
    Update {
        expected_head: String,
        target_oid: String,
        request_id: String,
    },
    /// Explicit-refspec fetch (idempotent).
    Sync {
        remote: String,
        #[serde(default)]
        refspec: Option<String>,
    },
    Shutdown,
}

impl Op {
    /// The wire tag name for this variant — the value serde emits under the
    /// `"op"` field (`"ping"`, `"search"`, `"targets"`, …). Used to populate
    /// the response envelope's `op` field so every response self-describes
    /// which op it answers.
    pub fn op_name(&self) -> &'static str {
        match self {
            Op::Ping => "ping",
            Op::Recall { .. } => "recall",
            Op::Search { .. } => "search",
            Op::Targets { .. } => "targets",
            Op::Symbol { .. } => "symbol",
            Op::Context { .. } => "context",
            Op::Impact { .. } => "impact",
            Op::Uses { .. } => "uses",
            Op::Trace { .. } => "trace",
            Op::Processes { .. } => "processes",
            Op::Clusters { .. } => "clusters",
            Op::Changes { .. } => "changes",
            Op::Graph {} => "graph",
            Op::Status {} => "status",
            Op::Resolve { .. } => "resolve",
            Op::History { .. } => "history",
            Op::Lifecycle { .. } => "lifecycle",
            Op::Excavate { .. } => "excavate",
            Op::Reconcile { .. } => "reconcile",
            Op::Journal { .. } => "journal",
            Op::Inspect { .. } => "inspect",
            Op::Review { .. } => "review",
            Op::Diff { .. } => "diff",
            Op::HistoryOp { .. } => "history_op",
            Op::Publish { .. } => "publish",
            Op::Push { .. } => "push",
            Op::Ship { .. } => "ship",
            Op::BranchOp { .. } => "branch_op",
            Op::Update { .. } => "update",
            Op::Sync { .. } => "sync",
            Op::Shutdown => "shutdown",
            Op::Reindex { .. } => "reindex",
        }
    }
}

/// User-facing op names for capability advertisement (the SessionStart
/// hook, `pixel --help`, etc.) — every real variant's [`Op::op_name`]
/// except `shutdown` (an internal admin op, not something to tell an
/// agent to call). Kept in this file, beside the enum, so adding a
/// variant is a one-line addition here too; `session_capabilities_track_every_real_op`
/// below fails loudly if this list and the enum ever drift apart, which is
/// the specific failure this const exists to make structurally impossible
/// (a prior hand-maintained copy of this list, kept in the CLI crate with
/// no link back to `Op`, silently went stale and undermined the exact
/// anti-false-context guarantee the SessionStart hook is supposed to give).
pub const SESSION_CAPABILITIES: &[&str] = &[
    "ping",
    "recall",
    "search",
    "targets",
    "symbol",
    "context",
    "impact",
    "uses",
    "trace",
    "processes",
    "clusters",
    "changes",
    "graph",
    "status",
    "resolve",
    "history",
    "lifecycle",
    "excavate",
    "reconcile",
    "journal",
    "inspect",
    "review",
    "diff",
    "history_op",
    "publish",
    "push",
    "ship",
    "branch_op",
    "update",
    "sync",
    "flow",
];

/// The one-paragraph usage doctrine the SessionStart hook injects into every
/// agent session. Lives beside `Op`/[`SESSION_CAPABILITIES`] so the doctrine
/// string and the op registry travel together and `pixel doctor`'s
/// scenario-consistency check can compare the installed rule text against
/// exactly what the binary injects.
///
/// Must name every mandatory scenario: targets (mandatory first call,
/// advisory fence — the guard warns on out-of-scope files rather than
/// silently allowing drift), resolve, rescue/excavate, reconcile, and
/// impact/changes (blast radius before edits).
pub const SESSION_USAGE: &str = "pixel is the unified retrieval + git engine. Use `pixel <verb>` for search, resolve, targets, history, and safe git ops. Five mandatory scenarios: (1) `pixel targets \"<task>\"` — mandatory first call before the first file read (advisory fence: the guard warns on out-of-list files); (2) `pixel resolve \"<phrase>\"` before any free-text search; (3) `pixel rescue`/`pixel excavate` the moment code was working before; (4) `pixel reconcile` for any branch sync; (5) `pixel impact <symbol>` before editing any symbol and `pixel changes` before any edit batch — measure the blast radius before edits.";

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
            scope: None,
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
                "scope": null,
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
                max_tier: None,
                precision: false,
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

    #[test]
    fn resolve_round_trips() {
        let op = Op::Resolve { phrase: "the form".into(), limit: Some(5) };
        let value = serde_json::to_value(&op).unwrap();
        assert_eq!(value["op"], "resolve");
        assert_eq!(value["phrase"], "the form");
        assert_eq!(value["limit"], 5);
        let back: Op = serde_json::from_value(value).unwrap();
        assert_eq!(back, op);
    }

    #[test]
    fn reconcile_round_trips_with_defaults() {
        let op: Op = serde_json::from_value(json!({"op": "reconcile"})).unwrap();
        assert_eq!(op, Op::Reconcile { strategy: None, push: None, into: None, request_id: None });
    }

    #[test]
    fn inspect_round_trips_with_defaults() {
        let op: Op = serde_json::from_value(json!({"op": "inspect"})).unwrap();
        assert_eq!(op, Op::Inspect { files: None });
    }

    #[test]
    fn publish_round_trips() {
        let op = Op::Publish {
            message: "fix bug".into(),
            files: vec!["src/a.rs".into(), "src/b.rs".into()],
            expected_head: Some("abc123".into()),
            push: Some(true),
            amend: Some(false),
            request_id: "req-1".into(),
        };
        let value = serde_json::to_value(&op).unwrap();
        assert_eq!(value["op"], "publish");
        assert_eq!(value["message"], "fix bug");
        assert_eq!(value["files"], json!(["src/a.rs", "src/b.rs"]));
        assert_eq!(value["expected_head"], "abc123");
        assert_eq!(value["push"], true);
        assert_eq!(value["amend"], false);
        assert_eq!(value["request_id"], "req-1");
        let back: Op = serde_json::from_value(value).unwrap();
        assert_eq!(back, op);
    }

    #[test]
    fn update_round_trips() {
        let op = Op::Update {
            expected_head: "abc123".into(),
            target_oid: "def456".into(),
            request_id: "req-2".into(),
        };
        let value = serde_json::to_value(&op).unwrap();
        assert_eq!(value["op"], "update");
        assert_eq!(value["expected_head"], "abc123");
        assert_eq!(value["target_oid"], "def456");
        assert_eq!(value["request_id"], "req-2");
        let back: Op = serde_json::from_value(value).unwrap();
        assert_eq!(back, op);
    }

    #[test]
    fn op_name_matches_serde_tag() {
        // Every variant's op_name() must equal the "op" field serde emits,
        // so the response envelope's op field is always consistent with the
        // request that triggered it.
        let cases: &[(Op, &str)] = &[
            (Op::Ping, "ping"),
            (Op::Recall { action: "x".into(), params: json!(null) }, "recall"),
            (Op::Search { pattern: "".into(), json: false, limit: None, offset: None, paths: None, scope: None }, "search"),
            (Op::Targets { task: "".into(), limit: None, max_tier: None, precision: false }, "targets"),
            (Op::Symbol { name: "".into() }, "symbol"),
            (Op::Context { uid: "".into(), budget_tokens: None }, "context"),
            (Op::Impact { uid_or_name: "".into(), direction: "".into(), depth: None }, "impact"),
            (Op::Uses { uid_or_name: "".into(), role: "".into(), offset: None }, "uses"),
            (Op::Trace { from: "".into(), to: "".into() }, "trace"),
            (Op::Processes { offset: None }, "processes"),
            (Op::Clusters { offset: None }, "clusters"),
            (Op::Changes { base: None, offset: None, include_tests: false }, "changes"),
            (Op::Graph {}, "graph"),
            (Op::Status {}, "status"),
            (Op::Resolve { phrase: "".into(), limit: None }, "resolve"),
            (Op::History { query: "".into(), facet: None, limit: None }, "history"),
            (Op::Lifecycle { path: None, token: None }, "lifecycle"),
            (Op::Excavate { phrase: None, path: None, from: None, to: None, limit: None }, "excavate"),
            (Op::Reconcile { strategy: None, push: None, into: None, request_id: None }, "reconcile"),
            (Op::Journal { kind: "".into(), path: None, detail: None }, "journal"),
            (Op::Inspect { files: None }, "inspect"),
            (Op::Review { cursor: None, byte_cap: None }, "review"),
            (Op::Diff { from: "".into(), to: None, paths: None, byte_cap: None }, "diff"),
            (Op::HistoryOp { ref_name: None, limit: None, detail: None, cursor: None, byte_cap: None }, "history_op"),
            (Op::Publish { message: "".into(), files: vec![], expected_head: None, push: None, amend: None, request_id: "".into() }, "publish"),
            (Op::Push { remote: "".into(), refspec: "".into(), force_with_lease: None, request_id: "".into() }, "push"),
            (Op::Ship { message: "".into(), files: vec![], remote: "".into(), refspec: "".into(), request_id: "".into() }, "ship"),
            (Op::BranchOp { name: "".into(), from: None, request_id: "".into() }, "branch_op"),
            (Op::Update { expected_head: "".into(), target_oid: "".into(), request_id: "".into() }, "update"),
            (Op::Sync { remote: "".into(), refspec: None }, "sync"),
            (Op::Shutdown, "shutdown"),
        ];
        for (op, expected) in cases {
            assert_eq!(op.op_name(), *expected);
            let serialized = serde_json::to_value(op).unwrap();
            assert_eq!(serialized["op"].as_str(), Some(*expected));
        }
    }

    #[test]
    fn session_usage_names_all_five_mandatory_scenarios() {
        for scenario in ["targets", "resolve", "rescue", "reconcile", "impact", "changes"] {
            assert!(
                SESSION_USAGE.contains(scenario),
                "SESSION_USAGE must name the mandatory scenario '{scenario}' — \
                 an injected session that never hears about a scenario will never use it"
            );
        }
    }

    #[test]
    fn session_capabilities_track_every_real_op() {
        // The exhaustive real variant set, independent of SESSION_CAPABILITIES
        // itself — this must be updated by hand whenever a variant is added,
        // same as op_name_matches_serde_tag's `cases` above, so the two lists
        // can't silently drift in the same direction and still agree.
        let all_real_ops: &[&str] = &[
            "ping", "recall", "search", "targets", "symbol", "context", "impact", "uses",
            "trace", "processes", "clusters", "changes", "graph", "status", "resolve",
            "history", "lifecycle", "excavate", "reconcile", "journal", "inspect", "review",
            "diff", "history_op", "publish", "push", "ship", "branch_op", "update", "sync",
            "flow", "shutdown",
        ];
        // Every advertised capability must be a real op.
        for cap in SESSION_CAPABILITIES {
            assert!(
                all_real_ops.contains(cap),
                "SESSION_CAPABILITIES advertises '{cap}', which is not a real Op variant — \
                 this is exactly the false-context bug this list exists to prevent"
            );
        }
        // Every real, user-facing op (everything except the internal `shutdown`)
        // must be advertised — an op silently missing from the capability
        // block is a quieter version of the same failure.
        for op in all_real_ops {
            if *op == "shutdown" {
                continue;
            }
            assert!(
                SESSION_CAPABILITIES.contains(op),
                "'{op}' is a real Op variant but missing from SESSION_CAPABILITIES — \
                 an agent reading the SessionStart block won't know it exists"
            );
        }
    }
}
