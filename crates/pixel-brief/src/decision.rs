// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! What the brief decided about one prompt: the `brief` line of
//! `.pixel/journal.jsonl` (`specs/metrics.md`). The caller that delivers the
//! brief appends it, with its own provenance; nothing here touches a file.

use pixel_metrics::journal::logged_typed;
use serde_json::{Value, json};

/// What the brief decided about one prompt, as the journal and `pixel brief
/// --json` show it.
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    /// Milliseconds since the Unix epoch when the decision was recorded.
    pub ts_ms: u64,
    /// `strong`, `weak`, `prose`, or `None` when the prompt asked nothing
    /// about the repository.
    pub signal: Option<&'static str>,
    /// `open`, `closed`, `unjudged` or `declined`.
    pub gate: &'static str,
    /// The gate decision silenced the brief (it is always `true` for prose;
    /// a weak prompt records the verdict it would have been refused by).
    pub enforced: bool,
    /// Why the gate decided as it did.
    pub reason: Option<String>,
    /// The best file's share of the prompt's keyword weight.
    pub score: Option<f64>,
    /// The tier the score fell in (`high`, `low`, `off`), when the relevance
    /// decision was made.
    pub tier: Option<&'static str>,
    /// The gate model's source (the json its constants were copied from).
    pub model: &'static str,
    pub best_file: Option<String>,
    /// What the relevance decision weighed, when the probe answered.
    pub features: Option<crate::relevance::Features>,
    /// The question kind the plan routed to.
    pub kind: Option<&'static str>,
    pub ops: usize,
    pub answered: usize,
    /// Bytes of the brief that was rendered; `0` when none was.
    pub bytes: usize,
    /// The block carried the search receipt.
    pub receipt: bool,
    /// The block carried answer excerpts.
    pub answer: bool,
    /// Bytes of those excerpts.
    pub excerpt_bytes: usize,
    /// Bytes of the region pack when the block is one; `0` otherwise.
    pub pack_bytes: usize,
    /// Hook parts the region pack's text fills.
    pub pack_parts: usize,
    /// Symbols the region pack is about.
    pub pack_targets: usize,
    /// Blocks a locator (setting or test) contributed to the region pack.
    pub pack_located: usize,
    /// The text search stood in for a graph that could not answer.
    pub fallback: bool,
    /// From the prompt to the brief's text.
    pub elapsed_ms: u64,
    /// The packet was cut short (its window ended before the chain did, or
    /// the op cap refused an operation): what was rendered is hedged as
    /// partial.
    pub cut: bool,
    /// The window the brief was given, for the chain; `None` for a prompt
    /// that got none.
    pub deadline_ms: Option<u64>,
    /// Where `elapsed_ms` went, stage by stage.
    pub stages: Stages,
    /// Where the evidence came from: `daemon` or `local`.
    pub route: Option<&'static str>,
    /// The typed text, masked and bounded.
    pub typed: String,
    /// SHA-256 of the whole typed text, lowercase hex.
    pub sha256: String,
    /// The brief probes that were due, with how each ended.
    pub probes: Vec<crate::probes::ProbeRun>,
    /// The recipe steps that ran, with their milliseconds, in the order they
    /// finished. A step whose lexical gate stayed closed is absent, not `0`.
    pub steps: Vec<(&'static str, u64)>,
}

/// How long each stage of a brief took, in milliseconds; `None` for a stage
/// that did not run, never `0`: a declined prompt runs none, a closed gate
/// never ranks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stages {
    /// Opening the evidence and the candidate searches: the relevance,
    /// meaning probes of a plain prompt, or the search, concept and
    /// symbol lookups of a code-shaped one.
    pub search_ms: Option<u64>,
    /// Folding the candidates, the kind route, the answer excerpts and the
    /// region pack, until the worker ended or the window cut it.
    pub rank_ms: Option<u64>,
    /// Rendering the block from what had arrived.
    pub render_ms: Option<u64>,
    /// The brief probes, from the first start to the merge; `None` when no
    /// probe was due.
    pub probe_ms: Option<u64>,
}

impl Stages {
    fn to_json(self) -> Value {
        json!({
            "search_ms": self.search_ms,
            "rank_ms": self.rank_ms,
            "render_ms": self.render_ms,
            "probe_ms": self.probe_ms,
        })
    }
}

impl Record {
    /// The record as the journal line it is: `t` names it `brief`.
    pub fn to_json(&self) -> Value {
        json!({
            "t": "brief",
            "ts": self.ts_ms,
            "signal": self.signal,
            "gate": self.gate,
            "enforced": self.enforced,
            "reason": self.reason,
            "score": self.score,
            "tier": self.tier,
            "model": self.model,
            "best_file": self.best_file,
            "features": self.features.as_ref().map(|features| json!({
                "struct_per_mille": features.struct_per_mille,
                "question": features.question,
                "ops_share": features.ops_share,
                "struct_ratio": features.struct_ratio,
                "total_weight": features.total_weight,
                "best_structural_weight": features.best_structural_weight,
                "files_considered": features.files_considered,
                "structural_files": features.structural_files,
                "keywords": features.keywords,
                "informative": features.informative,
                "shared": features.shared,
            })),
            "kind": self.kind,
            "ops": self.ops,
            "answered": self.answered,
            "bytes": self.bytes,
            "receipt": self.receipt,
            "answer": self.answer,
            "excerpt_bytes": self.excerpt_bytes,
            "pack_bytes": self.pack_bytes,
            "pack_parts": self.pack_parts,
            "pack_targets": self.pack_targets,
            "pack_located": self.pack_located,
            "fallback": self.fallback,
            "elapsed_ms": self.elapsed_ms,
            "cut": self.cut,
            "deadline_ms": self.deadline_ms,
            "stages": self.stages.to_json(),
            "route": self.route,
            "typed": self.typed,
            "sha256": self.sha256,
            "steps": self.steps.iter().map(|(name, ms)| json!({
                "name": name,
                "ms": ms,
            })).collect::<Vec<_>>(),
            "probes": self.probes.iter().map(|run| json!({
                "name": run.name,
                "ms": run.ms,
                "bytes": run.bytes,
                "outcome": run.outcome,
            })).collect::<Vec<_>>(),
        })
    }
}

