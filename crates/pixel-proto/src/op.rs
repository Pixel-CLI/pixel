// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `Op`: the daemon request type (`specs/ops.md`), defined in the contract
//! crate so the daemon and the CLI share one enum. Fifteen variants: four
//! admin, three hooks, eight queries. Everything else (context packing,
//! trace, rename, concept resolution) is an in-process `Engine` call, never
//! a wire op.

use serde::{Deserialize, Serialize};

/// When an [`Op::EditCheck`] runs relative to the host's edit tool.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditPhase {
    /// Before the tool: the card, and the file's baseline for the session.
    #[default]
    Before,
    /// After the tool: the callers the edit left on an old shape.
    After,
}

impl EditPhase {
    fn is_before(&self) -> bool {
        *self == Self::Before
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    // --- admin ---
    Ping,
    /// Index, graph and history freshness and the daemon's generation.
    Status {},
    /// Force a rebuild of the text index and the graph. When sent to the
    /// daemon its already-open service does the work (singleton: no
    /// concurrent build races).
    Reindex {},
    Shutdown,

    // --- hooks ---
    /// The prompt-start brief, computed inside the daemon over one evidence
    /// implementation (`specs/brief.md`).
    Brief {
        prompt: String,
        /// The host's session id, for the dedupe (the stores that key by it hash it).
        session: String,
        /// The host the brief is delivered to; it sets the size limits.
        host: String,
        /// How many hook parts the host delivers it over (1 to 8).
        #[serde(default = "one")]
        parts: u8,
        /// Recipe steps switched off for this brief (`pixel brief --without`).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        without: Vec<String>,
    },
    /// An edit to `path`, before or after the tool ran (`specs/hooks.md`).
    /// `proposed` is the new text the tool would write, when the host's
    /// payload carries it.
    EditCheck {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proposed: Option<String>,
        #[serde(default, skip_serializing_if = "EditPhase::is_before")]
        phase: EditPhase,
        session: String,
    },
    /// At Stop: whether stale callers are still pending for this session.
    StopCheck {
        session: String,
    },

    // --- queries ---
    Search {
        /// See [`Op::stale_ok`].
        #[serde(default, skip_serializing_if = "is_false")]
        stale_ok: bool,
        pattern: String,
        #[serde(default)]
        json: bool,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        offset: Option<usize>,
        /// Repo-relative path prefixes to restrict the search to (rg-style
        /// multi-path invocations). None or empty means the whole repo.
        #[serde(default)]
        paths: Option<Vec<String>>,
        /// `"code"` enables ranked output: matches are reranked by file-level
        /// signals without changing the hit set. Any other value preserves
        /// the path/line order.
        #[serde(default)]
        scope: Option<String>,
        /// `-g/--glob` rules with ripgrep's semantics: a leading `!`
        /// excludes, the last matching rule decides, and with any include
        /// rule a path no rule matches is left out. Empty means no filter.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        globs: Vec<String>,
        /// `-t/--type` names (`rust`, `ts`, `md`, ...). Empty means every type.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        types: Vec<String>,
    },
    Symbol {
        name: String,
        /// See [`Op::stale_ok`].
        #[serde(default, skip_serializing_if = "is_false")]
        stale_ok: bool,
    },
    /// Callers or callees of a symbol, by uid or name.
    Callers {
        /// See [`Op::stale_ok`].
        #[serde(default, skip_serializing_if = "is_false")]
        stale_ok: bool,
        uid_or_name: String,
        /// `"callers"` or `"callees"`.
        role: String,
        #[serde(default)]
        offset: Option<usize>,
    },
    /// Blast radius of a symbol, with the tests that cover it.
    Impact {
        /// See [`Op::stale_ok`].
        #[serde(default, skip_serializing_if = "is_false")]
        stale_ok: bool,
        uid_or_name: String,
        direction: String,
        #[serde(default)]
        depth: Option<u32>,
    },
    /// Working-tree (or `base`) changes mapped onto symbols.
    Changes {
        #[serde(default)]
        base: Option<String>,
        #[serde(default)]
        offset: Option<usize>,
        /// Map affected symbols to the test files that exercise them
        /// (upstream caller walk).
        #[serde(default)]
        include_tests: bool,
    },
    /// History-wide fact and diff search.
    History {
        query: String,
        #[serde(default)]
        facet: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
        /// Answer from the history db exactly as it stands: never ingest,
        /// never create it, never spawn the warmer. An absent db answers
        /// `unavailable`.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        read_only: bool,
    },
    /// Natural-language retrieval over the resident code-chunk embeddings
    /// (feature `semantic`): ranked leads, never a verdict on relevance.
    /// Answers `unavailable` with a reason instead of waiting while the
    /// vectors are cold, stale or rebuilding, so a deadline-bounded caller
    /// falls back. Never downloads the model.
    Meaning {
        /// See [`Op::stale_ok`].
        #[serde(default, skip_serializing_if = "is_false")]
        stale_ok: bool,
        query: String,
        /// Leads returned; the daemon's default when absent, capped.
        #[serde(default)]
        limit: Option<usize>,
    },
    /// Structural repo map: every indexed file with its symbols. `markdown`
    /// emits the exportable document form; otherwise a compact outline.
    Map {
        #[serde(default)]
        markdown: bool,
    },
}

impl Op {
    /// The wire tags of every variant, in declaration order. The capability
    /// test compares this list with the tags serde's derived deserializer
    /// accepts, so a variant cannot be added without being listed.
    pub const NAMES: [&'static str; 15] = [
        "ping",
        "status",
        "reindex",
        "shutdown",
        "brief",
        "edit_check",
        "stop_check",
        "search",
        "symbol",
        "callers",
        "impact",
        "changes",
        "history",
        "meaning",
        "map",
    ];

