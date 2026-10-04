// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Wire contract of `pixel evaluate`: bounded predicate evaluation with a witness.
//!
//! Every answer is one JSON object, a tagged union on `kind`:
//!
//! - `{"kind": "evaluation", …}` — the predicate was evaluated (exit code 0,
//!   whatever the status: **0 means evaluated, not true**);
//! - `{"kind": "error", "code": …, "message": …}` — a usage error (exit
//!   code 2) or a technical failure (exit code 3).
//!
//! An evaluation carries `status` + `answer` + `witness` + `reason` +
//! `next_actions` as flat fields, but the Rust side is one [`Outcome`] enum
//! so an illegal tuple (an `established` answer without a witness, an
//! `unknown` without a reason) cannot be built, and is refused on
//! deserialization. The wire shape, from `docs/design/evaluate.md`:
//!
//! ```json
//! {
//!   "kind": "evaluation", "schema_version": 1,
//!   "predicate": "path", "status": "established", "answer": true,
//!   "domain":   { "relation": "indexed_call_graph", "edge_kinds": ["calls", "has_method"],
//!                 "traversal": "callees", "tiers": ["exact"] },
//!   "snapshot": { "signature": "…", "extractor_version": "…", "generation_coherent": true,
//!                 "working_tree_check": "full_before_and_after", "working_tree_matches": true },
//!   "coverage": { "traversal_exhausted": true, "depth_cap": 8, "depth_cap_dropped_frontier": false,
//!                 "time_budget_ms": 250, "time_budget_hit": false, "graph_file_cap_hit": false,
//!                 "files_excluded_by_size": 0, "unresolved_same_name_sites": 0,
//!                 "extraction_limits": ["…"] },
//!   "witness":  { "kind": "path", "probable_edges": 0, "edges": [ { "from": {…}, "to": {…},
//!                 "edge": { "kind": "calls", "tier": "exact", "site": { "path": "…", "line": 51 },
//!                           "receiver": "manager" },
//!                 "call_direction": "from→to", "traversal_step": 1,
//!                 "premises": { "kind": "not_required" } } ] },
//!   "reason": null, "next_actions": [],
//!   "epistemics": { … },
//!   "summary": "Path found in the indexed call graph, snapshot 3f9c1a2b…, …"
//! }
//! ```
//!
//! This crate holds the contract only: turning a `pixel_graph` evaluation
//! into an [`EvaluationEnvelope`], checking the snapshot, resolving symbol
//! names and rendering the CLI all live in the daemon and the CLI crates.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::Epistemics;

/// The version of this wire shape; bumped on any field rename or removal.
pub const SCHEMA_VERSION: u32 = 1;

/// The one relation `pixel evaluate` walks today.
pub const RELATION_INDEXED_CALL_GRAPH: &str = "indexed_call_graph";

/// Exit code of a completed evaluation, whatever its status.
pub const EXIT_EVALUATED: i32 = 0;
/// Exit code of a usage or protocol error.
pub const EXIT_USAGE: i32 = 2;
/// Exit code of a technical failure (daemon, IO, database).
pub const EXIT_TECHNICAL: i32 = 3;

/// Length of the signature prefix a summary quotes before `…`.
const SIGNATURE_PREFIX_CHARS: usize = 8;

/// The fixed closing sentence of every `diff_reaches` summary.
pub const DIFF_REACHES_SCOPE_SENTENCE: &str = "This follows indexed call relations only; it does \
     not cover constants, types, schemas, imports, configuration or shared data.";

// ---------------------------------------------------------------------------
// output: evaluation | error
// ---------------------------------------------------------------------------

/// One `pixel evaluate` answer on stdout: an evaluation or a structured error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Output {
    /// The predicate was evaluated; see [`EvaluationEnvelope::outcome`].
    Evaluation(Box<EvaluationEnvelope>),
    /// The predicate was not evaluated: usage or technical error.
    Error(ErrorEnvelope),
}

impl Output {
    /// The process exit code this output maps to.
    ///
    /// `0` for every evaluation, including `absent_in_snapshot` and
    /// `unknown`; `2` for a usage error; `3` for a technical failure.
    pub fn exit_code(&self) -> i32 {
        match self {
            Output::Evaluation(_) => EXIT_EVALUATED,
            Output::Error(e) => e.code.exit_code(),
        }
    }
}

/// `{"kind": "error", "code", "message", "argument"?}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    pub code: ErrorKind,
    pub message: String,
    /// The CLI argument at fault, for usage errors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument: Option<String>,
}

/// Why an evaluation did not run. Usage errors and technical failures are
/// **not** `unknown` reasons: they carry their own exit codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// A flag or positional argument the command refuses (`--tiers foo`).
    InvalidArgument,
    /// A predicate name the binary does not know.
    UnsupportedPredicate,
    /// Daemon, IO or database failure.
    Internal,
}

impl ErrorKind {
    /// `2` for usage errors, `3` for technical failures.
    pub fn exit_code(self) -> i32 {
        match self {
            ErrorKind::InvalidArgument | ErrorKind::UnsupportedPredicate => EXIT_USAGE,
            ErrorKind::Internal => EXIT_TECHNICAL,
        }
    }
}

// ---------------------------------------------------------------------------
// the evaluation envelope
// ---------------------------------------------------------------------------

/// A completed evaluation, `{"kind": "evaluation", …}` on the wire.
///
/// Serializes through [`EvaluationWire`], which flattens [`Outcome`] into
/// `status`, `answer`, `witness`, `reason` and `next_actions`; deserializing
/// refuses a tuple the enum cannot represent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(into = "EvaluationWire", try_from = "EvaluationWire")]
pub struct EvaluationEnvelope {
    pub predicate: Predicate,
    pub outcome: Outcome,
    pub domain: Domain,
    pub snapshot: Snapshot,
    pub coverage: Coverage,
    pub epistemics: Epistemics,
    /// The fixed template for (predicate, status); see [`summary`].
    pub summary: String,
}

impl EvaluationEnvelope {
    /// Build an envelope and fill `summary` from the fixed templates.
    pub fn new(
        predicate: Predicate,
        outcome: Outcome,
        domain: Domain,
        snapshot: Snapshot,
        coverage: Coverage,
        epistemics: Epistemics,
    ) -> Self {
        let summary = summary(predicate, &outcome, &domain, &snapshot, &coverage);
        EvaluationEnvelope {
            predicate,
            outcome,
            domain,
            snapshot,
            coverage,
            epistemics,
            summary,
        }
    }
}

/// The predicates the binary knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Predicate {
    /// `graph.path_exists`: a path from `--from` to `--to`.
    Path,
    /// `diff.reaches`: a path between the changed symbols and `--to`.
    DiffReaches,
}

/// The three-way result, structured so the decision rule is a type:
/// `established` has a witness, `absent_in_snapshot` has none, `unknown`
/// has a reason.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// A witness was obtained and re-read from the store (`answer: true`).
    Established { witness: Witness },
    /// Exhaustive traversal of the stored relation found nothing
    /// (`answer: false`); true in that relation only.
    AbsentInSnapshot,
    /// The predicate could not be evaluated (`answer: null`).
    Unknown {
        reason: Reason,
        next_actions: Vec<NextAction>,
    },
}

impl Outcome {
    /// The `status` string on the wire.
    pub fn status(&self) -> Status {
        match self {
            Outcome::Established { .. } => Status::Established,
            Outcome::AbsentInSnapshot => Status::AbsentInSnapshot,
            Outcome::Unknown { .. } => Status::Unknown,
        }
    }

