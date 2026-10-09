// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `Op`: the daemon request type, defined in the shared contract crate so
//! the daemon and CLI share one enum — `crates/pixel-daemon/src/api.rs`
//! re-exports it (`pub use pixel_proto::Op as Request`) and dispatches on
//! it.
//!
//! Re-deriving CLI args from it (per `PLAN.md` A2) is a separate future
//! step.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Ping,
    /// Transcript-corpus operation, served only by a recall daemon (a repo
    /// daemon answers it with an "unsupported" error). `action` selects the
    /// recall op ("search-content" | "search-meaning"); `params` is its argument object.
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
        /// `-g/--glob` rules with ripgrep's semantics: a leading `!`
        /// excludes, the last matching rule decides, and with any include
        /// rule a path no rule matches is left out. Empty = no glob filter.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        globs: Vec<String>,
        /// `-t/--type` names (`rust`, `ts`, `md`, ...). Empty = every type.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        types: Vec<String>,
    },
    /// Sniper target list: task description in, closed prioritized file
    /// list (P0/P1/P2) out. With `regions`, the response also carries the
    /// symbol-level regions manifest (`.pixel/regions.json`).
    Targets {
        task: String,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        max_tier: Option<String>,
        #[serde(default)]
        precision: bool,
        /// Attach the regions manifest: symbol line ranges, conservative
        /// conflict pairs, merge-order layers, and declared shared files.
        #[serde(default)]
        regions: bool,
    },
    /// Deterministic prompt-start file facts from already-published indexes.
    /// This operation never builds or refreshes either index; unavailable or
    /// stale inputs are reported in a typed successful result.
    TargetsFacts {
        task: String,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// Natural-language retrieval over the code chunks whose embeddings stay
    /// resident in the daemon: ranked leads (file, lines, symbol, snippet),
    /// never a verdict on relevance. Answers `unavailable` with a reason
    /// instead of waiting while the vectors are cold, stale or rebuilding
    /// (a background build is started), so a deadline-bounded caller falls
    /// back. Never downloads the model.
    Meaning {
        query: String,
        /// Leads returned; the daemon's default when absent, capped.
        #[serde(default)]
        limit: Option<usize>,
    },
    Symbol {
        name: String,
    },
    /// All signatures in a file — the "list-signatures" view at ~10% of Read cost.
    Skeleton {
        file: String,
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
    /// Deterministic pre-review: the `what-changed` engine plus the two
    /// mechanical passes an LLM review would otherwise start from scratch
    /// (credential-shaped added lines, change-propagation divergence).
    ReviewGate {
        #[serde(default)]
        base: Option<String>,
    },
    /// Build the code graph. By default a full rebuild, whatever is stored.
    /// With `if_stale`, the stored graph is kept when its signature still
    /// matches the tree and updated in place when few files drifted: a
    /// restored or already-current graph costs one walk, not a rebuild. An
    /// older daemon ignores the field and rebuilds, which is the safe side.
    Graph {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        if_stale: bool,
    },
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
        /// Answer from the facts db exactly as it stands: never ingest,
        /// never create it, never spawn the warmer — an absent db is an
        /// error. Omitted on the wire when false so pre-flag requests
        /// serialize identically; an older daemon ignores the flag and
        /// answers the ingesting path.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        read_only: bool,
    },
    /// Engine 2: lifecycle of a path or token — first-seen, last-changed,
    /// removed-in, present-at-HEAD.
    Lifecycle {
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        token: Option<String>,
        /// Same contract as `History::read_only`.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        read_only: bool,
    },
    /// Engine 2: history-wide discovery ("dig-history"). `phrase` may be empty
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
        /// Same contract as `History::read_only`.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        read_only: bool,
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
    /// P2·2: human notes — durable annotations keyed by `file` + `target`
    /// (a symbol `name` or concept `norm`) that survive rebuilds and are
    /// merged into `resolve`/`targets` results. `action` is one of
    /// "set" | "get" | "rm" | "list"; `set` requires `note`,
    /// `get`/`rm` require `file` + `target`, `list` takes an optional
    /// `file` filter.
    Note {
        action: String,
        #[serde(default)]
        file: Option<String>,
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        note: Option<String>,
    },
    /// P2·2: structural repo map — every indexed file with its symbols.
    /// `markdown` emits the exportable document form (per-file headings +
    /// symbol bullets); otherwise a compact per-file outline.
    Map {
        #[serde(default)]
        markdown: bool,
    },
    /// Deterministic todo list: graph and git-history findings for either an
    /// explicit `query` (dead-interactive | dead-code | hotspots |
    /// recent-changes | by-concept) or the queries `prompt` classifies to.
    /// Served by the daemon so the graph it reads is the daemon's, kept
    /// fresh incrementally and never rebuilt under a concurrent reader.
    Plan {
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default)]
        query: Option<String>,
        #[serde(default)]
        tag: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// Bounded predicate evaluation with a witness: does a path exist from
    /// `from` to `to` along the selected call relation? Answers
    /// `established` with the edges that prove it, `absent_in_snapshot`
    /// after an exhaustive traversal, or `unknown` with a typed reason —
    /// never a guess. The daemon owns it because the answer is attributed
    /// to a graph generation whose signature it checks against the working
    /// tree before and after the traversal.
    Evaluate {
        /// `--from`: a uid (`path#qualified#kind`) or a name.
        from: String,
        /// `--to`: a uid or a name.
        to: String,
        /// `callees` (outgoing edges) or `callers` (incoming).
        #[serde(default)]
        traversal: Option<String>,
        /// `exact` or `exact,probable`; selects the relation, not a
        /// confidence threshold.
        #[serde(default)]
        tiers: Option<String>,
        #[serde(default)]
        max_depth: Option<u32>,
        #[serde(default)]
        time_budget_ms: Option<u64>,
        /// Restrict name resolution to paths under this prefix.
        #[serde(default)]
        scope: Option<String>,
        /// Answer about the stored snapshot: skip the after-check.
        #[serde(default)]
        at_snapshot: bool,
    },
    /// Bounded recursion-cycle enumeration with witnesses and explicit
    /// coverage: finds strongly connected components in the call graph
    /// (potential recursion cycles), bounded by node/edge/time/component
    /// budgets, each reported cycle carrying a concrete closed-path witness
    /// re-read from the store.
    Cycles {
        /// `exact` or `exact,probable`; selects the relation, not a
        /// confidence threshold.
        #[serde(default)]
        tiers: Option<String>,
        #[serde(default)]
        max_nodes: Option<u32>,
        #[serde(default)]
        max_edges: Option<u32>,
        #[serde(default)]
        time_budget_ms: Option<u64>,
        #[serde(default)]
        max_components: Option<u32>,
    },
    /// IDE-style symbol rename: the graph drives the edit set (definition,
    /// resolved call/reference sites, import bindings) and a tree-sitter
    /// re-parse verifies every rewritten identifier is the symbol — sites
    /// the graph could not resolve are reported, never guessed. `file` or
    /// `uid` disambiguate a shared `name`; `dry_run` returns the edit plan
    /// without writing.
    Rename {
        name: String,
        new_name: String,
        #[serde(default)]
        file: Option<String>,
        #[serde(default)]
        uid: Option<String>,
        #[serde(default)]
        dry_run: bool,
    },
    Shutdown,
}