    /// The caller accepts an answer from the generation published before
    /// the watcher batches still queued: a read replica may serve it during
    /// an update instead of the writer after it, and the envelope then says
    /// so (`epistemics.staleness_ms` and a `STALE_GENERATION` warning).
    pub fn stale_ok(&self) -> bool {
        match self {
            Op::Search { stale_ok, .. }
            | Op::Symbol { stale_ok, .. }
            | Op::Callers { stale_ok, .. }
            | Op::Impact { stale_ok, .. }
            | Op::Meaning { stale_ok, .. } => *stale_ok,
            _ => false,
        }
    }

    /// The wire tag of this variant: the value serde emits under `"op"`.
    /// The response envelope's `op` field carries it, so every response
    /// names the op it answers.
    pub fn op_name(&self) -> &'static str {
        match self {
            Op::Ping => "ping",
            Op::Status {} => "status",
            Op::Reindex {} => "reindex",
            Op::Shutdown => "shutdown",
            Op::Brief { .. } => "brief",
            Op::EditCheck { .. } => "edit_check",
            Op::StopCheck { .. } => "stop_check",
            Op::Search { .. } => "search",
            Op::Symbol { .. } => "symbol",
            Op::Callers { .. } => "callers",
            Op::Impact { .. } => "impact",
            Op::Changes { .. } => "changes",
            Op::History { .. } => "history",
            Op::Meaning { .. } => "meaning",
            Op::Map { .. } => "map",
        }
    }
}

/// The serde default of `Op::Brief::parts`: a single hook.
fn one() -> u8 {
    1
}