    /// The boolean `answer` on the wire: `Some(true)`, `Some(false)`, or
    /// `None` for `unknown`.
    pub fn answer(&self) -> Option<bool> {
        match self {
            Outcome::Established { .. } => Some(true),
            Outcome::AbsentInSnapshot => Some(false),
            Outcome::Unknown { .. } => None,
        }
    }
}

/// The `status` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Established,
    AbsentInSnapshot,
    Unknown,
}

// ---------------------------------------------------------------------------
// domain, snapshot, coverage
// ---------------------------------------------------------------------------

/// Which stored relation was walked, and how.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Domain {
    /// [`RELATION_INDEXED_CALL_GRAPH`] today.
    pub relation: String,
    /// `calls`, `has_method`.
    pub edge_kinds: Vec<String>,
    pub traversal: Traversal,
    /// The tiers admitted, `--tiers`; the relation, not a confidence.
    pub tiers: Vec<Tier>,
}

/// Direction of the walk relative to the call edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Traversal {
    Callees,
    Callers,
}

impl Traversal {
    pub fn as_str(self) -> &'static str {
        match self {
            Traversal::Callees => "callees",
            Traversal::Callers => "callers",
        }
    }
}

/// How an edge was resolved when the graph was built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Same-file scope chain or import-resolved.
    Exact,
    /// Unique name within the import-connected component.
    Probable,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Exact => "exact",
            Tier::Probable => "probable",
        }
    }
}

/// Identity of the graph generation answered from, and whether the working
/// tree matched it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// The graph's stored freshness signature (a content digest).
    pub signature: String,
    pub extractor_version: String,
    /// Rows and signature read belong to the same generation.
    pub generation_coherent: bool,
    pub working_tree_check: WorkingTreeCheck,
    pub working_tree_matches: bool,
}

/// How the working tree was compared to the snapshot.
///
/// There is deliberately no witness-only variant: a check limited to the
/// witness's files can never certify the current tree, so it does not exist
/// on the wire. A `watcher_signed` fast path (trusting the daemon's watcher
/// between full walks) is not added until that design decision is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkingTreeCheck {
    /// A whole-tree walk before and after the traversal, both matching.
    FullBeforeAndAfter,
    /// `--at-snapshot`: the answer is about the stored snapshot; only the
    /// before-check ran.
    BeforeOnly,
}

/// What the traversal covered and where the stored relation stops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coverage {
    pub traversal_exhausted: bool,
    pub depth_cap: u32,
    pub depth_cap_dropped_frontier: bool,
    pub time_budget_ms: u64,
    pub time_budget_hit: bool,
    pub graph_file_cap_hit: bool,
    pub files_excluded_by_size: u64,
    pub unresolved_same_name_sites: u64,
    pub extraction_limits: Vec<String>,
}

// ---------------------------------------------------------------------------
// witness
// ---------------------------------------------------------------------------

/// What established the answer. There is no empty witness: absence of a
/// path is [`Outcome::AbsentInSnapshot`], never a witness with no edges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Witness {
    /// The edges of the path found, in traversal order.
    Path {
        probable_edges: u32,
        edges: Vec<WitnessEdge>,
    },
    /// A zero-length path: the target is itself a source. For
    /// `diff_reaches`, `hunks` are the changed ranges that put it in the
    /// changed set; empty for `path`.
    Identity {
        symbol: SymbolRef,
        hunks: Vec<HunkRange>,
    },
}

/// A symbol as the witness names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolRef {
    /// `path#qualified#kind`, stable across builds.
    pub uid: String,
    pub path: String,
    /// `[start_line, end_line]`, inclusive.
    pub lines: [u32; 2],
    /// Content hash of the file the symbol lives in, as the graph stored it.
    pub content_hash: String,
}

/// One edge of the witness path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WitnessEdge {
    pub from: SymbolRef,
    pub to: SymbolRef,
    pub edge: EdgeInfo,
    /// Always in the call sense, whatever the traversal walked.
    pub call_direction: CallDirection,
    /// 1-based position along the traversal.
    pub traversal_step: u32,
    pub premises: Premises,
}

/// The stored edge behind a witness hop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeInfo {
    /// `calls` or `has_method`.
    pub kind: String,
    pub tier: Tier,
    pub site: CallSite,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver: Option<String>,
}

/// Where the call is written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallSite {
    pub path: String,
    pub line: u32,
}

/// `from→to`: the edge's `from` calls its `to`. Present so a `callers`
/// traversal, which walks edges backwards, cannot be misread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallDirection {
    #[serde(rename = "from→to")]
    FromTo,
}

/// What a verifier needs beyond the two files of an edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Premises {
    /// An `exact` edge: same-file scope or an import in the calling file.
    NotRequired,
    /// A `probable` edge: the import rows linking the two files.
    Imports { imports: Vec<ImportPremise> },
    /// A `probable` edge whose component the store does not describe.
    Unavailable,
}

/// One import row that links a calling file to a defining file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportPremise {
    pub from_path: String,
    pub spec: String,
    pub to_path: String,
}

/// A changed range, in the new file's coordinates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HunkRange {
    pub path: String,
    /// `[start_line, end_line]`, inclusive.
    pub lines: [u32; 2],
}

// ---------------------------------------------------------------------------
// unknown: reasons and next actions
// ---------------------------------------------------------------------------

/// Why an evaluation is `unknown`; tagged on `code`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum Reason {
    /// Several candidates, none exact-unique.
    AmbiguousSymbol {
        argument: String,
        candidates: Vec<Candidate>,
    },
    /// No candidate at any tier; a verbatim uid that does not exist too.
    SymbolNotFound { argument: String },
    /// The file exists on disk but the graph does not hold it, cause known.
    SymbolOutsideIndex {
        argument: String,
        cause: OutsideIndexCause,
    },
    /// Depth or time cap dropped a frontier node.
    TraversalBudgetExhausted {
        parameter: BudgetParameter,
        current: u64,
    },
    /// No graph database.
    GraphUnavailable,
    /// Drift above the incremental threshold.
    GraphStale,
    /// The working tree diverged before or during the evaluation.
    SnapshotChanged,
    /// `diff_reaches`: changed portions without a symbolic anchor.
    UnmappedChanges { motifs: Vec<UncoveredChange> },
    /// `diff_reaches`: deleted symbols with no anchor in the current graph.
    UnanchoredChanges { symbols: Vec<String> },
}

impl Reason {
    /// The sentence the summary prints after "Not evaluated:".
    pub fn sentence(&self) -> String {
        match self {
            Reason::AmbiguousSymbol {
                argument,
                candidates,
            } => {
                let n = candidates.len();
                format!("`{argument}` matches {n} symbols and none is an exact unique match")
            }
            Reason::SymbolNotFound { argument } => format!("no symbol matches `{argument}`"),
            Reason::SymbolOutsideIndex { argument, cause } => {
                let why = cause.phrase();
                format!("`{argument}` names a file the graph does not hold ({why})")
            }
            Reason::TraversalBudgetExhausted { parameter, current } => {
                let flag = parameter.flag();
                format!("the traversal was cut by {flag} at {current} with a non-empty frontier")
            }
            Reason::GraphUnavailable => "no code graph is built for this repository".to_string(),
            Reason::GraphStale => {
                "the code graph drifted from the tree beyond the incremental threshold".to_string()
            }
            Reason::SnapshotChanged => {
                "the working tree changed before or during the evaluation".to_string()
            }
            Reason::UnmappedChanges { motifs } => {
                let n = motifs.len();
                format!("{n} changed range(s) have no symbolic anchor")
            }
            Reason::UnanchoredChanges { symbols } => {
                let n = symbols.len();
                format!("{n} deleted symbol(s) have no anchor in the current graph")
            }
        }
    }
}