impl Op {
    /// The wire tag name for this variant — the value serde emits under the
    /// `"op"` field (`"ping"`, `"search-content"`, `"scope-task"`, …). Used to populate
    /// the response envelope's `op` field so every response self-describes
    /// which op it answers.
    pub fn op_name(&self) -> &'static str {
        match self {
            Op::Ping => "ping",
            Op::Recall { .. } => "recall",
            Op::Search { .. } => "search",
            Op::Targets { .. } => "targets",
            Op::TargetsFacts { .. } => "targets_facts",
            Op::Meaning { .. } => "meaning",
            Op::Symbol { .. } => "symbol",
            Op::Skeleton { .. } => "skeleton",
            Op::Context { .. } => "context",
            Op::Impact { .. } => "impact",
            Op::Uses { .. } => "uses",
            Op::Trace { .. } => "trace",
            Op::Processes { .. } => "processes",
            Op::Clusters { .. } => "clusters",
            Op::Changes { .. } => "changes",
            Op::ReviewGate { .. } => "review_gate",
            Op::Graph { .. } => "graph",
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
            Op::Note { .. } => "note",
            Op::Map { .. } => "map",
            Op::Plan { .. } => "plan",
            Op::Evaluate { .. } => "evaluate",
            Op::Cycles { .. } => "cycles",
            Op::Rename { .. } => "rename",
            Op::Shutdown => "shutdown",
            Op::Reindex { .. } => "reindex",
        }
    }
}