/// The `skip_serializing_if` predicate for a flag whose absence means false;
/// serde hands the field by reference.
fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn sample(name: &str) -> Value {
        match name {
            "ping" | "shutdown" => json!({"op": name}),
            "status" | "reindex" => json!({"op": name}),
            "brief" => json!({"op": "brief", "prompt": "p", "session": "s", "host": "claude"}),
            "edit_check" => json!({"op": "edit_check", "path": "a.rs", "session": "s"}),
            "stop_check" => json!({"op": "stop_check", "session": "s"}),
            "search" => json!({"op": "search", "pattern": "x"}),
            "symbol" => json!({"op": "symbol", "name": "n"}),
            "callers" => json!({"op": "callers", "uid_or_name": "n", "role": "callers"}),
            "impact" => json!({"op": "impact", "uid_or_name": "n", "direction": "upstream"}),
            "changes" => json!({"op": "changes"}),
            "history" => json!({"op": "history", "query": "q"}),
            "meaning" => json!({"op": "meaning", "query": "q"}),
            "map" => json!({"op": "map"}),
            other => panic!("no sample for {other}"),
        }
    }

    #[test]
    fn ping_serializes_as_bare_tag() {
        assert_eq!(
            serde_json::to_value(Op::Ping).unwrap(),
            json!({"op": "ping"})
        );
    }

    #[test]
    fn shutdown_serializes_as_bare_tag() {
        assert_eq!(
            serde_json::to_value(Op::Shutdown).unwrap(),
            json!({"op": "shutdown"})
        );
    }

    #[test]
    fn status_and_reindex_serialize_as_empty_object_variants() {
        assert_eq!(
            serde_json::to_value(Op::Status {}).unwrap(),
            json!({"op": "status"})
        );
        assert_eq!(
            serde_json::to_value(Op::Reindex {}).unwrap(),
            json!({"op": "reindex"})
        );
    }

    #[test]
    fn every_listed_name_deserializes_and_names_its_own_variant() {
        for name in Op::NAMES {
            let op: Op = serde_json::from_value(sample(name))
                .unwrap_or_else(|e| panic!("{name} does not deserialize: {e}"));
            assert_eq!(op.op_name(), name);
            assert_eq!(
                serde_json::to_value(&op).unwrap()["op"],
                name,
                "{name} serializes under another tag"
            );
        }
    }

    #[test]
    fn an_unlisted_tag_is_refused() {
        for tag in ["recall", "publish", "targets", "scope_task", "uses"] {
            assert!(
                serde_json::from_value::<Op>(json!({"op": tag})).is_err(),
                "{tag} must not be a wire op"
            );
        }
    }

    #[test]
    fn brief_parts_default_to_one_hook() {
        let op: Op = serde_json::from_value(sample("brief")).unwrap();
        assert_eq!(
            op,
            Op::Brief {
                prompt: "p".into(),
                session: "s".into(),
                host: "claude".into(),
                parts: 1,
                without: Vec::new(),
            }
        );
    }

    #[test]
    fn brief_sends_the_switched_off_steps_only_when_there_are_some() {
        let mut op = Op::Brief {
            prompt: "p".into(),
            session: "s".into(),
            host: "cli".into(),
            parts: 1,
            without: Vec::new(),
        };
        assert!(serde_json::to_value(&op).unwrap().get("without").is_none());
        if let Op::Brief { without, .. } = &mut op {
            without.push("workspace-overview".into());
        }
        assert_eq!(
            serde_json::to_value(&op).unwrap()["without"],
            serde_json::json!(["workspace-overview"])
        );
    }

    #[test]
    fn edit_check_sends_proposed_text_only_when_present() {
        let without = Op::EditCheck {
            path: "a.rs".into(),
            proposed: None,
            phase: EditPhase::Before,
            session: "s".into(),
        };
        assert_eq!(
            serde_json::to_value(&without).unwrap(),
            json!({"op": "edit_check", "path": "a.rs", "session": "s"})
        );
        let with = Op::EditCheck {
            path: "a.rs".into(),
            proposed: Some("fn f() {}".into()),
            phase: EditPhase::Before,
            session: "s".into(),
        };
        assert_eq!(
            serde_json::to_value(&with).unwrap()["proposed"],
            "fn f() {}"
        );
    }

    #[test]
    fn edit_check_phase_defaults_to_before_and_is_sent_only_when_after() {
        let before: Op =
            serde_json::from_value(json!({"op": "edit_check", "path": "a.rs", "session": "s"}))
                .unwrap();
        assert!(matches!(
            before,
            Op::EditCheck {
                phase: EditPhase::Before,
                ..
            }
        ));
        let after = Op::EditCheck {
            path: "a.rs".into(),
            proposed: None,
            phase: EditPhase::After,
            session: "s".into(),
        };
        assert_eq!(
            serde_json::to_value(&after).unwrap(),
            json!({"op": "edit_check", "path": "a.rs", "phase": "after", "session": "s"})
        );
    }

    #[test]
    fn search_serializes_with_snake_case_tag_and_fields() {
        let op = Op::Search {
            stale_ok: false,
            pattern: "fn main".into(),
            json: true,
            limit: Some(50),
            offset: None,
            paths: Some(vec!["src".into()]),
            scope: None,
            globs: vec![],
            types: vec![],
        };
        assert_eq!(
            serde_json::to_value(&op).unwrap(),
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
    fn stale_ok_reaches_the_wire_only_when_asked_and_only_on_reads_that_take_it() {
        let ask = |op: Op| serde_json::to_value(&op).unwrap();
        let symbol = Op::Symbol {
            name: "n".into(),
            stale_ok: false,
        };
        assert!(ask(symbol.clone()).get("stale_ok").is_none());
        assert!(!symbol.stale_ok());
        let stale = Op::Symbol {
            name: "n".into(),
            stale_ok: true,
        };
        assert_eq!(ask(stale.clone())["stale_ok"], true);
        assert!(stale.stale_ok());
        for op in [
            Op::Callers {
                stale_ok: true,
                uid_or_name: "n".into(),
                role: "callers".into(),
                offset: None,
            },
            Op::Impact {
                stale_ok: true,
                uid_or_name: "n".into(),
                direction: "upstream".into(),
                depth: None,
            },
            Op::Meaning {
                stale_ok: true,
                query: "q".into(),
                limit: None,
            },
            Op::Search {
                stale_ok: true,
                pattern: "x".into(),
                json: false,
                limit: None,
                offset: None,
                paths: None,
                scope: None,
                globs: vec![],
                types: vec![],
            },
        ] {
            assert!(op.stale_ok(), "{op:?}");
        }
        for op in [
            Op::Ping,
            Op::Status {},
            Op::Changes {
                base: None,
                offset: None,
                include_tests: false,
            },
        ] {
            assert!(!op.stale_ok(), "{op:?}");
        }
    }

    #[test]
    fn history_read_only_defaults_off_and_is_sent_only_when_set() {
        let default: Op = serde_json::from_value(sample("history")).unwrap();
        assert!(matches!(
            default,
            Op::History {
                read_only: false,
                ..
            }
        ));
        assert!(
            serde_json::to_value(&default)
                .unwrap()
                .get("read_only")
                .is_none()
        );
        let read_only: Op =
            serde_json::from_value(json!({"op": "history", "query": "q", "read_only": true}))
                .unwrap();
        assert_eq!(serde_json::to_value(&read_only).unwrap()["read_only"], true);
    }

    #[test]
    fn impact_round_trips() {
        let op = Op::Impact {
            stale_ok: false,
            uid_or_name: "a.rs#f#function".into(),
            direction: "upstream".into(),
            depth: Some(2),
        };
        let back: Op = serde_json::from_value(serde_json::to_value(&op).unwrap()).unwrap();
        assert_eq!(back, op);
    }
}