/// A symbol the resolver could have meant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub uid: String,
    pub kind: String,
    pub path: String,
    pub line: u32,
}

/// Why a file on disk is not in the graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutsideIndexCause {
    FileCap,
    SizeCap,
    UnsupportedLanguage,
    NotYetIndexed,
}

impl OutsideIndexCause {
    fn phrase(self) -> &'static str {
        match self {
            OutsideIndexCause::FileCap => "beyond the graph file cap",
            OutsideIndexCause::SizeCap => "over the per-file size cap",
            OutsideIndexCause::UnsupportedLanguage => "unsupported language",
            OutsideIndexCause::NotYetIndexed => "not indexed yet",
        }
    }

    /// Whether the cause is intrinsic: no action can bring the file in.
    fn is_terminal(self) -> bool {
        match self {
            OutsideIndexCause::FileCap
            | OutsideIndexCause::SizeCap
            | OutsideIndexCause::UnsupportedLanguage => true,
            OutsideIndexCause::NotYetIndexed => false,
        }
    }
}

/// The budget a traversal ran out of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetParameter {
    MaxDepth,
    TimeBudgetMs,
}

impl BudgetParameter {
    /// The CLI flag that raises this budget.
    pub fn flag(self) -> &'static str {
        match self {
            BudgetParameter::MaxDepth => "--max-depth",
            BudgetParameter::TimeBudgetMs => "--time-budget-ms",
        }
    }
}

/// A changed range with no symbolic anchor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UncoveredChange {
    pub path: String,
    pub motif: UncoveredMotif,
    /// `[start, end]` in the old file, when the range exists there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_lines: Option<[u32; 2]>,
    /// `[start, end]` in the new file, when the range exists there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_lines: Option<[u32; 2]>,
}

/// Why a changed range has no anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UncoveredMotif {
    OutsideSymbol,
    UnsupportedLanguage,
    ExcludedByFileCap,
    ExcludedBySize,
    NotIndexed,
    NonTextChange,
}

/// What a caller can do about an `unknown`; tagged on `kind`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NextAction {
    /// Pick one of the listed candidates by uid, or narrow with `--in`.
    SelectSymbol {
        argument: String,
        accepts: String,
        narrow: String,
    },
    /// Look the name up with the suggested commands first.
    SearchSymbol { suggest: Vec<String> },
    /// Re-run with a larger budget.
    RaiseBudget {
        parameter: BudgetParameter,
        flag: String,
        current: u64,
        suggested: u64,
    },
    /// Build the graph with the named command; never done implicitly.
    BuildGraph { command: String },
    /// Rebuild the graph in full.
    RebuildGraph,
    /// Let the daemon refresh the drifted files.
    RefreshGraph,
    /// Re-run once; the daemon has refreshed in the meantime.
    RetryOnce,
    /// Accept an answer about the stored snapshot.
    EvaluateAtSnapshot { flag: String },
    /// Nothing can be done from here.
    Terminal,
}

impl NextAction {
    /// The phrase the summary prints after "Next:".
    pub fn phrase(&self) -> String {
        match self {
            NextAction::SelectSymbol { argument, .. } => {
                format!("pass a uid for `{argument}` or narrow with --in <path-prefix>")
            }
            NextAction::SearchSymbol { suggest } => {
                let cmds = suggest.join(" or ");
                format!("look the name up with {cmds}")
            }
            NextAction::RaiseBudget {
                flag, suggested, ..
            } => format!("re-run with {flag} {suggested}"),
            NextAction::BuildGraph { command } => format!("run `{command}`"),
            NextAction::RebuildGraph => "rebuild the graph".to_string(),
            NextAction::RefreshGraph => "let the daemon refresh the graph, then re-run".to_string(),
            NextAction::RetryOnce => "re-run once".to_string(),
            NextAction::EvaluateAtSnapshot { flag } => {
                format!("re-run with {flag} to answer about the stored snapshot")
            }
            NextAction::Terminal => "nothing to do; the limit is intrinsic".to_string(),
        }
    }
}

/// Multiplier of the exhausted budget the `raise_budget` action suggests.
const RAISE_BUDGET_FACTOR: u64 = 2;

/// The actions the contract prescribes for a reason.
///
/// Terminal for intrinsic limits (caps, unsupported language, uncovered or
/// unanchored changes); an identical retry is never advised except
/// `retry_once` after `snapshot_changed`, when the daemon has refreshed.
pub fn default_next_actions(reason: &Reason) -> Vec<NextAction> {
    match reason {
        Reason::AmbiguousSymbol { argument, .. } => vec![NextAction::SelectSymbol {
            argument: argument.clone(),
            accepts: "uid".to_string(),
            narrow: "--in <path-prefix>".to_string(),
        }],
        Reason::SymbolNotFound { .. } => vec![NextAction::SearchSymbol {
            suggest: vec![
                "pixel find-symbol".to_string(),
                "pixel find-code".to_string(),
            ],
        }],
        Reason::SymbolOutsideIndex { cause, .. } => {
            if cause.is_terminal() {
                vec![NextAction::Terminal]
            } else {
                vec![NextAction::RefreshGraph]
            }
        }
        Reason::TraversalBudgetExhausted { parameter, current } => vec![NextAction::RaiseBudget {
            parameter: *parameter,
            flag: parameter.flag().to_string(),
            current: *current,
            suggested: current.saturating_mul(RAISE_BUDGET_FACTOR),
        }],
        Reason::GraphUnavailable => vec![NextAction::BuildGraph {
            command: "pixel rebuild-graph".to_string(),
        }],
        Reason::GraphStale => vec![NextAction::RebuildGraph],
        Reason::SnapshotChanged => vec![
            NextAction::RetryOnce,
            NextAction::EvaluateAtSnapshot {
                flag: "--at-snapshot".to_string(),
            },
        ],
        Reason::UnmappedChanges { .. } | Reason::UnanchoredChanges { .. } => {
            vec![NextAction::Terminal]
        }
    }
}

// ---------------------------------------------------------------------------
// summary templates
// ---------------------------------------------------------------------------

/// The first `SIGNATURE_PREFIX_CHARS` of a signature, then `…`.
fn short_signature(signature: &str) -> String {
    let prefix: String = signature.chars().take(SIGNATURE_PREFIX_CHARS).collect();
    format!("{prefix}…")
}