/// The daemon ops a client may send — every real variant's [`Op::op_name`]
/// except `shutdown` (an internal admin op). These are wire tags, not CLI
/// commands: the SessionStart hook advertises the parser's subcommands
/// instead, because several tags name another command or none (`update` is
/// `fast-forward`, `sync` is `fetch`, `history_op` is `commit-history`). Kept in this file, beside the enum, so adding a
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
    "targets_facts",
    "meaning",
    "symbol",
    "skeleton",
    "context",
    "impact",
    "uses",
    "trace",
    "processes",
    "clusters",
    "changes",
    "review_gate",
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
    "note",
    "map",
    "plan",
    "evaluate",
    "cycles",
    "rename",
    "flow",
];

/// The one-paragraph usage doctrine the SessionStart hook injects into every
/// agent session. Lives beside `Op`/[`SESSION_CAPABILITIES`] so the doctrine
/// string and the op registry travel together and `pixel doctor`'s
/// scenario-consistency check can compare the installed rule text against
/// exactly what the binary injects.
///
/// Describes what Pixel offers without directing the agent: native tools
/// stay the default, and Pixel is one option among them.
pub const SESSION_USAGE: &str = "pixel offers deterministic repository retrieval and git commands: `pixel search-content`, `pixel find-code`, `pixel impact <symbol>` for callers and blast radius, `pixel dig-history` for past code, and `pixel what-changed`. Use one when it answers a question faster than your native tools; native search and reads remain available. `pixel --help` lists every command.";

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
            globs: Vec::new(),
            types: Vec::new(),
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
        let op: Op =
            serde_json::from_value(json!({"op": "targets", "task": "fix the bug"})).unwrap();
        assert_eq!(
            op,
            Op::Targets {
                task: "fix the bug".into(),
                limit: None,
                max_tier: None,
                precision: false,
                regions: false,
            }
        );
    }

    #[test]
    fn targets_facts_round_trips_with_defaulted_limit() {
        let op = Op::TargetsFacts {
            task: "fix login flow".into(),
            limit: Some(12),
        };
        let value = serde_json::to_value(&op).unwrap();
        assert_eq!(
            value,
            json!({"op": "targets_facts", "task": "fix login flow", "limit": 12})
        );
        assert_eq!(serde_json::from_value::<Op>(value).unwrap(), op);

        let defaulted: Op = serde_json::from_value(json!({
            "op": "targets_facts",
            "task": "fix login flow"
        }))
        .unwrap();
        assert_eq!(
            defaulted,
            Op::TargetsFacts {
                task: "fix login flow".into(),
                limit: None,
            }
        );
    }

    /// `limit` is optional on the wire; the daemon applies its default.
    #[test]
    fn meaning_round_trips_with_defaulted_limit() {
        let op = Op::Meaning {
            query: "how is the index refreshed".into(),
            limit: Some(5),
        };
        let value = serde_json::to_value(&op).unwrap();
        assert_eq!(
            value,
            json!({"op": "meaning", "query": "how is the index refreshed", "limit": 5})
        );
        assert_eq!(serde_json::from_value::<Op>(value).unwrap(), op);

        let defaulted: Op =
            serde_json::from_value(json!({"op": "meaning", "query": "refresh"})).unwrap();
        assert_eq!(
            defaulted,
            Op::Meaning {
                query: "refresh".into(),
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

    /// `if_stale` is opt-in on the wire: an older CLI's bare `graph` keeps
    /// asking for a rebuild, and only a caller that sets it sends the field.
    #[test]
    fn graph_if_stale_defaults_off_and_is_sent_only_when_set() {
        let bare: Op = serde_json::from_value(json!({"op": "graph"})).unwrap();
        assert_eq!(bare, Op::Graph { if_stale: false });
        let reuse = Op::Graph { if_stale: true };
        assert_eq!(
            serde_json::to_value(&reuse).unwrap(),
            json!({"op": "graph", "if_stale": true})
        );
    }

    /// `read_only` is opt-in on the wire: the history ops deserialize it as
    /// false when absent and emit it only when set, so a flag-aware client
    /// and a pre-flag client serialize identically when it is off.
    #[test]
    fn history_ops_read_only_defaults_off_and_is_sent_only_when_set() {
        let bare: Op = serde_json::from_value(json!({"op": "history", "query": "login"})).unwrap();
        assert_eq!(
            bare,
            Op::History {
                query: "login".into(),
                facet: None,
                limit: None,
                read_only: false,
            }
        );
        assert_eq!(
            serde_json::to_value(&Op::History {
                query: "login".into(),
                facet: None,
                limit: None,
                read_only: true,
            })
            .unwrap()["read_only"],
            json!(true)
        );
        assert_eq!(
            serde_json::to_value(&bare).unwrap().get("read_only"),
            None,
            "read_only: false must not reach the wire"
        );

        for (value, expected) in [
            (
                json!({"op": "lifecycle", "path": "a.rs"}),
                Op::Lifecycle {
                    path: Some("a.rs".into()),
                    token: None,
                    read_only: false,
                },
            ),
            (
                json!({"op": "excavate", "phrase": "login"}),
                Op::Excavate {
                    phrase: Some("login".into()),
                    path: None,
                    from: None,
                    to: None,
                    limit: None,
                    read_only: false,
                },
            ),
        ] {
            assert_eq!(serde_json::from_value::<Op>(value).unwrap(), expected);
        }
        for op in [
            Op::Lifecycle {
                path: None,
                token: Some("login".into()),
                read_only: true,
            },
            Op::Excavate {
                phrase: None,
                path: None,
                from: None,
                to: None,
                limit: None,
                read_only: true,
            },
        ] {
            assert_eq!(serde_json::to_value(&op).unwrap()["read_only"], json!(true));
        }
    }

    #[test]
    fn graph_and_status_serialize_as_empty_object_variants() {
        assert_eq!(
            serde_json::to_value(Op::Graph { if_stale: false }).unwrap(),
            json!({"op": "graph"})
        );
        assert_eq!(
            serde_json::to_value(Op::Status {}).unwrap(),
            json!({"op": "status"})
        );
    }

    #[test]
    fn shutdown_serializes_as_bare_tag() {
        let value = serde_json::to_value(Op::Shutdown).unwrap();
        assert_eq!(value, json!({"op": "shutdown"}));
    }

    #[test]
    fn resolve_round_trips() {
        let op = Op::Resolve {
            phrase: "the form".into(),
            limit: Some(5),
        };
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
        assert_eq!(
            op,
            Op::Reconcile {
                strategy: None,
                push: None,
                into: None,
                request_id: None
            }
        );
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
            (
                Op::Recall {
                    action: "x".into(),
                    params: json!(null),
                },
                "recall",
            ),
            (
                Op::Search {
                    pattern: "".into(),
                    json: false,
                    limit: None,
                    offset: None,
                    paths: None,
                    scope: None,
                    globs: Vec::new(),
                    types: Vec::new(),
                },
                "search",
            ),
            (
                Op::Targets {
                    task: "".into(),
                    limit: None,
                    max_tier: None,
                    precision: false,
                    regions: false,
                },
                "targets",
            ),
            (
                Op::TargetsFacts {
                    task: "".into(),
                    limit: None,
                },
                "targets_facts",
            ),
            (
                Op::Meaning {
                    query: "".into(),
                    limit: None,
                },
                "meaning",
            ),
            (Op::Symbol { name: "".into() }, "symbol"),
            (Op::Skeleton { file: "".into() }, "skeleton"),
            (
                Op::Context {
                    uid: "".into(),
                    budget_tokens: None,
                },
                "context",
            ),
            (
                Op::Impact {
                    uid_or_name: "".into(),
                    direction: "".into(),
                    depth: None,
                },
                "impact",
            ),
            (
                Op::Uses {
                    uid_or_name: "".into(),
                    role: "".into(),
                    offset: None,
                },
                "uses",
            ),
            (
                Op::Trace {
                    from: "".into(),
                    to: "".into(),
                },
                "trace",
            ),
            (Op::Processes { offset: None }, "processes"),
            (Op::Clusters { offset: None }, "clusters"),
            (
                Op::Changes {
                    base: None,
                    offset: None,
                    include_tests: false,
                },
                "changes",
            ),
            (Op::ReviewGate { base: None }, "review_gate"),
            (Op::Graph { if_stale: false }, "graph"),
            (Op::Status {}, "status"),
            (
                Op::Resolve {
                    phrase: "".into(),
                    limit: None,
                },
                "resolve",
            ),
            (
                Op::History {
                    query: "".into(),
                    facet: None,
                    limit: None,
                    read_only: false,
                },
                "history",
            ),
            (
                Op::Lifecycle {
                    path: None,
                    token: None,
                    read_only: false,
                },
                "lifecycle",
            ),
            (
                Op::Excavate {
                    phrase: None,
                    path: None,
                    from: None,
                    to: None,
                    limit: None,
                    read_only: false,
                },
                "excavate",
            ),
            (
                Op::Reconcile {
                    strategy: None,
                    push: None,
                    into: None,
                    request_id: None,
                },
                "reconcile",
            ),
            (
                Op::Journal {
                    kind: "".into(),
                    path: None,
                    detail: None,
                },
                "journal",
            ),
            (Op::Inspect { files: None }, "inspect"),
            (
                Op::Review {
                    cursor: None,
                    byte_cap: None,
                },
                "review",
            ),
            (
                Op::Diff {
                    from: "".into(),
                    to: None,
                    paths: None,
                    byte_cap: None,
                },
                "diff",
            ),
            (
                Op::HistoryOp {
                    ref_name: None,
                    limit: None,
                    detail: None,
                    cursor: None,
                    byte_cap: None,
                },
                "history_op",
            ),
            (
                Op::Publish {
                    message: "".into(),
                    files: vec![],
                    expected_head: None,
                    push: None,
                    amend: None,
                    request_id: "".into(),
                },
                "publish",
            ),
            (
                Op::Push {
                    remote: "".into(),
                    refspec: "".into(),
                    force_with_lease: None,
                    request_id: "".into(),
                },
                "push",
            ),
            (
                Op::Ship {
                    message: "".into(),
                    files: vec![],
                    remote: "".into(),
                    refspec: "".into(),
                    request_id: "".into(),
                },
                "ship",
            ),
            (
                Op::BranchOp {
                    name: "".into(),
                    from: None,
                    request_id: "".into(),
                },
                "branch_op",
            ),
            (
                Op::Update {
                    expected_head: "".into(),
                    target_oid: "".into(),
                    request_id: "".into(),
                },
                "update",
            ),
            (
                Op::Sync {
                    remote: "".into(),
                    refspec: None,
                },
                "sync",
            ),
            (
                Op::Plan {
                    prompt: None,
                    query: None,
                    tag: None,
                    limit: None,
                },
                "plan",
            ),
            (
                Op::Evaluate {
                    from: String::new(),
                    to: String::new(),
                    traversal: None,
                    tiers: None,
                    max_depth: None,
                    time_budget_ms: None,
                    scope: None,
                    at_snapshot: false,
                },
                "evaluate",
            ),
            (
                Op::Cycles {
                    tiers: None,
                    max_nodes: None,
                    max_edges: None,
                    time_budget_ms: None,
                    max_components: None,
                },
                "cycles",
            ),
            (
                Op::Rename {
                    name: "".into(),
                    new_name: "".into(),
                    file: None,
                    uid: None,
                    dry_run: false,
                },
                "rename",
            ),
            (Op::Shutdown, "shutdown"),
        ];
        for (op, expected) in cases {
            assert_eq!(op.op_name(), *expected);
            let serialized = serde_json::to_value(op).unwrap();
            assert_eq!(serialized["op"].as_str(), Some(*expected));
        }
    }

    #[test]
    fn session_usage_describes_pixel_without_mandating_it() {
        for command in [
            "search-content",
            "find-code",
            "impact",
            "dig-history",
            "what-changed",
        ] {
            assert!(SESSION_USAGE.contains(command), "{command}");
        }
        for directive in ["mandatory", "must", "before any", "before editing"] {
            assert!(
                !SESSION_USAGE.contains(directive),
                "SESSION_USAGE must not direct the agent: {directive}"
            );
        }
        assert!(SESSION_USAGE.contains("native search and reads remain available"));
    }

    #[test]
    fn session_capabilities_track_every_real_op() {
        // The exhaustive real variant set, independent of SESSION_CAPABILITIES
        // itself — this must be updated by hand whenever a variant is added,
        // same as op_name_matches_serde_tag's `cases` above, so the two lists
        // can't silently drift in the same direction and still agree.
        let all_real_ops: &[&str] = &[
            "ping",
            "recall",
            "search",
            "targets",
            "targets_facts",
            "meaning",
            "symbol",
            "skeleton",
            "context",
            "impact",
            "uses",
            "trace",
            "processes",
            "clusters",
            "changes",
            "review_gate",
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
            "note",
            "map",
            "plan",
            "evaluate",
            "cycles",
            "flow",
            "rename",
            "shutdown",
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