/// The typed text as a record carries it: the task of a long paste, masked
/// and bounded (`pixel_metrics::journal::logged_typed`).
pub fn logged(typed: &str) -> String {
    logged_typed(crate::prompt::brief_task(typed))
}

#[cfg(test)]
mod tests {
    use pixel_metrics::journal::sha256_hex;

    use super::*;

    fn record() -> Record {
        Record {
            ts_ms: 1_700_000_000_000,
            signal: Some("prose"),
            gate: "open",
            enforced: true,
            reason: Some("2 of 3 key terms".into()),
            score: Some(0.75),
            tier: Some("high"),
            model: "test model",
            best_file: Some("src/a.rs".into()),
            features: Some(crate::relevance::Features {
                struct_per_mille: 3.5,
                question: true,
                ops_share: 0.25,
                struct_ratio: 0.75,
                total_weight: 8.0,
                best_structural_weight: 6.0,
                files_considered: 1161,
                structural_files: 30,
                keywords: 4,
                informative: 4,
                shared: 3,
            }),
            kind: Some("lookup"),
            ops: 3,
            answered: 2,
            bytes: 412,
            receipt: true,
            answer: true,
            excerpt_bytes: 1300,
            pack_bytes: 5200,
            pack_parts: 1,
            pack_targets: 2,
            pack_located: 1,
            fallback: true,
            elapsed_ms: 188,
            cut: true,
            deadline_ms: Some(3000),
            stages: Stages {
                search_ms: Some(90),
                rank_ms: Some(70),
                render_ms: Some(3),
                probe_ms: Some(40),
            },
            route: Some("daemon"),
            typed: "how does the daemon start".into(),
            sha256: sha256_hex("how does the daemon start"),
            probes: vec![crate::probes::ProbeRun {
                name: "recovery".into(),
                ms: 38,
                bytes: 120,
                outcome: "ok",
            }],
            steps: vec![("name-files", 12), ("test-bodies", 4)],
        }
    }

    #[test]
    fn to_json_should_carry_every_field_under_its_name() {
        let value = record().to_json();
        assert_eq!(
            value,
            json!({
                "t": "brief",
                "ts": 1_700_000_000_000_u64,
                "signal": "prose",
                "gate": "open",
                "enforced": true,
                "reason": "2 of 3 key terms",
                "score": 0.75,
                "tier": "high",
                "model": "test model",
                "best_file": "src/a.rs",
                "features": {
                    "struct_per_mille": 3.5,
                    "question": true,
                    "ops_share": 0.25,
                    "struct_ratio": 0.75,
                    "total_weight": 8.0,
                    "best_structural_weight": 6.0,
                    "files_considered": 1161,
                    "structural_files": 30,
                    "keywords": 4,
                    "informative": 4,
                    "shared": 3,
                },
                "kind": "lookup",
                "ops": 3,
                "answered": 2,
                "bytes": 412,
                "receipt": true,
                "answer": true,
                "excerpt_bytes": 1300,
                "pack_bytes": 5200,
                "pack_parts": 1,
                "pack_targets": 2,
                "pack_located": 1,
                "fallback": true,
                "elapsed_ms": 188,
                "cut": true,
                "deadline_ms": 3000,
                "stages": {"search_ms": 90, "rank_ms": 70, "render_ms": 3, "probe_ms": 40},
                "route": "daemon",
                "typed": "how does the daemon start",
                "sha256": sha256_hex("how does the daemon start"),
                "steps": [
                    {"name": "name-files", "ms": 12},
                    {"name": "test-bodies", "ms": 4},
                ],
                "probes": [{"name": "recovery", "ms": 38, "bytes": 120, "outcome": "ok"}],
            })
        );
        let bare = Record {
            signal: None,
            reason: None,
            score: None,
            best_file: None,
            features: None,
            tier: None,
            kind: None,
            ..record()
        };
        let value = bare.to_json();
        for key in [
            "signal",
            "reason",
            "score",
            "tier",
            "best_file",
            "features",
            "kind",
        ] {
            assert_eq!(value[key], Value::Null, "{key}");
        }
    }

    #[test]
    fn logged_should_keep_only_the_last_paragraph_of_a_pasted_log() {
        let pasted = format!("{}\n\nwhy does it fail", "error line\n".repeat(60));
        assert_eq!(logged(&pasted), "why does it fail");
    }
}