/// `snapshot S, relation R, tiers T, traversal D`: the clause every first
/// sentence carries so a quoted verdict keeps its scope.
fn scope_clause(domain: &Domain, snapshot: &Snapshot) -> String {
    let sig = short_signature(&snapshot.signature);
    let relation = domain.edge_kinds.join("+");
    let tiers = domain
        .tiers
        .iter()
        .map(|t| t.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let traversal = domain.traversal.as_str();
    format!("snapshot {sig}, relation {relation}, tiers {tiers}, traversal {traversal}")
}

/// The fixed summary for (predicate, status). The first sentence stands on
/// its own with the scope; `diff_reaches` appends
/// [`DIFF_REACHES_SCOPE_SENTENCE`].
pub fn summary(
    predicate: Predicate,
    outcome: &Outcome,
    domain: &Domain,
    snapshot: &Snapshot,
    coverage: &Coverage,
) -> String {
    let scope = scope_clause(domain, snapshot);
    let mut text = match outcome {
        Outcome::Established {
            witness:
                Witness::Path {
                    probable_edges,
                    edges,
                },
        } => {
            let n = edges.len();
            format!(
                "Path found in the indexed call graph, {scope}: {n} edge(s), {probable_edges} probable. \
                 This does not establish that the call happens at runtime."
            )
        }
        Outcome::Established {
            witness: Witness::Identity { symbol, .. },
        } => {
            let uid = &symbol.uid;
            format!(
                "Path found in the indexed call graph, {scope}: zero-length, `{uid}` is itself a source. \
                 This does not establish that the call happens at runtime."
            )
        }
        Outcome::AbsentInSnapshot => {
            let exhaustive = if coverage.traversal_exhausted {
                "traversal exhaustive"
            } else {
                "traversal NOT exhaustive"
            };
            format!(
                "No path in the indexed call graph, {scope}, {exhaustive}. \
                 This says nothing about calls outside that relation (other tiers, callbacks, \
                 dynamic dispatch, macros, files beyond caps) or at runtime."
            )
        }
        Outcome::Unknown {
            reason,
            next_actions,
        } => {
            let why = reason.sentence();
            let next = next_actions
                .first()
                .map_or_else(|| "none".to_string(), NextAction::phrase);
            format!("Not evaluated: {why}. Next: {next}.")
        }
    };
    if predicate == Predicate::DiffReaches {
        text.push(' ');
        text.push_str(DIFF_REACHES_SCOPE_SENTENCE);
    }
    text
}

// ---------------------------------------------------------------------------
// wire shape
// ---------------------------------------------------------------------------

/// The flat wire form of [`EvaluationEnvelope`]; `Outcome` spread over
/// `status`, `answer`, `witness`, `reason`, `next_actions`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvaluationWire {
    pub schema_version: u32,
    pub predicate: Predicate,
    pub status: Status,
    pub answer: Option<bool>,
    pub domain: Domain,
    pub snapshot: Snapshot,
    pub coverage: Coverage,
    pub witness: WitnessWire,
    pub reason: Option<Reason>,
    pub next_actions: Vec<NextAction>,
    pub epistemics: Epistemics,
    pub summary: String,
}

/// `witness` on the wire: a [`Witness`] or `{"kind": "none"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WitnessWire {
    Some(Witness),
    None(NoWitness),
}

/// `{"kind": "none"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NoWitness {
    None,
}

impl From<EvaluationEnvelope> for EvaluationWire {
    fn from(e: EvaluationEnvelope) -> Self {
        let status = e.outcome.status();
        let answer = e.outcome.answer();
        let (witness, reason, next_actions) = match e.outcome {
            Outcome::Established { witness } => (WitnessWire::Some(witness), None, Vec::new()),
            Outcome::AbsentInSnapshot => (WitnessWire::None(NoWitness::None), None, Vec::new()),
            Outcome::Unknown {
                reason,
                next_actions,
            } => (
                WitnessWire::None(NoWitness::None),
                Some(reason),
                next_actions,
            ),
        };
        EvaluationWire {
            schema_version: SCHEMA_VERSION,
            predicate: e.predicate,
            status,
            answer,
            domain: e.domain,
            snapshot: e.snapshot,
            coverage: e.coverage,
            witness,
            reason,
            next_actions,
            epistemics: e.epistemics,
            summary: e.summary,
        }
    }
}

/// A wire tuple the [`Outcome`] enum cannot represent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IllegalEvaluation(pub String);

impl fmt::Display for IllegalEvaluation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "illegal evaluation on the wire: {}", self.0)
    }
}

impl std::error::Error for IllegalEvaluation {}

impl TryFrom<EvaluationWire> for EvaluationEnvelope {
    type Error = IllegalEvaluation;

    fn try_from(w: EvaluationWire) -> Result<Self, IllegalEvaluation> {
        if w.schema_version != SCHEMA_VERSION {
            let v = w.schema_version;
            return Err(IllegalEvaluation(format!(
                "schema_version {v}, this reader knows {SCHEMA_VERSION}"
            )));
        }
        let outcome = match (w.status, w.answer, w.witness, w.reason) {
            (Status::Established, Some(true), WitnessWire::Some(witness), None) => {
                if !w.next_actions.is_empty() {
                    return Err(IllegalEvaluation(
                        "established carries next_actions".to_string(),
                    ));
                }
                Outcome::Established { witness }
            }
            (Status::AbsentInSnapshot, Some(false), WitnessWire::None(_), None) => {
                if !w.next_actions.is_empty() {
                    return Err(IllegalEvaluation(
                        "absent_in_snapshot carries next_actions".to_string(),
                    ));
                }
                Outcome::AbsentInSnapshot
            }
            (Status::Unknown, None, WitnessWire::None(_), Some(reason)) => Outcome::Unknown {
                reason,
                next_actions: w.next_actions,
            },
            (status, answer, witness, reason) => {
                let has_witness = matches!(witness, WitnessWire::Some(_));
                let has_reason = reason.is_some();
                return Err(IllegalEvaluation(format!(
                    "status {status:?} with answer {answer:?}, witness {has_witness}, reason {has_reason}"
                )));
            }
        };
        Ok(EvaluationEnvelope {
            predicate: w.predicate,
            outcome,
            domain: w.domain,
            snapshot: w.snapshot,
            coverage: w.coverage,
            epistemics: w.epistemics,
            summary: w.summary,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const SIGNATURE: &str = "3f9c1a2b7e6d5c4b";

    fn domain(traversal: Traversal, tiers: &[Tier]) -> Domain {
        Domain {
            relation: RELATION_INDEXED_CALL_GRAPH.to_string(),
            edge_kinds: vec!["calls".to_string(), "has_method".to_string()],
            traversal,
            tiers: tiers.to_vec(),
        }
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            signature: SIGNATURE.to_string(),
            extractor_version: "7".to_string(),
            generation_coherent: true,
            working_tree_check: WorkingTreeCheck::FullBeforeAndAfter,
            working_tree_matches: true,
        }
    }

    fn coverage(exhausted: bool) -> Coverage {
        Coverage {
            traversal_exhausted: exhausted,
            depth_cap: 8,
            depth_cap_dropped_frontier: !exhausted,
            time_budget_ms: 250,
            time_budget_hit: false,
            graph_file_cap_hit: false,
            files_excluded_by_size: 0,
            unresolved_same_name_sites: 0,
            extraction_limits: vec!["dynamic dispatch".to_string()],
        }
    }

    fn symbol(uid: &str, path: &str) -> SymbolRef {
        SymbolRef {
            uid: uid.to_string(),
            path: path.to_string(),
            lines: [40, 58],
            content_hash: "ab12".to_string(),
        }
    }

    fn edge(tier: Tier, step: u32) -> WitnessEdge {
        WitnessEdge {
            from: symbol("src/a.rs#UserService::delete#method", "src/a.rs"),
            to: symbol("src/b.rs#AccountManager::close#method", "src/b.rs"),
            edge: EdgeInfo {
                kind: "calls".to_string(),
                tier,
                site: CallSite {
                    path: "src/a.rs".to_string(),
                    line: 51,
                },
                receiver: Some("manager".to_string()),
            },
            call_direction: CallDirection::FromTo,
            traversal_step: step,
            premises: match tier {
                Tier::Exact => Premises::NotRequired,
                Tier::Probable => Premises::Imports {
                    imports: vec![ImportPremise {
                        from_path: "src/a.rs".to_string(),
                        spec: "crate::b::AccountManager".to_string(),
                        to_path: "src/b.rs".to_string(),
                    }],
                },
            },
        }
    }

    fn path_witness(probable: u32) -> Witness {
        let tier = if probable == 0 {
            Tier::Exact
        } else {
            Tier::Probable
        };
        Witness::Path {
            probable_edges: probable,
            edges: vec![edge(Tier::Exact, 1), edge(tier, 2)],
        }
    }

    fn envelope(predicate: Predicate, outcome: Outcome) -> EvaluationEnvelope {
        let exhausted = !matches!(
            outcome,
            Outcome::Unknown {
                reason: Reason::TraversalBudgetExhausted { .. },
                ..
            }
        );
        EvaluationEnvelope::new(
            predicate,
            outcome,
            domain(Traversal::Callees, &[Tier::Exact]),
            snapshot(),
            coverage(exhausted),
            Epistemics::default(),
        )
    }

    fn unknown(reason: Reason) -> Outcome {
        let next_actions = default_next_actions(&reason);
        Outcome::Unknown {
            reason,
            next_actions,
        }
    }

    fn every_reason() -> Vec<Reason> {
        vec![
            Reason::AmbiguousSymbol {
                argument: "to".to_string(),
                candidates: vec![Candidate {
                    uid: "src/x.rs#parse#function".to_string(),
                    kind: "function".to_string(),
                    path: "src/x.rs".to_string(),
                    line: 10,
                }],
            },
            Reason::SymbolNotFound {
                argument: "from".to_string(),
            },
            Reason::SymbolOutsideIndex {
                argument: "to".to_string(),
                cause: OutsideIndexCause::FileCap,
            },
            Reason::SymbolOutsideIndex {
                argument: "to".to_string(),
                cause: OutsideIndexCause::SizeCap,
            },
            Reason::SymbolOutsideIndex {
                argument: "to".to_string(),
                cause: OutsideIndexCause::UnsupportedLanguage,
            },
            Reason::SymbolOutsideIndex {
                argument: "to".to_string(),
                cause: OutsideIndexCause::NotYetIndexed,
            },
            Reason::TraversalBudgetExhausted {
                parameter: BudgetParameter::MaxDepth,
                current: 8,
            },
            Reason::TraversalBudgetExhausted {
                parameter: BudgetParameter::TimeBudgetMs,
                current: 250,
            },
            Reason::GraphUnavailable,
            Reason::GraphStale,
            Reason::SnapshotChanged,
            Reason::UnmappedChanges {
                motifs: vec![UncoveredChange {
                    path: "src/a.rs".to_string(),
                    motif: UncoveredMotif::OutsideSymbol,
                    old_lines: Some([3, 3]),
                    new_lines: Some([3, 4]),
                }],
            },
            Reason::UnanchoredChanges {
                symbols: vec!["src/a.rs#gone#function".to_string()],
            },
        ]
    }

    fn to_json(envelope: &EvaluationEnvelope) -> Value {
        serde_json::to_value(Output::Evaluation(Box::new(envelope.clone()))).unwrap()
    }

    fn keys(v: &Value) -> Vec<&str> {
        let mut k: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        k.sort_unstable();
        k
    }

    // -- exit codes ---------------------------------------------------------

    #[test]
    fn exit_code_should_be_zero_for_every_evaluation_status() {
        for outcome in [
            Outcome::Established {
                witness: path_witness(0),
            },
            Outcome::AbsentInSnapshot,
            unknown(Reason::GraphUnavailable),
        ] {
            let out = Output::Evaluation(Box::new(envelope(Predicate::Path, outcome)));
            assert_eq!(out.exit_code(), 0, "{out:?}");
        }
    }

    #[test]
    fn exit_code_should_separate_usage_errors_from_technical_failures() {
        let error = |code: ErrorKind| {
            Output::Error(ErrorEnvelope {
                code,
                message: "x".to_string(),
                argument: None,
            })
        };
        assert_eq!(error(ErrorKind::InvalidArgument).exit_code(), 2);
        assert_eq!(error(ErrorKind::UnsupportedPredicate).exit_code(), 2);
        assert_eq!(error(ErrorKind::Internal).exit_code(), 3);
    }

    #[test]
    fn error_envelope_should_serialize_kind_error_with_snake_case_codes() {
        let out = Output::Error(ErrorEnvelope {
            code: ErrorKind::InvalidArgument,
            message: "unknown tier `foo`".to_string(),
            argument: Some("--tiers".to_string()),
        });
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(
            v,
            json!({"kind": "error", "code": "invalid_argument",
                   "message": "unknown tier `foo`", "argument": "--tiers"})
        );
        let internal = serde_json::to_value(ErrorKind::Internal).unwrap();
        assert_eq!(internal, json!("internal"));
        let unsupported = serde_json::to_value(ErrorKind::UnsupportedPredicate).unwrap();
        assert_eq!(unsupported, json!("unsupported_predicate"));
        let back: Output = serde_json::from_value(v).unwrap();
        assert_eq!(back, out);
    }

    // -- status / answer / witness per outcome ------------------------------

    #[test]
    fn established_should_serialize_true_with_a_witness_and_no_reason() {
        let v = to_json(&envelope(
            Predicate::Path,
            Outcome::Established {
                witness: path_witness(1),
            },
        ));
        assert_eq!(v["kind"], "evaluation");
        assert_eq!(v["schema_version"], 1);
        assert_eq!(v["status"], "established");
        assert_eq!(v["answer"], true);
        assert_eq!(v["witness"]["kind"], "path");
        assert_eq!(v["witness"]["probable_edges"], 1);
        assert_eq!(v["witness"]["edges"].as_array().unwrap().len(), 2);
        assert_eq!(v["reason"], Value::Null);
        assert_eq!(v["next_actions"], json!([]));
    }

    #[test]
    fn identity_should_serialize_as_a_witness_distinct_from_none_and_from_an_empty_path() {
        let v = to_json(&envelope(
            Predicate::DiffReaches,
            Outcome::Established {
                witness: Witness::Identity {
                    symbol: symbol("src/a.rs#charge#function", "src/a.rs"),
                    hunks: vec![HunkRange {
                        path: "src/a.rs".to_string(),
                        lines: [40, 44],
                    }],
                },
            },
        ));
        assert_eq!(v["status"], "established");
        assert_eq!(v["answer"], true);
        assert_eq!(v["witness"]["kind"], "identity");
        assert_eq!(v["witness"]["symbol"]["uid"], "src/a.rs#charge#function");
        assert_eq!(v["witness"]["hunks"][0]["lines"], json!([40, 44]));
        assert!(v["witness"].get("edges").is_none());
    }

    #[test]
    fn absent_should_serialize_false_with_witness_none_and_no_reason() {
        let v = to_json(&envelope(Predicate::Path, Outcome::AbsentInSnapshot));
        assert_eq!(v["status"], "absent_in_snapshot");
        assert_eq!(v["answer"], false);
        assert_eq!(v["witness"], json!({"kind": "none"}));
        assert_eq!(v["reason"], Value::Null);
        assert_eq!(v["next_actions"], json!([]));
    }

    #[test]
    fn unknown_should_serialize_null_with_a_reason_code_and_next_actions() {
        let v = to_json(&envelope(
            Predicate::Path,
            unknown(Reason::TraversalBudgetExhausted {
                parameter: BudgetParameter::MaxDepth,
                current: 8,
            }),
        ));
        assert_eq!(v["status"], "unknown");
        assert_eq!(v["answer"], Value::Null);
        assert_eq!(v["witness"], json!({"kind": "none"}));
        assert_eq!(v["reason"]["code"], "traversal_budget_exhausted");
        assert_eq!(v["reason"]["parameter"], "max_depth");
        assert_eq!(v["reason"]["current"], 8);
        assert_eq!(v["next_actions"][0]["kind"], "raise_budget");
        assert_eq!(v["next_actions"][0]["flag"], "--max-depth");
        assert_eq!(v["next_actions"][0]["current"], 8);
        assert_eq!(v["next_actions"][0]["suggested"], 16);
    }

    #[test]
    fn outcome_should_report_status_and_answer_consistently() {
        let est = Outcome::Established {
            witness: path_witness(0),
        };
        assert_eq!(est.status(), Status::Established);
        assert_eq!(est.answer(), Some(true));
        assert_eq!(Outcome::AbsentInSnapshot.status(), Status::AbsentInSnapshot);
        assert_eq!(Outcome::AbsentInSnapshot.answer(), Some(false));
        let unk = unknown(Reason::GraphStale);
        assert_eq!(unk.status(), Status::Unknown);
        assert_eq!(unk.answer(), None);
    }

    // -- field names pinned -------------------------------------------------

    #[test]
    fn evaluation_should_expose_exactly_the_documented_top_level_fields() {
        let v = to_json(&envelope(
            Predicate::Path,
            Outcome::Established {
                witness: path_witness(0),
            },
        ));
        assert_eq!(
            keys(&v),
            vec![
                "answer",
                "coverage",
                "domain",
                "epistemics",
                "kind",
                "next_actions",
                "predicate",
                "reason",
                "schema_version",
                "snapshot",
                "status",
                "summary",
                "witness",
            ]
        );
        assert_eq!(
            keys(&v["domain"]),
            vec!["edge_kinds", "relation", "tiers", "traversal"]
        );
        assert_eq!(v["domain"]["relation"], "indexed_call_graph");
        assert_eq!(v["domain"]["traversal"], "callees");
        assert_eq!(v["domain"]["tiers"], json!(["exact"]));
        assert_eq!(
            keys(&v["snapshot"]),
            vec![
                "extractor_version",
                "generation_coherent",
                "signature",
                "working_tree_check",
                "working_tree_matches",
            ]
        );
        assert_eq!(v["snapshot"]["working_tree_check"], "full_before_and_after");
        assert_eq!(
            keys(&v["coverage"]),
            vec![
                "depth_cap",
                "depth_cap_dropped_frontier",
                "extraction_limits",
                "files_excluded_by_size",
                "graph_file_cap_hit",
                "time_budget_hit",
                "time_budget_ms",
                "traversal_exhausted",
                "unresolved_same_name_sites",
            ]
        );
    }

    #[test]
    fn witness_edge_should_expose_the_documented_fields_and_call_direction() {
        let v = to_json(&envelope(
            Predicate::Path,
            Outcome::Established {
                witness: path_witness(1),
            },
        ));
        let e0 = &v["witness"]["edges"][0];
        assert_eq!(
            keys(e0),
            vec![
                "call_direction",
                "edge",
                "from",
                "premises",
                "to",
                "traversal_step",
            ]
        );
        assert_eq!(e0["call_direction"], "from→to");
        assert_eq!(e0["traversal_step"], 1);
        assert_eq!(
            keys(&e0["from"]),
            vec!["content_hash", "lines", "path", "uid"]
        );
        assert_eq!(e0["from"]["lines"], json!([40, 58]));
        assert_eq!(keys(&e0["edge"]), vec!["kind", "receiver", "site", "tier"]);
        assert_eq!(e0["edge"]["tier"], "exact");
        assert_eq!(e0["edge"]["site"], json!({"path": "src/a.rs", "line": 51}));
        assert_eq!(e0["premises"], json!({"kind": "not_required"}));
        let e1 = &v["witness"]["edges"][1];
        assert_eq!(e1["edge"]["tier"], "probable");
        assert_eq!(e1["premises"]["kind"], "imports");
        assert_eq!(
            e1["premises"]["imports"][0]["spec"],
            "crate::b::AccountManager"
        );
    }

    #[test]
    fn premises_unavailable_should_be_its_own_kind() {
        let v = serde_json::to_value(Premises::Unavailable).unwrap();
        assert_eq!(v, json!({"kind": "unavailable"}));
    }

    #[test]
    fn working_tree_check_should_have_only_the_two_documented_values() {
        assert_eq!(
            serde_json::to_value(WorkingTreeCheck::FullBeforeAndAfter).unwrap(),
            json!("full_before_and_after")
        );
        assert_eq!(
            serde_json::to_value(WorkingTreeCheck::BeforeOnly).unwrap(),
            json!("before_only")
        );
        let refused: Result<WorkingTreeCheck, _> = serde_json::from_value(json!("witness_only"));
        assert!(refused.is_err(), "witness_only must not exist on the wire");
    }

    #[test]
    fn predicate_and_traversal_should_serialize_snake_case() {
        assert_eq!(
            serde_json::to_value(Predicate::Path).unwrap(),
            json!("path")
        );
        assert_eq!(
            serde_json::to_value(Predicate::DiffReaches).unwrap(),
            json!("diff_reaches")
        );
        assert_eq!(
            serde_json::to_value(Traversal::Callers).unwrap(),
            json!("callers")
        );
        assert_eq!(Traversal::Callers.as_str(), "callers");
        assert_eq!(Traversal::Callees.as_str(), "callees");
        assert_eq!(Tier::Probable.as_str(), "probable");
        assert_eq!(Tier::Exact.as_str(), "exact");
    }

    // -- reasons and next actions -------------------------------------------

    #[test]
    fn every_reason_should_serialize_its_code_in_snake_case() {
        let expected = [
            "ambiguous_symbol",
            "symbol_not_found",
            "symbol_outside_index",
            "symbol_outside_index",
            "symbol_outside_index",
            "symbol_outside_index",
            "traversal_budget_exhausted",
            "traversal_budget_exhausted",
            "graph_unavailable",
            "graph_stale",
            "snapshot_changed",
            "unmapped_changes",
            "unanchored_changes",
        ];
        let reasons = every_reason();
        assert_eq!(reasons.len(), expected.len());
        for (reason, code) in reasons.iter().zip(expected) {
            let v = serde_json::to_value(reason).unwrap();
            assert_eq!(v["code"], code, "{reason:?}");
            let back: Reason = serde_json::from_value(v).unwrap();
            assert_eq!(&back, reason);
        }
    }

    #[test]
    fn ambiguous_symbol_should_carry_candidates_with_uids_and_the_select_action() {
        let reason = &every_reason()[0];
        let v = serde_json::to_value(reason).unwrap();
        assert_eq!(v["argument"], "to");
        assert_eq!(
            keys(&v["candidates"][0]),
            vec!["kind", "line", "path", "uid"]
        );
        let actions = default_next_actions(reason);
        assert_eq!(
            actions,
            vec![NextAction::SelectSymbol {
                argument: "to".to_string(),
                accepts: "uid".to_string(),
                narrow: "--in <path-prefix>".to_string(),
            }]
        );
        assert_eq!(
            serde_json::to_value(&actions[0]).unwrap()["kind"],
            "select_symbol"
        );
    }

    #[test]
    fn default_next_actions_should_follow_the_table_row_by_row() {
        let rows: Vec<(Reason, Vec<NextAction>)> = vec![
            (
                Reason::SymbolNotFound {
                    argument: "from".to_string(),
                },
                vec![NextAction::SearchSymbol {
                    suggest: vec![
                        "pixel find-symbol".to_string(),
                        "pixel find-code".to_string(),
                    ],
                }],
            ),
            (
                Reason::SymbolOutsideIndex {
                    argument: "to".to_string(),
                    cause: OutsideIndexCause::FileCap,
                },
                vec![NextAction::Terminal],
            ),
            (
                Reason::SymbolOutsideIndex {
                    argument: "to".to_string(),
                    cause: OutsideIndexCause::SizeCap,
                },
                vec![NextAction::Terminal],
            ),
            (
                Reason::SymbolOutsideIndex {
                    argument: "to".to_string(),
                    cause: OutsideIndexCause::UnsupportedLanguage,
                },
                vec![NextAction::Terminal],
            ),
            (
                Reason::SymbolOutsideIndex {
                    argument: "to".to_string(),
                    cause: OutsideIndexCause::NotYetIndexed,
                },
                vec![NextAction::RefreshGraph],
            ),
            (
                Reason::TraversalBudgetExhausted {
                    parameter: BudgetParameter::TimeBudgetMs,
                    current: 250,
                },
                vec![NextAction::RaiseBudget {
                    parameter: BudgetParameter::TimeBudgetMs,
                    flag: "--time-budget-ms".to_string(),
                    current: 250,
                    suggested: 500,
                }],
            ),
            (
                Reason::GraphUnavailable,
                vec![NextAction::BuildGraph {
                    command: "pixel rebuild-graph".to_string(),
                }],
            ),
            (Reason::GraphStale, vec![NextAction::RebuildGraph]),
            (
                Reason::SnapshotChanged,
                vec![
                    NextAction::RetryOnce,
                    NextAction::EvaluateAtSnapshot {
                        flag: "--at-snapshot".to_string(),
                    },
                ],
            ),
            (
                Reason::UnmappedChanges { motifs: vec![] },
                vec![NextAction::Terminal],
            ),
            (
                Reason::UnanchoredChanges {
                    symbols: vec!["x".to_string()],
                },
                vec![NextAction::Terminal],
            ),
        ];
        for (reason, expected) in rows {
            assert_eq!(default_next_actions(&reason), expected, "{reason:?}");
        }
    }

    #[test]
    fn raise_budget_should_saturate_instead_of_overflowing() {
        let actions = default_next_actions(&Reason::TraversalBudgetExhausted {
            parameter: BudgetParameter::MaxDepth,
            current: u64::MAX,
        });
        assert_eq!(
            actions,
            vec![NextAction::RaiseBudget {
                parameter: BudgetParameter::MaxDepth,
                flag: "--max-depth".to_string(),
                current: u64::MAX,
                suggested: u64::MAX,
            }]
        );
    }

    #[test]
    fn budget_parameter_should_name_its_flag_and_serialize_snake_case() {
        assert_eq!(BudgetParameter::MaxDepth.flag(), "--max-depth");
        assert_eq!(BudgetParameter::TimeBudgetMs.flag(), "--time-budget-ms");
        assert_eq!(
            serde_json::to_value(BudgetParameter::TimeBudgetMs).unwrap(),
            json!("time_budget_ms")
        );
    }

    #[test]
    fn next_action_kinds_should_serialize_snake_case() {
        let cases: Vec<(NextAction, &str)> = vec![
            (
                NextAction::SearchSymbol { suggest: vec![] },
                "search_symbol",
            ),
            (
                NextAction::BuildGraph {
                    command: "c".to_string(),
                },
                "build_graph",
            ),
            (NextAction::RebuildGraph, "rebuild_graph"),
            (NextAction::RefreshGraph, "refresh_graph"),
            (NextAction::RetryOnce, "retry_once"),
            (
                NextAction::EvaluateAtSnapshot {
                    flag: "--at-snapshot".to_string(),
                },
                "evaluate_at_snapshot",
            ),
            (NextAction::Terminal, "terminal"),
        ];
        for (action, kind) in cases {
            let v = serde_json::to_value(&action).unwrap();
            assert_eq!(v["kind"], kind, "{action:?}");
            let back: NextAction = serde_json::from_value(v).unwrap();
            assert_eq!(back, action);
        }
    }

    #[test]
    fn uncovered_change_should_serialize_motifs_and_optional_ranges() {
        let v = serde_json::to_value(UncoveredChange {
            path: "cfg.toml".to_string(),
            motif: UncoveredMotif::UnsupportedLanguage,
            old_lines: None,
            new_lines: Some([1, 2]),
        })
        .unwrap();
        assert_eq!(
            v,
            json!({"path": "cfg.toml", "motif": "unsupported_language", "new_lines": [1, 2]})
        );
        for (motif, name) in [
            (UncoveredMotif::OutsideSymbol, "outside_symbol"),
            (UncoveredMotif::ExcludedByFileCap, "excluded_by_file_cap"),
            (UncoveredMotif::ExcludedBySize, "excluded_by_size"),
            (UncoveredMotif::NotIndexed, "not_indexed"),
            (UncoveredMotif::NonTextChange, "non_text_change"),
        ] {
            assert_eq!(serde_json::to_value(motif).unwrap(), json!(name));
        }
    }

    // -- summaries ----------------------------------------------------------

    #[test]
    fn established_summary_should_open_with_the_scope_and_count_edges() {
        let e = envelope(
            Predicate::Path,
            Outcome::Established {
                witness: path_witness(1),
            },
        );
        assert_eq!(
            e.summary,
            "Path found in the indexed call graph, snapshot 3f9c1a2b…, relation calls+has_method, \
             tiers exact, traversal callees: 2 edge(s), 1 probable. \
             This does not establish that the call happens at runtime."
        );
    }

    #[test]
    fn identity_summary_should_name_the_zero_length_path() {
        let e = envelope(
            Predicate::Path,
            Outcome::Established {
                witness: Witness::Identity {
                    symbol: symbol("src/a.rs#charge#function", "src/a.rs"),
                    hunks: vec![],
                },
            },
        );
        assert!(
            e.summary.starts_with(
                "Path found in the indexed call graph, snapshot 3f9c1a2b…, relation calls+has_method, \
                 tiers exact, traversal callees: zero-length, `src/a.rs#charge#function` is itself a source."
            ),
            "{}",
            e.summary
        );
    }

    #[test]
    fn absent_summary_should_say_no_path_exhaustive_and_bound_the_claim() {
        let e = envelope(Predicate::Path, Outcome::AbsentInSnapshot);
        assert_eq!(
            e.summary,
            "No path in the indexed call graph, snapshot 3f9c1a2b…, relation calls+has_method, \
             tiers exact, traversal callees, traversal exhaustive. \
             This says nothing about calls outside that relation (other tiers, callbacks, \
             dynamic dispatch, macros, files beyond caps) or at runtime."
        );
    }

    #[test]
    fn absent_summary_should_not_claim_exhaustive_when_coverage_says_otherwise() {
        let text = summary(
            Predicate::Path,
            &Outcome::AbsentInSnapshot,
            &domain(Traversal::Callers, &[Tier::Exact, Tier::Probable]),
            &snapshot(),
            &coverage(false),
        );
        assert!(text.contains("traversal NOT exhaustive"), "{text}");
        assert!(
            text.contains("tiers exact,probable, traversal callers"),
            "{text}"
        );
    }

    #[test]
    fn unknown_summary_should_name_the_reason_and_the_first_action() {
        let e = envelope(
            Predicate::Path,
            unknown(Reason::AmbiguousSymbol {
                argument: "to".to_string(),
                candidates: vec![],
            }),
        );
        assert_eq!(
            e.summary,
            "Not evaluated: `to` matches 0 symbols and none is an exact unique match. \
             Next: pass a uid for `to` or narrow with --in <path-prefix>."
        );
    }

    #[test]
    fn unknown_summary_should_say_none_when_no_action_is_listed() {
        let text = summary(
            Predicate::Path,
            &Outcome::Unknown {
                reason: Reason::GraphStale,
                next_actions: vec![],
            },
            &domain(Traversal::Callees, &[Tier::Exact]),
            &snapshot(),
            &coverage(true),
        );
        assert_eq!(
            text,
            "Not evaluated: the code graph drifted from the tree beyond the incremental threshold. \
             Next: none."
        );
    }

    #[test]
    fn every_reason_should_have_a_distinct_sentence_and_the_first_action_a_phrase() {
        let reasons = every_reason();
        let mut sentences: Vec<String> = reasons.iter().map(Reason::sentence).collect();
        for s in &sentences {
            assert!(!s.is_empty());
        }
        sentences.sort();
        sentences.dedup();
        // The four outside-index causes share a template but not a phrase,
        // and the two budget parameters differ by flag: all 13 distinct.
        assert_eq!(sentences.len(), reasons.len());
        for reason in &reasons {
            for action in default_next_actions(reason) {
                assert!(!action.phrase().is_empty(), "{action:?}");
            }
        }
    }

    #[test]
    fn reason_sentences_should_name_the_argument_the_cause_and_the_flag() {
        let s = Reason::SymbolOutsideIndex {
            argument: "to".to_string(),
            cause: OutsideIndexCause::SizeCap,
        }
        .sentence();
        assert_eq!(
            s,
            "`to` names a file the graph does not hold (over the per-file size cap)"
        );
        let s = Reason::TraversalBudgetExhausted {
            parameter: BudgetParameter::TimeBudgetMs,
            current: 250,
        }
        .sentence();
        assert_eq!(
            s,
            "the traversal was cut by --time-budget-ms at 250 with a non-empty frontier"
        );
        let s = Reason::SymbolNotFound {
            argument: "from".to_string(),
        }
        .sentence();
        assert_eq!(s, "no symbol matches `from`");
        let s = Reason::UnmappedChanges { motifs: vec![] }.sentence();
        assert_eq!(s, "0 changed range(s) have no symbolic anchor");
        let s = Reason::UnanchoredChanges {
            symbols: vec!["a".to_string(), "b".to_string()],
        }
        .sentence();
        assert_eq!(s, "2 deleted symbol(s) have no anchor in the current graph");
        assert_eq!(
            Reason::GraphUnavailable.sentence(),
            "no code graph is built for this repository"
        );
        assert_eq!(
            Reason::SnapshotChanged.sentence(),
            "the working tree changed before or during the evaluation"
        );
    }

    #[test]
    fn next_action_phrases_should_carry_their_parameters() {
        let raise = NextAction::RaiseBudget {
            parameter: BudgetParameter::MaxDepth,
            flag: "--max-depth".to_string(),
            current: 8,
            suggested: 16,
        };
        assert_eq!(raise.phrase(), "re-run with --max-depth 16");
        let search = NextAction::SearchSymbol {
            suggest: vec![
                "pixel find-symbol".to_string(),
                "pixel find-code".to_string(),
            ],
        };
        assert_eq!(
            search.phrase(),
            "look the name up with pixel find-symbol or pixel find-code"
        );
        let build = NextAction::BuildGraph {
            command: "pixel rebuild-graph".to_string(),
        };
        assert_eq!(build.phrase(), "run `pixel rebuild-graph`");
        let at = NextAction::EvaluateAtSnapshot {
            flag: "--at-snapshot".to_string(),
        };
        assert_eq!(
            at.phrase(),
            "re-run with --at-snapshot to answer about the stored snapshot"
        );
        assert_eq!(NextAction::RetryOnce.phrase(), "re-run once");
        assert_eq!(NextAction::RebuildGraph.phrase(), "rebuild the graph");
        assert_eq!(
            NextAction::RefreshGraph.phrase(),
            "let the daemon refresh the graph, then re-run"
        );
        assert_eq!(
            NextAction::Terminal.phrase(),
            "nothing to do; the limit is intrinsic"
        );
    }

    #[test]
    fn diff_reaches_summary_should_append_the_scope_sentence_and_path_should_not() {
        for outcome in [
            Outcome::Established {
                witness: path_witness(0),
            },
            Outcome::AbsentInSnapshot,
            unknown(Reason::GraphUnavailable),
        ] {
            let diff = envelope(Predicate::DiffReaches, outcome.clone());
            let path = envelope(Predicate::Path, outcome);
            assert!(
                diff.summary
                    .ends_with(&format!(" {DIFF_REACHES_SCOPE_SENTENCE}")),
                "{}",
                diff.summary
            );
            assert!(!path.summary.contains(DIFF_REACHES_SCOPE_SENTENCE));
            assert_eq!(
                diff.summary
                    .trim_end_matches(DIFF_REACHES_SCOPE_SENTENCE)
                    .trim_end(),
                path.summary
            );
        }
    }

    #[test]
    fn short_signature_should_keep_eight_chars_then_an_ellipsis() {
        assert_eq!(short_signature("3f9c1a2b7e6d5c4b"), "3f9c1a2b…");
        assert_eq!(short_signature("abc"), "abc…");
    }

    // -- round trips and refusals -------------------------------------------

    #[test]
    fn every_outcome_should_round_trip_through_json() {
        let mut outcomes = vec![
            Outcome::Established {
                witness: path_witness(1),
            },
            Outcome::Established {
                witness: Witness::Identity {
                    symbol: symbol("u", "p"),
                    hunks: vec![],
                },
            },
            Outcome::AbsentInSnapshot,
        ];
        outcomes.extend(every_reason().into_iter().map(unknown));
        for outcome in outcomes {
            let e = envelope(Predicate::DiffReaches, outcome);
            let out = Output::Evaluation(Box::new(e.clone()));
            let text = serde_json::to_string(&out).unwrap();
            let back: Output = serde_json::from_str(&text).unwrap();
            assert_eq!(back, out);
        }
    }

    #[test]
    fn deserialize_should_refuse_established_without_a_witness() {
        let mut v = to_json(&envelope(
            Predicate::Path,
            Outcome::Established {
                witness: path_witness(0),
            },
        ));
        v["witness"] = json!({"kind": "none"});
        let err = serde_json::from_value::<Output>(v).unwrap_err().to_string();
        assert!(err.contains("illegal evaluation"), "{err}");
    }

    #[test]
    fn deserialize_should_refuse_a_true_answer_on_absent() {
        let mut v = to_json(&envelope(Predicate::Path, Outcome::AbsentInSnapshot));
        v["answer"] = json!(true);
        let err = serde_json::from_value::<Output>(v).unwrap_err().to_string();
        assert!(err.contains("illegal evaluation"), "{err}");
    }

    #[test]
    fn deserialize_should_refuse_unknown_without_a_reason_and_a_reason_on_established() {
        let mut v = to_json(&envelope(Predicate::Path, unknown(Reason::GraphStale)));
        v["reason"] = Value::Null;
        let err = serde_json::from_value::<Output>(v).unwrap_err().to_string();
        assert!(err.contains("illegal evaluation"), "{err}");

        let mut v = to_json(&envelope(
            Predicate::Path,
            Outcome::Established {
                witness: path_witness(0),
            },
        ));
        v["reason"] = json!({"code": "graph_stale"});
        let err = serde_json::from_value::<Output>(v).unwrap_err().to_string();
        assert!(err.contains("illegal evaluation"), "{err}");
    }

    #[test]
    fn deserialize_should_refuse_next_actions_outside_unknown() {
        let mut v = to_json(&envelope(Predicate::Path, Outcome::AbsentInSnapshot));
        v["next_actions"] = json!([{"kind": "terminal"}]);
        let err = serde_json::from_value::<Output>(v).unwrap_err().to_string();
        assert!(err.contains("carries next_actions"), "{err}");
    }

    #[test]
    fn deserialize_should_refuse_another_schema_version() {
        let mut v = to_json(&envelope(Predicate::Path, Outcome::AbsentInSnapshot));
        v["schema_version"] = json!(2);
        let err = serde_json::from_value::<Output>(v).unwrap_err().to_string();
        assert!(err.contains("schema_version 2"), "{err}");
    }

    #[test]
    fn illegal_evaluation_should_display_its_detail() {
        let e = IllegalEvaluation("x".to_string());
        assert_eq!(e.to_string(), "illegal evaluation on the wire: x");
    }
}
