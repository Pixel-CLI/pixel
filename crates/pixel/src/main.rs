// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! pixel CLI — index/search plus the graph command surface, speaking to a
//! per-root daemon over its Unix socket when one is up, else in-process.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::{ArgMatches, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};

// Count rendered writes without changing descriptors, native streams, or TTY state.
macro_rules! print {
    ($($arg:tt)*) => { crate::operation_metrics::print(format_args!($($arg)*)) };
}
macro_rules! println {
    () => { crate::operation_metrics::print(format_args!("\n")) };
    ($($arg:tt)*) => { crate::operation_metrics::print(format_args!("{}\n", format_args!($($arg)*))) };
}
macro_rules! eprintln {
    () => { crate::operation_metrics::print_error(format_args!("\n")) };
    ($($arg:tt)*) => { crate::operation_metrics::print_error(format_args!("{}\n", format_args!($($arg)*))) };
}
macro_rules! eprint {
    ($($arg:tt)*) => { crate::operation_metrics::print_error(format_args!($($arg)*)) };
}
mod audit_cmd;
mod brief_parts;
mod call_guard;
mod classify;
mod classify_eval;
mod classify_history;
mod classify_history_cmd;
mod classify_setup;
mod config_cmd;
mod config_file;
mod coverage_cmd;
mod cycles_cmd;
mod decide_clef;
mod decide_infisical;
mod decide_jev;
mod decide_ollaya;
mod decide_remote;
mod evaluate_cmd;
mod execution_brief;
mod graph_shadow_cmd;
mod hook_input;
mod hook_metrics;
mod impact_read;
mod index_cmd;

mod operation_metrics;
mod overview_intent;
mod plan_cmd;
mod plan_state;
mod plugin_cmd;
mod prompt_continuation;
mod prompt_intent;
mod prompt_key;
mod reference_cmd;
mod removed_commands;
mod search_compat;
mod search_filter;
mod serve_trace;
mod setup_cmd;
mod task_bridge;
mod task_commands;
mod task_config;
mod task_hook;
mod task_prepare;
mod task_route;
mod task_runtime;
mod task_verify;
use task_commands::TaskCmd;

mod select;
mod update_notice;
mod web_search;
mod web_search_setup;
mod workspace_cmd;
use pixel_actionlog::{InProcessReason, ServeRoute, ServeStep};
use pixel_daemon::api::{PROTOCOL_VERSION, Request, Response, Service, failure_response};
use pixel_daemon::daemon;
use pixel_index::index::build;
use pixel_index::{Crc32Weigher, GramExtractor, SparseGramExtractor, TrigramExtractor};
use pixel_proto::{
    QueryKind, QueryStatus, TargetsFactsResult, TargetsFactsUnavailableReason, compile_query,
};
use serde_json::{Value, json};

/// `pixel --version` (long form): the crate version plus where the binary
/// came from, all captured by `build.rs` at compile time (`unknown` when a
/// value could not be determined, e.g. a tarball build without `.git`).
/// `pixel -V` keeps the one-line `pixel x.y.z`.
const LONG_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "\ncommit: ",
    env!("PIXEL_BUILD_COMMIT"),
    "\ntarget: ",
    env!("PIXEL_BUILD_TARGET"),
    "\nrustc: ",
    env!("PIXEL_RUSTC_VERSION"),
    "\nbuilt: ",
    env!("PIXEL_BUILD_DATE"),
);

#[derive(Parser)]
#[command(
    name = "pixel",
    version,
    long_version = LONG_VERSION,
    about = "Fast, fresh code retrieval for agents"
)]
struct Cli {
    /// Disable live metrics (accounting remains available in the local action log).
    #[arg(long, global = true, default_value = "on", value_parser = ["on", "off"])]
    metrics: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Copy, Clone, ValueEnum)]
enum ExtractorKind {
    Sparse,
    Trigram,
}

#[derive(Copy, Clone, ValueEnum)]
enum DirectionArg {
    Upstream,
    Downstream,
}

#[derive(Copy, Clone, ValueEnum)]
enum RoleArg {
    Callers,
    Callees,
}

/// `pixel doctor --fail-on`: the lowest check status that exits 1.
#[derive(Copy, Clone, ValueEnum)]
enum FailOn {
    Yellow,
    Red,
}

/// `pixel review-gate --fail-on`: the lowest finding severity that exits 1.
/// The names are the code-review vocabulary the human render prints.
#[derive(Copy, Clone, ValueEnum)]
enum ReviewFailOn {
    Blocker,
    Concern,
    Suggestion,
    Nit,
}

impl ReviewFailOn {
    /// The severity rank (LOW 1 .. CRITICAL 4) at or above which the gate fails.
    fn threshold(self) -> u8 {
        match self {
            Self::Blocker => 4,
            Self::Concern => 3,
            Self::Suggestion => 2,
            Self::Nit => 1,
        }
    }
}

impl FailOn {
    fn threshold(self) -> pixel_setup::doctor::CheckStatus {
        match self {
            Self::Yellow => pixel_setup::doctor::CheckStatus::Yellow,
            Self::Red => pixel_setup::doctor::CheckStatus::Red,
        }
    }
}

/// One lock for every unit test that mutates process-wide state (`HOME`
/// and friends): two module-local locks cannot see each other, so all
/// such tests share this single mutex.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Subcommand)]
enum Command {
    /// Build (or rebuild) the text index for a directory tree.
    BuildIndex {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, value_enum, default_value = "trigram")]
        extractor: ExtractorKind,
        /// Maximum sparse gram length (ignored for trigram).
        #[arg(long, default_value_t = pixel_index::gram::DEFAULT_MAX_GRAM)]
        max_gram: usize,
        /// Also ingest the facts/history db (commit metadata + diff text).
        #[arg(long)]
        history: bool,
    },
    /// Search the indexed tree with a regex pattern. Accepts any number of
    /// paths (repo roots, subdirectories, or files) — ripgrep-style; the repo
    /// root is discovered automatically for each.
    SearchContent {
        pattern: String,
        /// Paths to search: repo roots, subdirectories, or files (any mix).
        #[arg(default_value = ".")]
        paths: Vec<PathBuf>,
        /// Emit ndjson matches instead of text lines.
        #[arg(long)]
        json: bool,
        /// Print candidate/timing stats to stderr.
        #[arg(long)]
        stats: bool,
        /// Maximum matching lines to return (hard-capped at 10,000).
        #[arg(long)]
        limit: Option<usize>,
        /// Skip this many matching lines for page-wise retrieval.
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Skip the daemon even if one is running.
        #[arg(long)]
        no_daemon: bool,
        /// Ranking scope. `code` reranks matches by file-level signals
        /// (filename match, symbol match, content density) via pixel-rank's
        /// RRF without changing the hit set. `hybrid` adds a semantic
        /// embedding channel (static code embeddings, potion-code-16M-v2)
        /// fused as a 6th RRF signal — better recall on paraphrase/synonym
        /// queries at ~ms cost when the model is cached; degrades to `code`
        /// if the model is unavailable. Any other value is an error. Omit
        /// `scope` for unranked (path/line) order.
        #[arg(long)]
        scope: Option<String>,
        /// Lines of context to include around each match (reads the file
        /// on-demand). Eliminates the need for a follow-up Read call.
        #[arg(long, default_value_t = 0)]
        context: usize,
        /// Case-insensitive search (ripgrep-compatible shorthand).
        #[arg(short = 'i', long = "ignore-case")]
        ignore_case: bool,
        /// Only keep paths matching this glob, ripgrep-style: `.gitignore`
        /// rules (`*.rs` at any depth, a `/` anchors at the root), `!`
        /// excludes. Repeatable; the last glob matching a path decides.
        #[arg(short = 'g', long = "glob")]
        globs: Vec<String>,
        /// Only keep files of this type, ripgrep-style (`rust`, `py`, `ts`,
        /// `md`, …). Repeatable.
        #[arg(short = 't', long = "type")]
        types: Vec<String>,
        /// Print only the path of each file with a match (ripgrep `-l`).
        #[arg(short = 'l', long = "files-with-matches", conflicts_with = "json")]
        files_with_matches: bool,
        /// Match the pattern as a literal string, not a regex (ripgrep `-F`).
        #[arg(short = 'F', long = "fixed-strings")]
        fixed_strings: bool,
        /// Run one task-aware find-code query when an exact lookup has no hits.
        #[arg(long, hide = true)]
        fallback_query: Option<String>,
        /// Accepted for ripgrep compatibility; matches always carry their
        /// line number.
        #[arg(short = 'n', long = "line-number")]
        line_number: bool,
    },
    /// Native-output literal file search for automatic routing; unsupported
    /// inputs execute the original rg/grep command without modification.
    SearchLikeRg {
        #[arg(value_enum)]
        tool: search_compat::SearchTool,
        #[arg(last = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Compile and execute one bounded deterministic retrieval recipe.
    RunRecipe {
        intent: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value = "auto")]
        kind: String,
        #[arg(long, default_value_t = 800)]
        budget: usize,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        no_daemon: bool,
    },
    /// Semantic code search: embed a natural-language question ("how is
    /// authentication handled?") and rank files by semantic/lexical rank fusion.
    /// Complements `search` (regex) and `resolve` (deterministic phrase→code);
    /// the answer is a ranked list, not a resolved certainty. First use
    /// downloads the embedding model into the shared recall model cache
    /// (once; subsequent calls are offline). At a root carrying a pixel
    /// index, chunk vectors persist in .pixel/code-vectors, so a repeated
    /// question embeds only the code that changed. Tests, configuration and
    /// data files and docs rank below code unless the question names them
    /// ("test", "config", "readme"...); a JSON hit's `demoted` says which.
    SearchMeaning {
        /// The natural-language question.
        question: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Number of ranked hits to return.
        #[arg(long, default_value_t = pixel_recall::code_search::DEFAULT_LIMIT)]
        limit: usize,
        // Help text built from the ceiling constant: `max_files_help`.
        #[arg(long, help = max_files_help())]
        max_files: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// Sniper target list: task description in, closed prioritized file list
    /// out (P0 = start here, P1 = likely, P2 = droppable). Writes the
    /// enforcement manifest .pixel/targets.json unless --no-manifest.
    /// With --regions, also writes .pixel/regions.json: symbol-level edit
    /// territories with conservative conflict pairs, merge-order layers, and
    /// declared shared files (evidence for a parallel-edit harness, never an
    /// action recommendation).
    ScopeTask {
        /// Task/feature description (omit with --clear).
        task: Option<String>,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
        /// Maximum files in the closed list (default 20, max 100).
        #[arg(long)]
        limit: Option<usize>,
        /// Skip writing the enforcement manifest.
        #[arg(long)]
        no_manifest: bool,
        /// Also write .pixel/regions.json: symbol line ranges, conservative
        /// conflict pairs, merge-order layers, and shared files. Suppressed
        /// by --no-manifest and --read-only.
        #[arg(long)]
        regions: bool,
        /// Serve only already-published deterministic facts from a compatible
        /// running daemon. Never starts a daemon or builds/refreshes indexes.
        /// This mode never writes a targets manifest.
        #[arg(long)]
        read_only: bool,
        /// Deactivate scoping: delete .pixel/targets.json and exit.
        #[arg(long)]
        clear: bool,
        /// Drop files above this tier: "P0" = P0 only, "P1" = P0+P1, "P2" = all.
        #[arg(long)]
        max_tier: Option<String>,
        /// Precision mode: drop low-score P1/P2 files when there's a sharp
        /// score gap after P0. Improves precision on simple tasks.
        #[arg(long)]
        precision: bool,
    },
    /// `pixel brief "<prompt>"` — the evidence brief a prompt-submit hook
    /// injects for Claude and Codex, on stdout. Harnesses without a
    /// prompt-submit hook (OpenCode's `pixel-brief` plugin) call this
    /// directly. Empty output means no brief (a non-code prompt,
    /// an unindexed repository, or `PIXEL_BRIEF=0`).
    Brief {
        /// The user's prompt, verbatim.
        prompt: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Print the decision (signal, gate, score, judge, cost) and the
        /// rendered brief as one JSON object, also when no brief was made.
        #[arg(long)]
        json: bool,
        /// Hook-sized parts the caller accepts (1 = one message under 4 000
        /// bytes). Above 1 the brief and its probes get the budget of an
        /// N-part prompt hook, printed as one text.
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=8))]
        parts: u8,
    },
    /// Build a deterministic, bounded execution brief from scope-task evidence.
    ExecutionBrief {
        /// Task/feature description.
        task: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
        /// Maximum files in the scope evidence (default 20, max 100).
        #[arg(long)]
        limit: Option<usize>,
        /// Drop files above this tier: "P0" = P0 only, "P1" = P0+P1.
        #[arg(long)]
        max_tier: Option<String>,
        /// Precision mode: drop low-score P1 files after a sharp score gap.
        #[arg(long)]
        precision: bool,
        /// Accepted for parity with scope-task; execution-brief never writes a manifest.
        #[arg(long)]
        no_manifest: bool,
        /// Skip the daemon and use the in-process service.
        #[arg(long)]
        no_daemon: bool,
    },
    /// Persistent JSONL evidence protocol for a local coding harness.
    #[command(hide = true)]
    Evidence {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Preserve stdin/stdout JSONL framing; required to avoid prose output.
        #[arg(long)]
        jsonl: bool,
    },
    /// Look up symbols by name in the code graph.
    FindSymbol {
        name: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// All signatures in a file — the skeleton view at ~10% of Read cost.
    ListSignatures {
        file: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Structural repo map: every indexed file with its symbols. `--markdown`
    /// emits the exportable document form — the human-editable projection of
    /// the graph.
    RepoMap {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Emit the exportable markdown document.
        #[arg(long)]
        markdown: bool,
        #[arg(long)]
        json: bool,
    },
    /// Budget-fitted context for a symbol uid.
    PackContext {
        uid: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        budget: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// Blast radius of a symbol (callers upstream / callees downstream).
    Impact {
        uid_or_name: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, value_enum, default_value = "upstream")]
        direction: DirectionArg,
        #[arg(long)]
        depth: Option<u32>,
        /// Answer from every repo in .pixel/workspace.json, merged per repo.
        #[arg(long)]
        workspace: bool,
        #[arg(long)]
        no_daemon: bool,
        /// Read a fresh existing graph within 1500 ms; never start a daemon or refresh indexes.
        #[arg(long, conflicts_with = "workspace")]
        no_refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// Direct callers or callees of a symbol.
    WhoCalls {
        uid_or_name: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, value_enum, default_value = "callers")]
        role: RoleArg,
        /// Skip this many relationships for page-wise retrieval.
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Answer from every repo in .pixel/workspace.json, merged per repo.
        #[arg(long)]
        workspace: bool,
        #[arg(long)]
        no_daemon: bool,
        #[arg(long)]
        json: bool,
    },
    /// Rename a symbol like an IDE refactor — graph-resolved call/reference
    /// sites and import bindings, each verified against a fresh tree-sitter
    /// parse before its bytes are touched. Unresolved same-name sites are
    /// reported, never guessed.
    Rename {
        /// Symbol name to rename.
        name: String,
        /// New identifier.
        new_name: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Disambiguate to the declaration in this file.
        #[arg(long)]
        file: Option<String>,
        /// Disambiguate to this symbol uid (from `find-symbol`).
        #[arg(long)]
        uid: Option<String>,
        /// Compute and print the verified edit set without writing.
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
    /// Call path between two symbols.
    ///
    /// `evaluate path` asks the same question and tells an exhaustive
    /// absence from a search the depth cap cut; the output's `successor`
    /// field names that command for these two symbols.
    CallPath {
        from: String,
        to: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Evaluate a bounded predicate about the indexed call graph and
    /// return the witness that established it.
    ///
    /// Unlike `call-path`, a negative distinguishes "no path in the stored
    /// relation, traversal exhaustive" from "the traversal was cut": the
    /// first is an answer, the second is `unknown` with the budget to
    /// raise. The verdict always carries the snapshot it is about.
    Evaluate {
        #[command(subcommand)]
        cmd: EvaluateCmd,
    },
    /// Enumerate recursion cycles in the call graph with witnesses and
    /// explicit coverage.
    ///
    /// Finds strongly connected components (potential recursion cycles)
    /// over `Calls` edges only — `HasMethod` ownership is excluded because
    /// ownership is not runtime invocation. Bounded by node, edge, time,
    /// and component budgets; each reported cycle carries a concrete
    /// closed-path witness re-read from the store. The coverage report
    /// states whether the enumeration was exhaustive or which budget
    /// stopped it, so an incomplete graph is never read as proof of
    /// safety.
    Cycles {
        /// `exact` or `exact,probable`; selects the relation, not a
        /// confidence threshold.
        #[arg(long, default_value = "exact")]
        tiers: String,
        /// Maximum number of nodes to visit.
        #[arg(long)]
        max_nodes: Option<u32>,
        /// Maximum number of edges to follow.
        #[arg(long)]
        max_edges: Option<u32>,
        /// Wall-clock budget in milliseconds.
        #[arg(long)]
        time_budget_ms: Option<u64>,
        /// Maximum number of components to report.
        #[arg(long)]
        max_components: Option<u32>,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Discovered execution flows.
    ListFlows {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long)]
        json: bool,
    },
    /// Functional-area clusters.
    ListAreas {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long)]
        json: bool,
    },
    /// Symbols/flows affected by working-tree changes.
    WhatChanged {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Compare the working tree against this ref. Without it the base is
        /// the index (`base: "index"`): staged edits are not part of the diff.
        #[arg(long)]
        base: Option<String>,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Also map affected symbols to the test files that exercise them.
        #[arg(long)]
        tests: bool,
        #[arg(long)]
        json: bool,
    },
    /// Force (re)build of the code graph db.
    RebuildGraph {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Compare this graph's Rust edges with pixel-mvp's index, or (`--lang ruby`) count what merging its Ruby answer site by site would change (both built in a scratch directory).
    GraphShadow {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// `rust`: pixel-mvp's edges against this graph's; `ruby`: what
        /// merging them site by site does to this graph's.
        #[arg(long, value_enum, default_value = "rust")]
        lang: graph_shadow_cmd::Lang,
        /// How many edges each graph alone holds to show as examples.
        #[arg(long, default_value_t = 5)]
        examples: usize,
        #[arg(long)]
        json: bool,
    },
    /// Manage the multi-repo workspace (.pixel/workspace.json) that
    /// `impact --workspace` and `who-calls --workspace` fan out across.
    Workspace {
        #[command(subcommand)]
        cmd: workspace_cmd::WorkspaceCmd,
    },
    /// Freeze this repo's index into a single shareable `.pxpack` bundle —
    /// the file CI builds once and teammates install instead of re-indexing.
    IndexPack {
        /// Output file (e.g. index.pxpack).
        #[arg(long)]
        out: PathBuf,
        /// Also pack history.db (the on-demand facts index).
        #[arg(long)]
        include_history: bool,
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Install a packed index into this repo's `.pixel/` — from a path or
    /// an https:// URL.
    IndexUnpack {
        /// Pack file path or URL.
        source: String,
        /// Replace the index while a daemon is running.
        #[arg(long)]
        force: bool,
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Manage version-pinned reference corpora (.pixel/reference.json) for
    /// multi-repo analysis on top of workspaces and index packs.
    Reference {
        #[command(subcommand)]
        cmd: reference_cmd::ReferenceCmd,
    },
    /// Index + graph freshness status.
    Status {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
        /// Compact one-line summary for shell prompts / statuslines.
        #[arg(long)]
        statusline: bool,
    },
    /// Per-language coverage: files the index policy sees on disk vs files
    /// the graph actually indexed, with symbol counts per language.
    Coverage {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// What an agent reads to learn what the largest files contain: each
    /// whole file against its `list-signatures` outline, in tokens, with
    /// per-language coverage. Local and read-only.
    Audit {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// How many of the largest indexed source files to measure.
        #[arg(long, default_value_t = audit_cmd::AUDIT_DEFAULT_TOP, value_parser = clap::value_parser!(u32).range(1..))]
        top: u32,
        #[arg(long)]
        json: bool,
    },
    /// Make a repository ready for agent work: index, graph, and warm daemon.
    PrepareRepo {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Build indexes only; do not start or use the background daemon.
        #[arg(long)]
        no_daemon: bool,
        /// Rebuild the code graph from scratch even when the stored one
        /// still matches the tree (by default it is kept, or updated in
        /// place when few files changed).
        #[arg(long)]
        rebuild_graph: bool,
        #[arg(long)]
        json: bool,
    },
    /// Manage the per-root background daemon.
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
    /// Decision over a bounded label set through a remote LLM: one
    /// OpenAI-compatible chat completion maps the state, shared framing,
    /// labels and criteria onto a probability distribution — the shape a
    /// Jev-class decision model returns. Non-deterministic and
    /// network-bound; `--remote-preset` picks the provider. `--jsonl`
    /// serves one decision per stdin line.
    Classify(classify::ClassifyOptions),
    /// Offline go/no-go evaluation of the verified-history retrieval tier
    /// against frozen baselines. Reports per-label precision/recall, macro
    /// scores, confusion matrix, coverage-vs-error curves with Wilson CIs,
    /// and system metrics. Exits 0 on go, 1 on no-go.
    ClassifyEval {
        #[arg(long)]
        json: bool,
        /// Assumed model error rate (0-1) for the modeled fallback path. The
        /// offline evaluation never calls a model, so this assumption is
        /// disclosed in the report rather than presented as measured.
        #[arg(long, default_value_t = 0.20)]
        model_error_rate: f64,
        /// Assumed model latency in milliseconds for the modeled fallback path.
        #[arg(long, default_value_t = 1500.0)]
        model_latency_ms: f64,
    },
    /// Manage the verified-history store: list, add, remove, correct, or
    /// clear stored examples. The store is a bounded project-local file
    /// under `.pixel/classify-history.jsonl`.
    ClassifyHistory {
        #[command(subcommand)]
        cmd: ClassifyHistoryCmd,
    },
    /// Deterministic pre-review: `what-changed` plus the mechanical rules
    /// (credential-shaped added lines, changed symbols whose callers were
    /// not themselves changed), each finding carrying the witness that
    /// established it. Feed the output to a real review as the narrowed
    /// context it starts from.
    ReviewGate {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Base ref to diff against (default: the uncommitted diff, or the
        /// branch's merge-base with the remote default when the tree is clean).
        #[arg(long)]
        base: Option<String>,
        /// Lowest finding level that makes the command exit 1 — the pre-push
        /// gate sets it, a plain review leaves it off.
        #[arg(long, value_enum)]
        fail_on: Option<ReviewFailOn>,
        #[arg(long)]
        json: bool,
    },
    // -----------------------------------------------------------------
    // M3/M4 — engines (resolve, history, lifecycle, excavate, reconcile)
    // -----------------------------------------------------------------
    /// Engine 1: resolve a phrase to code via the concept index.
    FindCode {
        phrase: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    // -----------------------------------------------------------------
    // M5/M6 — setup / doctor / hook
    // -----------------------------------------------------------------
    /// Set pixel up for your agents, on this machine or in one repository
    /// (`--repo`). `--remove` takes everything back out.
    ///
    /// In a terminal this is a guided setup: it detects the agent CLIs, asks
    /// which of them to write for and which features to switch on, lists every
    /// skill file, hook, setting and note it would write, and writes nothing
    /// until you confirm. Cancelling writes nothing.
    ///
    /// Without a terminal on stdin and stdout, in CI, or with `--yes`,
    /// `--json` or `--print`, it does not ask: it deploys the agent
    /// integrations while preserving native retrieval (the `pixel` skill, task
    /// lifecycle hooks, the Pi package when Pi is configured) and reports what
    /// changed.
    ///
    /// ## Examples
    ///
    /// Set up this machine (guided in a terminal, unattended otherwise):
    ///
    /// ```text
    /// pixel setup
    /// ```
    ///
    /// Set up one repository (its `.pixel/config.yaml`):
    ///
    /// ```text
    /// pixel setup --repo .
    /// ```
    ///
    /// Never ask, whatever the terminal:
    ///
    /// ```text
    /// pixel setup --yes
    /// ```
    ///
    /// Print the skill the default answers deploy, changing nothing:
    ///
    /// ```text
    /// pixel setup --print
    /// ```
    ///
    /// Take everything back out:
    ///
    /// ```text
    /// pixel setup --remove
    /// ```
    Setup {
        #[arg(long)]
        json: bool,
        /// Set up this repository instead of the machine, skipping every
        /// global step: it writes the repository's `.pixel/config.yaml`.
        /// Pixel keeps no repo-local hooks, so nothing else is written under
        /// the repository.
        #[arg(long, value_name = "PATH", conflicts_with = "remove")]
        repo: Option<PathBuf>,
        /// Do not ask: deploy the integrations as an unattended run does,
        /// even in a terminal.
        #[arg(long, conflicts_with_all = ["remove", "print"])]
        yes: bool,
        /// Print the skill the default answers deploy, without prompting or
        /// writing anything.
        #[arg(long, conflicts_with = "remove")]
        print: bool,
        /// Remove everything `pixel setup` wrote: the `pixel` skill, hook
        /// entries from the settings files, the Pi package, the brief probes,
        /// and the pixel binary itself. Idempotent: safe to re-run.
        #[arg(long)]
        remove: bool,
        /// With `--remove`: preview what would be removed without making any
        /// changes.
        #[arg(long, requires = "remove")]
        dry_run: bool,
        /// With `--remove`: path to the pixel binary to remove (default: the
        /// running binary, left in place when Homebrew or mise installed it).
        #[arg(long, requires = "remove")]
        binary_path: Option<PathBuf>,
        // TEMPORARY (dev-flags) start: three development-only args, removed
        // before the release. The removal recipe is in `setup_cmd.rs`; the
        // three args below and the `dev:` field at the construction site are
        // the whole command-surface part of it.
        /// Answer the agent question with 1-based indices into the list
        /// `pixel setup` prints, comma-separated, and run the guided setup
        /// without a terminal or a confirmation. Needs
        /// `--selected-features`. Temporary.
        #[arg(
            long,
            value_name = "LIST",
            hide = true,
            requires = "selected_features",
            conflicts_with_all = ["remove", "yes", "print"]
        )]
        selected_agents: Option<String>,
        /// Answer the feature question with 1-based indices into the list
        /// `pixel setup` prints, comma-separated. Needs `--selected-agents`.
        /// Temporary.
        #[arg(long, value_name = "LIST", hide = true, requires = "selected_agents")]
        selected_features: Option<String>,
        /// Redirect every write under `tests/setup/` in this repository
        /// instead of a real home or repository, and leave the real global
        /// configuration and hook files alone. Temporary.
        #[arg(long, hide = true, conflicts_with_all = ["remove", "yes", "print"])]
        dummy_apply: bool,
        // TEMPORARY (dev-flags) end.
    },
    /// Install a plugin from `github:<owner>/<repo>[@<ref>]`, a git URL
    /// (https://, ssh://, file://, git@host:path) or a local directory
    /// holding `pixel-plugin.toml`. `pixel uninstall <name>` removes it.
    Install {
        /// The plugin source. `npm:<package>` is not supported yet.
        source: String,
        /// The plugin to install, when the source holds several.
        #[arg(long)]
        name: Option<String>,
    },
    /// Delete an installed plugin and forget its source and network opt-in.
    Uninstall {
        /// The plugin's name, as `pixel plugin list` shows it.
        name: String,
    },
    /// Check that a release tag is consistent with the tree before anything
    /// is built or published: crates/pixel/Cargo.toml carries the version,
    /// Cargo.lock is fresh for every workspace member, CHANGELOG.md has the
    /// `## [x.y.z]` heading and an empty Unreleased section. Exit 1 on any
    /// failed check.
    CheckRelease {
        /// The version or tag: `1.2.3`, `v1.2.3` or `refs/tags/v1.2.3`.
        version: String,
        /// Repository root (default: current directory).
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Emit the report as one JSON document.
        #[arg(long)]
        json: bool,
    },
    /// Rebuild the binary, stop the daemon, copy the new binary to the
    /// install path, and optionally restart the daemon. Solves the
    /// "Text file busy" error when the daemon holds the binary open.
    SelfUpdate {
        /// Cargo build command to run (default: `cargo build --release -p pixel-cli`).
        /// The built binary is read from `target/<profile>/pixel`, with the
        /// profile taken from this command's `--profile`/`--release` flags.
        #[arg(long, default_value = "cargo build --release -p pixel-cli")]
        build: String,
        /// Install path. Default: the binary running this command (unless
        /// it lives in a cargo `target/` dir), else the first `pixel` on
        /// PATH (shim directories skipped), else ~/.local/bin/pixel. A
        /// default that lands in a mise install dir or a Homebrew Cellar is
        /// refused; passing this flag writes there anyway.
        #[arg(long)]
        install_path: Option<PathBuf>,
        /// Install to ~/.local/bin/pixel-dev instead: a side build to call
        /// as `pixel-dev`, which never shadows or replaces `pixel`.
        #[arg(long, conflicts_with = "install_path")]
        dev: bool,
        /// Restart the daemon after upgrade.
        #[arg(long)]
        restart_daemon: bool,
        /// Repo path for daemon restart.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Resolve and print the install path, then exit without building
        /// or installing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Health check: install state, daemon, index/graph/facts freshness.
    ///
    /// Exits 0 when no check reaches `--fail-on`, 1 when one does, 2 when the
    /// checks could not run. Every yellow or red check names the command that
    /// repairs it (`fix`); `--fix` runs those commands.
    Doctor {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
        /// Shell asked whether `pixel` is on its PATH (default: the account's
        /// login shell, then $SHELL).
        #[arg(long)]
        shell: Option<String>,
        /// Run only this check id, or every check of a group as `<group>.*`
        /// (repeatable); `--list` names them.
        #[arg(long, value_name = "ID")]
        only: Vec<String>,
        /// Leave out this check id, or a group as `<group>.*` (repeatable):
        /// a `pixel-dev` build passes `--skip 'install.*'` to leave the home
        /// install to the managed `pixel`.
        #[arg(long, value_name = "ID")]
        skip: Vec<String>,
        /// Lowest status that makes the command exit 1.
        #[arg(long, value_enum, default_value = "red")]
        fail_on: FailOn,
        /// Print every check id with its repair command, then exit.
        #[arg(long)]
        list: bool,
        /// Run each distinct repair command the flagged checks name, once
        /// and in catalogue order, then re-run the checks: the report and the
        /// exit code are the ones after the repairs. A command only one
        /// outcome can name (a file to remove) is left to you.
        #[arg(long, conflicts_with = "list")]
        fix: bool,
    },
    /// Host hook entry points (task-event) registered by `pixel setup`.
    RunHook {
        #[command(subcommand)]
        cmd: HookCmd,
    },
    /// Show effective settings and paths, or edit persistent YAML configuration.
    Config {
        #[command(subcommand)]
        cmd: Option<ConfigCmd>,
    },
    /// Completion contracts, verification, task trajectories and Claude packets.
    TaskState {
        #[command(subcommand)]
        cmd: TaskCmd,
    },
    /// Deterministic todo list generation from code analysis.
    Plan {
        /// Natural-language prompt (optional if --query is used).
        prompt: Option<String>,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Explicit query: dead-interactive | dead-code | hotspots | recent-changes | by-concept.
        #[arg(long)]
        query: Option<String>,
        /// Tag filter for dead-interactive (e.g. button, a, Link).
        #[arg(long)]
        tag: Option<String>,
        /// Limit for hotspots/recent-changes.
        #[arg(long)]
        limit: Option<usize>,
        /// Output format: markdown | json | compact.
        #[arg(long, default_value = "markdown")]
        format: String,
        /// Omit the trailing verification todo.
        #[arg(long)]
        no_verify: bool,
        /// Omit the verification-gate block (auth session, env keys, real data).
        #[arg(long)]
        no_gates: bool,
        /// Cap the number of findings returned.
        #[arg(long)]
        max_todos: Option<usize>,
        /// Print the tracked checklist (.pixel/plan.json) without planning.
        #[arg(long, conflicts_with_all = ["prompt", "query", "tag", "limit", "format", "no_verify", "no_gates", "max_todos"])]
        status: bool,
        /// Mark tracked item N done (numbering from --status). Repeatable.
        #[arg(long, value_name = "N", conflicts_with_all = ["prompt", "query", "tag", "limit", "format", "no_verify", "no_gates", "max_todos"])]
        done: Vec<usize>,
        /// Mark tracked item N not done. Repeatable.
        #[arg(long, value_name = "N", conflicts_with_all = ["prompt", "query", "tag", "limit", "format", "no_verify", "no_gates", "max_todos"])]
        undone: Vec<usize>,
        /// Drop findings the latest plan no longer reports.
        #[arg(long, conflicts_with_all = ["prompt", "query", "tag", "limit", "format", "no_verify", "no_gates", "max_todos"])]
        prune: bool,
        #[arg(long)]
        json: bool,
    },
    /// List, enable and trust plugins: external commands that `pixel <name>
    /// [args…]` runs when the core has no such command. `pixel install
    /// <source>` adds one and `pixel uninstall <name>` removes it. A repo
    /// plugin (`<repo>/.pixel/plugins/<name>/`) runs only after `pixel plugin
    /// trust <name>`; your own live in `~/.pixel/plugins/`; an executable
    /// `pixel-<name>` on PATH is a plugin too.
    Plugin {
        #[command(subcommand)]
        cmd: plugin_cmd::PluginCmd,
    },
}

#[derive(Subcommand)]
enum EvaluateCmd {
    /// Does a path exist from `--from` to `--to` in the indexed call graph?
    ///
    /// `--tiers` selects which stored edges form the relation, never a
    /// confidence threshold: nothing widens automatically when the narrow
    /// relation finds nothing.
    Path {
        /// Source symbol: a uid (`path#qualified#kind`) or a name that
        /// resolves to exactly one symbol.
        #[arg(long)]
        from: String,
        /// Target symbol: a uid or an unambiguous name.
        #[arg(long)]
        to: String,
        /// Walk outgoing edges (`callees`) or incoming ones (`callers`).
        #[arg(long, default_value = "callees")]
        traversal: String,
        /// Edge tiers forming the relation: `exact` or `exact,probable`.
        #[arg(long, default_value = "exact")]
        tiers: String,
        /// Maximum traversal depth.
        #[arg(long)]
        max_depth: Option<u32>,
        /// Wall-clock budget for the traversal itself, in milliseconds.
        #[arg(long)]
        time_budget_ms: Option<u64>,
        /// Resolve names only under this repo-relative path prefix.
        #[arg(long = "in")]
        scope: Option<String>,
        /// Answer about the stored snapshot: skip the after-check.
        #[arg(long)]
        at_snapshot: bool,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

/// Clap value parser for a stored-history label: rejects anything outside the
/// built-in task-intent vocabulary at parse time, before the store is touched.
fn label_in_vocabulary(label: &str) -> Result<String, String> {
    classify_history_cmd::checked_label(label)?;
    Ok(label.to_string())
}

#[derive(Subcommand)]
enum ClassifyHistoryCmd {
    /// List all stored history entries.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Add a verified example to the store.
    Add {
        text: String,
        /// The label must be one of the built-in task-intent labels.
        #[arg(value_parser = label_in_vocabulary)]
        label: String,
        #[arg(long, default_value = "human-verified")]
        source: String,
    },
    /// Remove an entry by ID.
    Remove { id: String },
    /// Correct an entry's label (supersedes the old entry).
    Correct {
        id: String,
        /// The label must be one of the built-in task-intent labels.
        #[arg(value_parser = label_in_vocabulary)]
        label: String,
        #[arg(long, default_value = "human-verified")]
        source: String,
    },
    /// Clear all entries from the store.
    Clear,
}

/// `pixel search-meaning --max-files` help: an optional budget, and the
/// ceiling that applies without one, spelled from its constant.
fn max_files_help() -> String {
    format!(
        "Optional budget: most eligible files embedded; above it, a deterministic sample spread \
         across the whole tree is searched and the coverage says so. Without it, every eligible \
         file is searched, up to a safety ceiling of {} files for runaway trees (a home \
         directory)",
        pixel_recall::code_search::UNBUDGETED_FILE_CEILING
    )
}

#[derive(Subcommand)]
enum HookCmd {
    /// Durable task lifecycle and completion gate for supported agent hosts.
    TaskEvent {
        #[arg(long, value_enum)]
        provider: task_hook::TaskProvider,
        #[arg(long, value_enum)]
        event: task_hook::TaskHookEvent,
        /// Serve slice K of N of the prompt-submit brief (`--part 2/3`), for a
        /// host that caps what one hook may inject. Part 1 computes the brief
        /// and stores it; the others print their slice of it. Without the
        /// flag the one hook prints the whole brief.
        #[arg(long, value_name = "K/N")]
        part: Option<brief_parts::BriefPart>,
    },
    /// Relay finalized 🟩 metrics lines through a provider's hook contract.
    Metrics {
        /// Provider whose hook-response contract to emit under.
        #[arg(long, value_enum)]
        provider: Option<hook_metrics::Provider>,
        /// Hook event (Antigravity defers PostToolUse lines until PostInvocation).
        #[arg(long, value_enum, default_value = "post-tool-use")]
        event: hook_metrics::MetricsEvent,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Guided global setup in a terminal; also offered by pixel setup.
    Setup,
    /// Enable or disable all classify calls, including explicit --engine flags.
    Classify {
        #[arg(value_parser = ["on", "off"])]
        value: String,
    },
    /// Live 🟩 metrics footer: `pixel config metrics` reports the effective
    /// setting and its layer; `on|off` persists it to `<root>/.pixel/
    /// config.yaml` — or `~/.pixel/config.yaml` with `--global` (legacy JSON supported).
    Metrics {
        /// New value; omit to report the effective setting.
        #[arg(value_parser = ["on", "off"])]
        value: Option<String>,
        /// Write to the machine-wide configuration.
        #[arg(long)]
        global: bool,
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// API key for a `pixel classify` remote preset: `pixel config
    /// remote-key ollama <key>` stores it in the global configuration
    /// (never the repo config, never printed back). The provider env var
    /// (`OLLAMA_API_KEY`, `OPENROUTER_API_KEY`) still wins when set.
    RemoteKey {
        /// Remote provider preset the key belongs to.
        #[arg(value_enum)]
        preset: decide_remote::Preset,
        /// The key value, or `-` to read it from stdin (keeps it out of
        /// shell history and `ps`); omit to report set/unset, `--clear` to
        /// remove.
        value: Option<String>,
        /// Remove the stored key for this preset.
        #[arg(long)]
        clear: bool,
    },
    /// Remote provider `pixel classify` uses when the engine is `remote`:
    /// `pixel config remote-preset jev --model jev-latest`. Switching the
    /// stored engine to `remote` too; a `--model`/`--base` left out clears
    /// the previous provider's stale choice.
    RemotePreset {
        /// Remote provider preset (openrouter, ollama, deepseek,
        /// opencode-go, openai, jev, clef-ollama, clef-cloudflare, local).
        #[arg(value_enum)]
        preset: decide_remote::Preset,
        /// Model id the preset runs (default: the preset's own).
        #[arg(long)]
        model: Option<String>,
        /// Endpoint base override (default: the preset's own).
        #[arg(long)]
        base: Option<String>,
    },
    /// Which engine answers `pixel classify` when no `--engine` flag is
    /// given: `local` (an installed Ollaya server), `remote` (a hosted LLM
    /// behind a stored key), `jev` (TypeSafe's hosted decision model —
    /// remote engine on the `jev` preset), `clef-ollama` / `clef-cloudflare`
    /// (Cloudflare's Clef-flash decision model through Ollama, or on Workers
    /// AI behind a Cloudflare API token — remote engine on that preset), or
    /// `auto` (probe local, fall back to remote — the default). `pixel
    /// setup` sets this when you choose an engine.
    ClassifyEngine {
        /// The engine preference to store.
        #[arg(value_parser = ["local", "remote", "jev", "clef-ollama", "clef-cloudflare", "auto"])]
        value: String,
    },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Start the daemon (background unless --foreground).
    Start {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        foreground: bool,
        /// Once the daemon answers, ask it a question so its resident
        /// `meaning` vectors start building: what the hooks pass, because a
        /// daemon nobody asks keeps them cold.
        #[arg(long, hide = true, conflicts_with = "foreground")]
        warm_meaning: bool,
    },
    /// Stop a running daemon.
    Stop {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Check whether a daemon is running.
    Status {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

// ---------------------------------------------------------------------------
// daemon client / execution
// ---------------------------------------------------------------------------

/// One NDJSON round trip on an open stream.
fn roundtrip(stream: &mut UnixStream, req: &Request) -> Option<Response> {
    let mut line = serde_json::to_string(req).ok()?;
    line.push('\n');
    stream.write_all(line.as_bytes()).ok()?;
    stream.flush().ok()?;
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut buf = String::new();
    reader.read_line(&mut buf).ok()?;
    serde_json::from_str(&buf).ok()
}

/// Daemon path: only if the socket answers Ping within ~100ms.
fn try_daemon(root: &Path, req: &Request) -> Option<Response> {
    let (response, step) = route_through_daemon(root, req);
    serve_trace::record(step);
    response
}

/// Route one request through the daemon, starting one when none answers,
/// and say how it went; `None` leaves the request to this process, and the
/// step then names why.
fn route_through_daemon(root: &Path, req: &Request) -> (Option<Response>, ServeStep) {
    route_through_daemon_with(root, req, auto_start_daemon)
}

/// [`route_through_daemon`] with the auto-start as a parameter, so a test can
/// reach the start arm without spawning a process.
fn route_through_daemon_with(
    root: &Path,
    req: &Request,
    auto_start: impl FnOnce(&Path, &Request) -> Result<Response, InProcessReason>,
) -> (Option<Response>, ServeStep) {
    let clock = Instant::now();
    let probe = probe_daemon(root);
    let probe_ms = serve_trace::millis_since(clock);
    // Retiring a stale daemon is the first part of starting its replacement,
    // not a queue at the probe: `start_ms` counts it.
    let mut start_clock = Instant::now();
    let mut request_ms = None;
    let route = route_for_probe(root, probe).unwrap_or_else(|| {
        let clock = Instant::now();
        let route = send_to_daemon(root, req);
        request_ms = Some(serve_trace::millis_since(clock));
        start_clock = Instant::now();
        route
    });
    let (response, mut step) = match route {
        DaemonRoute::Served(response) => (Some(*response), ServeStep::new(ServeRoute::Daemon)),
        // A newer daemon belongs to a newer CLI still using it: leave it
        // running and serve this command in process, without starting ours.
        DaemonRoute::Declined => (None, ServeStep::in_process(InProcessReason::NewerDaemon)),
        DaemonRoute::Absent => {
            let started = auto_start(root, req);
            let start_ms = serve_trace::millis_since(start_clock);
            let (response, mut step) = match started {
                Ok(response) => (Some(response), ServeStep::new(ServeRoute::DaemonStarted)),
                Err(reason) => (None, ServeStep::in_process(reason)),
            };
            if step.reason != Some(InProcessReason::AutoStartDisabled) {
                step.start_ms = Some(start_ms);
            }
            (response, step)
        }
    };
    step.probe_ms = Some(probe_ms);
    step.request_ms = request_ms;
    (response, step)
}

fn auto_start_daemon(root: &Path, req: &Request) -> Result<Response, InProcessReason> {
    {
        // Auto-start: socket connection failed. Spawn the daemon in the
        // background and retry once. This makes the fast path transparent —
        // no need for the user to run `pixel daemon start` manually.
        // `PIXEL_DAEMON_AUTO_START=0` disables auto-start.
        if !config_cmd::feature_enabled(
            Some(root),
            config_cmd::DAEMON_AUTO_START_FEATURE,
            config_cmd::DAEMON_AUTO_START_ENV,
        ) {
            return Err(InProcessReason::AutoStartDisabled);
        }
        let abs = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        spawn_daemon(&abs).map_err(|_| InProcessReason::StartFailed)?;
        // Wait up to 5s for the socket to come up.
        for _ in 0..50 {
            if let DaemonRoute::Served(resp) = try_daemon_inner(root, req) {
                return Ok(*resp);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(InProcessReason::StartTimedOut)
    }
}

/// Run this binary with `args` as a background process of its own: no
/// standard streams, and a process group of its own so a terminal or an agent
/// supervisor that tears down the caller's group leaves it alone.
fn spawn_detached(args: &[OsString]) -> std::io::Result<()> {
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0);
    command.spawn().map(drop)
}

/// The arguments that run the daemon of `root` in the foreground.
fn daemon_args(root: &Path) -> Vec<OsString> {
    vec![
        "daemon".into(),
        "start".into(),
        root.as_os_str().to_owned(),
        "--foreground".into(),
    ]
}

/// The arguments of the start process the hooks launch: `pixel daemon start`
/// for `root` in the background, which retires a stale daemon, waits for the
/// new one and asks it the question that wakes its `meaning` vectors.
fn daemon_warm_args(root: &Path) -> Vec<OsString> {
    vec![
        "daemon".into(),
        "start".into(),
        root.as_os_str().to_owned(),
        "--warm-meaning".into(),
    ]
}

/// Start the daemon of `root` (an absolute path) in the background. The one
/// spelling of that command: the CLI's auto-start, `pixel daemon start` and
/// the hooks all come here.
fn spawn_daemon(root: &Path) -> std::io::Result<()> {
    spawn_detached(&daemon_args(root))
}

/// The question that wakes the daemon's `meaning` vectors: any non-empty one
/// does, and the answer (`unavailable` while they build) is not read.
const MEANING_WARM_QUERY: &str = "how is this repository organised";

/// Ask the daemon of `root` one `meaning` question so its resident vectors
/// start building. Best effort: a daemon that is gone, slow or without the
/// model changes nothing.
fn kick_meaning(root: &Path) {
    let _ = open_daemon_stream(root).and_then(|mut stream| {
        roundtrip(
            &mut stream,
            &Request::Meaning {
                query: MEANING_WARM_QUERY.to_string(),
                limit: Some(1),
            },
        )
    });
}

fn open_daemon_stream(root: &Path) -> Option<UnixStream> {
    let sock = daemon::socket_path(root);
    let stream = UnixStream::connect(&sock).ok()?;
    // The daemon drains its debounced watcher batch before serving a
    // connection; on a cold or loaded host that drain can outlast a short
    // probe timeout. 5s covers the drain without masking a dead daemon.
    stream
        .set_read_timeout(Some(Duration::from_millis(5000)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_millis(5000)))
        .ok()?;
    Some(stream)
}

/// What answers on a root's daemon socket, from this CLI's point of view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DaemonProbe {
    /// Nothing answers a Ping.
    Absent,
    /// A healthy daemon on this CLI's protocol.
    Current,
    /// An older protocol, no protocol at all, or a failed Ping: it must not
    /// serve this CLI, and every daemon ever shipped understands Shutdown.
    Stale,
    /// A newer protocol: a newer CLI started it and may still be using it.
    Newer,
}

fn classify_ping(ping: &Response) -> DaemonProbe {
    match ping.data().get("protocol_version").and_then(Value::as_u64) {
        Some(version) if version > PROTOCOL_VERSION => DaemonProbe::Newer,
        Some(PROTOCOL_VERSION) if ping.ok => DaemonProbe::Current,
        _ => DaemonProbe::Stale,
    }
}

fn probe_daemon(root: &Path) -> DaemonProbe {
    open_daemon_stream(root)
        .and_then(|mut stream| roundtrip(&mut stream, &Request::Ping))
        .map_or(DaemonProbe::Absent, |ping| classify_ping(&ping))
}

/// How long a stale daemon gets to release its socket after Shutdown.
const STALE_DAEMON_EXIT_CAP: Duration = Duration::from_secs(2);

/// Shut a stale daemon down and wait (up to `cap`) until it stops
/// answering, so a replacement started right after does not die on the
/// lock the old one still holds.
fn retire_stale_daemon_within(root: &Path, cap: Duration) {
    if let Some(mut stream) = open_daemon_stream(root) {
        let _ = roundtrip(&mut stream, &Request::Shutdown);
    }
    let deadline = std::time::Instant::now() + cap;
    while std::time::Instant::now() < deadline && probe_daemon(root) == DaemonProbe::Stale {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Outcome of routing one request through the daemon.
#[derive(Debug)]
enum DaemonRoute {
    Served(Box<Response>),
    /// No daemon can serve it now; starting a current one may help.
    Absent,
    /// A newer daemon is running: serve in process, start nothing.
    Declined,
}

fn try_daemon_inner(root: &Path, req: &Request) -> DaemonRoute {
    probe_route(root).unwrap_or_else(|| send_to_daemon(root, req))
}

/// The route the probe settles by itself; `None` when a current daemon
/// answered and the request can be sent to it.
fn probe_route(root: &Path) -> Option<DaemonRoute> {
    route_for_probe(root, probe_daemon(root))
}

/// [`probe_route`] for a probe already made; a stale daemon is retired here.
fn route_for_probe(root: &Path, probe: DaemonProbe) -> Option<DaemonRoute> {
    match probe {
        DaemonProbe::Current => None,
        DaemonProbe::Newer => Some(DaemonRoute::Declined),
        DaemonProbe::Stale => {
            // An old daemon must not serve stale schemas to a newer CLI.
            retire_stale_daemon_within(root, STALE_DAEMON_EXIT_CAP);
            Some(DaemonRoute::Absent)
        }
        DaemonProbe::Absent => Some(DaemonRoute::Absent),
    }
}

/// Send `req` to the daemon a probe found current.
fn send_to_daemon(root: &Path, req: &Request) -> DaemonRoute {
    // Each request gets its own connection. Besides removing a head-of-line
    // wait behind the Ping handshake, this keeps the client compatible with
    // a daemon that intentionally serves one request per connection.
    let served = open_daemon_stream(root).and_then(|mut stream| {
        // Real request may legitimately take a while (lazy graph build).
        stream
            .set_read_timeout(Some(Duration::from_secs(600)))
            .ok()?;
        stream
            .set_write_timeout(Some(Duration::from_secs(30)))
            .ok()?;
        roundtrip(&mut stream, req)
    });
    served.map_or(DaemonRoute::Absent, |response| {
        DaemonRoute::Served(Box::new(response))
    })
}

/// Prefer the daemon; fall back to an in-process Service. The given path may
/// be anywhere inside a repo — the root is discovered automatically, so
/// pointing any command at a subdirectory or file just works.
fn execute(path: &Path, req: Request, no_daemon: bool) -> Result<Value, String> {
    let root = discover_root(path)?;
    let mut step = if no_daemon {
        ServeStep::in_process(InProcessReason::NoDaemon)
    } else {
        match route_through_daemon(&root, &req) {
            (Some(resp), step) => {
                serve_trace::record(step);
                return unwrap_response(resp);
            }
            (None, step) => step,
        }
    };
    let clock = Instant::now();
    let opened = Service::open(&root);
    step.open_ms = Some(serve_trace::millis_since(clock));
    let resp = opened.map(|mut svc| {
        let clock = Instant::now();
        let resp = svc.handle(req);
        step.handle_ms = Some(serve_trace::millis_since(clock));
        resp
    });
    serve_trace::record(step);
    unwrap_response(resp.map_err(|e| e.to_string())?)
}

/// Read-only fact retrieval intentionally has no in-process or autostart
/// fallback. A missing or incompatible daemon is data for the caller, never a
/// reason to create an index as a side effect of a fact query.
#[cfg_attr(test, mutants::skip)] // the `!= DaemonProbe::Current` branch is only observable when a live daemon answers Ping; unit tests have none, and their absent-daemon path funnels identically through `unavailable`, so this preference check cannot be distinguished in-process
fn execute_targets_facts_read_only(
    path: &Path,
    task: String,
    limit: Option<usize>,
) -> Result<Value, String> {
    let root = discover_root(path)?;
    let unavailable = || {
        serde_json::to_value(TargetsFactsResult::Unavailable {
            reason: TargetsFactsUnavailableReason::DaemonUnavailable,
        })
        .map_err(|error| error.to_string())
    };
    if probe_daemon(&root) != DaemonProbe::Current {
        return unavailable();
    }
    match send_to_daemon(
        &root,
        &Request::TargetsFacts {
            task,
            limit,
            relevance_only: false,
        },
    ) {
        DaemonRoute::Served(response) => unwrap_response(*response).or_else(|_| unavailable()),
        DaemonRoute::Absent | DaemonRoute::Declined => unavailable(),
    }
}

fn unwrap_response(resp: Response) -> Result<Value, String> {
    if !resp.ok {
        return Err(resp.error_message());
    }
    // Display plumbing for the Envelope v2 honesty fields: the daemon
    // attaches `epistemics`/`snapshot`/`warnings` at the ENVELOPE level, but
    // the CLI historically prints only the result payload — which would
    // silently strip the completeness contract. Fold them into the printed
    // object (never clobbering a same-named key an op itself emitted) so
    // every cap surfaces in what the caller actually sees.
    let Response {
        snapshot,
        epistemics,
        warnings,
        result,
        ..
    } = resp;
    let mut data = result.unwrap_or(Value::Null);
    if let Some(obj) = data.as_object_mut() {
        if let Some(e) = epistemics
            && !obj.contains_key("epistemics")
        {
            obj.insert(
                "epistemics".into(),
                serde_json::to_value(e).unwrap_or(Value::Null),
            );
        }
        if let Some(s) = snapshot
            && !obj.contains_key("snapshot")
        {
            obj.insert(
                "snapshot".into(),
                serde_json::to_value(s).unwrap_or(Value::Null),
            );
        }
        if !warnings.is_empty() && !obj.contains_key("warnings") {
            obj.insert(
                "warnings".into(),
                serde_json::to_value(warnings).unwrap_or(Value::Null),
            );
        }
    }
    Ok(data)
}

fn announce_graph_build(data: &Value) {
    if let Some(info) = data.get("graph_build") {
        eprintln!("pixel: {}", graph_build_notice(info));
    }
}

/// One stderr line per graph build/update, naming which path ran: an agent
/// that edits then checks `impact` must be able to tell a 1-second
/// incremental update from a 100-second rebuild of the whole tree.
fn graph_build_notice(info: &Value) -> String {
    let ms = info.get("build_ms").and_then(Value::as_u64).unwrap_or(0);
    if info.get("incremental").and_then(Value::as_bool) == Some(true) {
        let changed = info
            .get("changed_files")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let removed = info
            .get("removed_files")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let removed = if removed > 0 {
            format!(", {removed} removed")
        } else {
            String::new()
        };
        return format!("updated graph.db for {changed} changed file(s){removed} ({ms} ms)");
    }
    match info.get("reason").and_then(Value::as_str) {
        Some("threshold") => {
            format!("rebuilt graph.db: drift above PIXEL_GRAPH_INCREMENTAL_MAX_PCT ({ms} ms)")
        }
        Some("incremental_failed") => format!(
            "rebuilt graph.db: incremental update failed ({}) ({ms} ms)",
            info.get("incremental_error")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
        ),
        Some("no_signature") | Some("unreadable") => {
            format!("rebuilt graph.db: no trusted freshness signature ({ms} ms)")
        }
        _ => format!("built graph.db on first use ({ms} ms)"),
    }
}

fn write_stdout(text: &str) -> Result<(), String> {
    write_text(
        &mut operation_metrics::Stdout(std::io::stdout().lock()),
        text,
    )
}

/// Write `text` whole; a reader that closed its end is not a failure.
fn write_text(out: &mut impl Write, text: &str) -> Result<(), String> {
    match out.write_all(text.as_bytes()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(format!("write stdout: {error}")),
    }
}

/// Whether the parsed invocation asked for JSON on stdout. Read from the
/// parsed matches, never from argv: `--json` is a flag only where a command
/// declares it. The walk reaches the deepest subcommand so a nested command
/// (`reference list --json`) answers for its own flag, and a command with no
/// `json` argument reads as `false`.
fn json_requested(matches: &ArgMatches) -> bool {
    let mut deepest = matches;
    while let Some((_, sub)) = deepest.subcommand() {
        deepest = sub;
    }
    deepest
        .try_get_one::<bool>("json")
        .ok()
        .flatten()
        .copied()
        .unwrap_or(false)
}

/// Write the `--json` failure envelope (`ok: false` + `error.code`) to stdout:
/// the parsable counterpart of the `pixel: <reason>` line on stderr. `op`
/// names the command the caller ran (the CLI's own label), not the daemon's
/// wire op. Built through the daemon's own `failure_response`, so a CLI-side
/// failure (a bad path, a refused flag combination) and a failure the daemon
/// already classified answer with the same code, from the same classifier.
///
/// Best-effort by design: a closed or full stdout must never change the exit
/// status. It bypasses `print_data`'s rendering cap on purpose — a truncated
/// failure envelope would be invalid JSON, worse than no cap at all.
fn write_failure_envelope(op: &str, message: &str) {
    let envelope: Response = failure_response(op, message);
    if let Ok(line) = serde_json::to_string(&envelope) {
        let _ = write_stdout(&format!("{line}\n"));
    }
}

/// Whether a failing command must answer with the failure envelope on stdout:
/// `--json` was asked for, the command does not own stdout, and it has not
/// answered yet. A command that owns stdout (`search-like-rg`'s passthrough,
/// the provider hook contracts, the statusline, a foreground daemon) keeps it,
/// and so does one that already wrote a document: `check-release --json`
/// reports its own failures as a document and exits 1, where a second JSON
/// line would break a reader that parses the stream as one answer.
fn failure_envelope_wanted(matches: &ArgMatches, protected: bool) -> bool {
    if protected || operation_metrics::stdout_bytes() > 0 {
        return false;
    }
    json_requested(matches)
}

/// Global stdout byte cap for `print_data` — the last-chance safety net
/// before pixel output hits the agent's context window. Individual commands
/// have their own (smaller) caps, but any command that routes through
/// `print_data` without its own cap would dump unlimited bytes to stdout.
/// At ~4 chars/token, 256KB ≈ 64K tokens ≈ $0.20 on Claude Sonnet. The
/// agent sees a `truncated` flag in the output and can page or narrow.
/// `PIXEL_OUTPUT_CAP_BYTES` overrides it (see [`stdout_byte_cap`]).
const STDOUT_BYTE_CAP: usize = 256 * 1024;

/// Effective stdout cap: `PIXEL_OUTPUT_CAP_BYTES=<bytes>` overrides the
/// default and `0` lifts the cap entirely (the same convention as
/// `PIXEL_INDEX_BUDGET_MS=0`). Unset, empty or unparsable values keep the
/// default so a typo can never silently disable the safety net.
fn stdout_byte_cap() -> usize {
    parse_output_cap(std::env::var("PIXEL_OUTPUT_CAP_BYTES").ok().as_deref())
}

fn parse_output_cap(raw: Option<&str>) -> usize {
    match raw.map(|v| v.trim().parse::<usize>()) {
        Some(Ok(0)) => usize::MAX,
        Some(Ok(n)) => n,
        _ => STDOUT_BYTE_CAP,
    }
}

fn print_data(data: &Value, raw_json: bool) -> Result<(), String> {
    let rendered = render_data(data, raw_json, stdout_byte_cap());
    if rendered.truncated {
        // Never count evidence hidden behind the final rendering cap.
        operation_metrics::unavailable();
    } else {
        operation_metrics::observe(data);
    }
    write_stdout(&rendered.text)
}

/// Whether the interactive install banner may use color: NO_COLOR unset or
/// empty means yes. A parameter over the env read, so the rule is tested.
fn banner_color(no_color: Option<&std::ffi::OsStr>) -> bool {
    no_color.is_none_or(std::ffi::OsStr::is_empty)
}

fn should_render_install_banner(json: bool, stdout_tty: bool) -> bool {
    !json && stdout_tty
}

/// Output of [`render_data`]: the bytes for stdout plus whether the cap
/// fired (so metrics can refuse to count evidence the caller never saw).
struct Rendered {
    text: String,
    truncated: bool,
}

/// One array shortened by [`truncate_structurally`]: dotted path from the
/// document root (`snapshot.dirty`, `matches[3].lines`), elements kept, and
/// the original length.
#[derive(Debug)]
struct TruncatedArray {
    path: String,
    kept: usize,
    total: usize,
}

/// Bytes reserved for the metadata [`truncate_structurally`] splices into the
/// top-level object (`truncated`, `cap_bytes`, `truncated_arrays`).
const TRUNCATION_META_RESERVE: usize = 256;

/// Most structural-truncation rounds before giving up: each round shortens
/// the (then) largest array, so a handful of rounds covers every realistic
/// response shape without letting a pathological document spin.
const TRUNCATION_MAX_ROUNDS: usize = 8;

fn serialized_len(v: &Value) -> usize {
    serde_json::to_vec(v).map_or(0, |b| b.len())
}

/// Locate the non-empty array under `v` whose tail is worth cutting most:
/// ranked by REMOVABLE bytes, `bytes - bytes / len` (everything past a
/// mean-sized first element), not raw size. Raw size would always pick an
/// enclosing array over the long list nested inside it (the parent is never
/// smaller than its child) and drop whole sibling records instead of
/// trimming the list. Returns `(path, len, bytes)`.
fn largest_array(v: &Value, path: &str) -> Option<(String, usize, usize)> {
    let mut best: Option<(String, usize, usize)> = None;
    let removable = |c: &(String, usize, usize)| c.2 - c.2 / c.1;
    let mut consider = |cand: Option<(String, usize, usize)>| {
        if let Some(c) = cand
            && best.as_ref().is_none_or(|b| removable(&c) > removable(b))
        {
            best = Some(c);
        }
    };
    match v {
        Value::Array(items) => {
            if !items.is_empty() {
                consider(Some((path.to_string(), items.len(), serialized_len(v))));
            }
            for (i, item) in items.iter().enumerate() {
                consider(largest_array(item, &format!("{path}[{i}]")));
            }
        }
        Value::Object(map) => {
            for (k, item) in map {
                let child = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                consider(largest_array(item, &child));
            }
        }
        _ => {}
    }
    best
}

/// Walk `v` to the array at the dotted `path` produced by [`largest_array`].
fn array_at_mut<'a>(v: &'a mut Value, path: &str) -> Option<&'a mut Vec<Value>> {
    let mut cur = v;
    if !path.is_empty() {
        for seg in path.split('.') {
            let (key, indexes) = match seg.find('[') {
                Some(i) => (&seg[..i], &seg[i..]),
                None => (seg, ""),
            };
            if !key.is_empty() {
                cur = cur.get_mut(key)?;
            }
            for idx in indexes
                .trim_start_matches('[')
                .trim_end_matches(']')
                .split("][")
                .filter(|s| !s.is_empty())
            {
                cur = cur.get_mut(idx.parse::<usize>().ok()?)?;
            }
        }
    }
    cur.as_array_mut()
}

/// Shorten the largest arrays in `data` until its compact serialization fits
/// `cap`, keeping every other field and the document structure intact, then
/// stamp the top-level object with `truncated: true`, `cap_bytes` and a
/// `truncated_arrays` list naming each cut (path, kept, total).
///
/// Returns `None` when `data` is not an object or no amount of array
/// trimming gets it under the cap (for example when the bulk is one huge
/// string), in which case the caller falls back to the textual wrapper.
fn truncate_structurally(data: &Value, cap: usize) -> Option<Value> {
    if !data.is_object() {
        return None;
    }
    let mut doc = data.clone();
    let mut cuts: Vec<TruncatedArray> = Vec::new();
    let budget = cap.saturating_sub(TRUNCATION_META_RESERVE);
    for _ in 0..TRUNCATION_MAX_ROUNDS {
        let size = serialized_len(&doc);
        if size <= budget {
            break;
        }
        let (path, len, bytes) = largest_array(&doc, "")?;
        // Elements that fit: the budget left after everything outside this
        // array, divided by the mean element size — always strictly fewer
        // than `len` so every round makes progress.
        let outside = size.saturating_sub(bytes);
        let per_elem = (bytes / len).max(1);
        let keep = budget
            .saturating_sub(outside)
            .checked_div(per_elem)
            .unwrap_or(0)
            .min(len - 1);
        let arr = array_at_mut(&mut doc, &path)?;
        arr.truncate(keep);
        match cuts.iter_mut().find(|c| c.path == path) {
            Some(c) => c.kept = keep,
            None => cuts.push(TruncatedArray {
                path,
                kept: keep,
                total: len,
            }),
        }
    }
    if serialized_len(&doc) > budget {
        return None;
    }
    let meta: Vec<Value> = cuts
        .iter()
        .map(|c| json!({"path": c.path, "kept": c.kept, "total": c.total}))
        .collect();
    let obj = doc.as_object_mut()?;
    obj.insert("truncated".into(), json!(true));
    obj.insert("cap_bytes".into(), json!(cap));
    obj.insert("truncated_arrays".into(), Value::Array(meta));
    (serialized_len(&doc) <= cap).then_some(doc)
}

/// Serialize `data` for stdout under a byte cap.
///
/// Human mode (`raw_json == false`) pretty-prints and, when over the cap,
/// cuts the text on a char boundary and appends a visible truncation note.
///
/// JSON mode (`raw_json == true`) must never emit anything that is not one
/// JSON document: a caller doing `serde_json::from_slice(stdout)` cannot
/// recover from a cut-off object followed by prose. When the compact
/// serialization exceeds the cap, the document is first truncated
/// STRUCTURALLY ([`truncate_structurally`]): the largest arrays are
/// shortened, every other field survives, and the object gains
/// `truncated: true`, `cap_bytes` and `truncated_arrays`, so `jq '.index'`
/// keeps working on a capped response. Only when no array trimming can fit
/// the cap does the output fall back to the small wrapper object
/// `{truncated: true, cap_bytes, note, partial}` where `partial` is the
/// leading bytes of the original serialization as a string. The wrapper
/// itself can exceed the cap by the size of the note and JSON escaping;
/// that is bounded and preferable to invalid output.
fn render_data(data: &Value, raw_json: bool, cap: usize) -> Rendered {
    let mut output = if raw_json {
        serde_json::to_string(data).unwrap_or_default()
    } else {
        serde_json::to_string_pretty(data).unwrap_or_default()
    };
    let truncated = output.len() > cap;
    if truncated
        && raw_json
        && let Some(doc) = truncate_structurally(data, cap)
    {
        output = serde_json::to_string(&doc).unwrap_or_default();
    } else if truncated {
        let mut end = cap;
        while end > 0 && !output.is_char_boundary(end) {
            end -= 1;
        }
        let note = format!(
            "OUTPUT TRUNCATED AT {cap} BYTES (global safety cap). The full \
             response was larger — re-run with a narrower scope, --limit, or \
             --offset to page through results. Remove .pixel/calls.json if \
             the circuit breaker fires."
        );
        output.truncate(end);
        if raw_json {
            output = serde_json::to_string(&json!({
                "truncated": true,
                "cap_bytes": cap,
                "note": note,
                "partial": output,
            }))
            .unwrap_or_default();
        } else {
            output.push_str("\n\n⚠ ");
            output.push_str(&note);
        }
    }
    output.push('\n');
    Rendered {
        text: output,
        truncated,
    }
}

/// The `successor` field `call-path` adds to its output: the `evaluate path`
/// command asking the same question of the same repository, ready to run,
/// and what it adds. The rest of the output is untouched: `call-path` stays
/// compatible for two minor versions (`docs/archive/design/evaluate.md`,
/// "`call-path` migration").
fn call_path_successor(from: &str, to: &str, repo: &Path) -> Value {
    json!({
        "command": format!(
            "pixel evaluate path --from {} --to {} {}",
            search_compat::shell_quote(from),
            search_compat::shell_quote(to),
            search_compat::shell_quote(&repo.to_string_lossy()),
        ),
        "why": "tells an exhaustive absence from a traversal cut by the depth cap, \
                and returns the witness edges with their call sites",
    })
}

/// Shared graph-command epilogue: candidates protocol + build announcement.
fn finish_graph_cmd(
    data: Value,
    raw_json: bool,
    pretty: impl Fn(&Value) -> Option<String>,
) -> Result<(), String> {
    announce_graph_build(&data);
    if raw_json {
        return print_data(&data, true);
    }
    if let Some(cands) = data.get("candidates").and_then(Value::as_array) {
        operation_metrics::observe(&json!({"candidates": cands}));
        eprintln!("ambiguous name — re-run with one of these uids:");
        let mut output = String::new();
        for c in cands {
            output.push_str(&format!(
                "  {}  ({} {}:{})\n",
                c.get("uid").and_then(Value::as_str).unwrap_or("?"),
                c.get("kind").and_then(Value::as_str).unwrap_or("?"),
                c.get("path").and_then(Value::as_str).unwrap_or("?"),
                c.get("start_line").and_then(Value::as_u64).unwrap_or(0),
            ));
        }
        return write_stdout(&output);
    }
    if let Some(output) = pretty(&data) {
        operation_metrics::observe(&data);
        write_stdout(&output)
    } else {
        print_data(&data, false)
    }
}

fn symbol_line(s: &Value) -> String {
    format!(
        "{:<9} {}  {}:{}-{}  {}",
        s.get("kind").and_then(Value::as_str).unwrap_or("?"),
        s.get("name").and_then(Value::as_str).unwrap_or("?"),
        s.get("path").and_then(Value::as_str).unwrap_or("?"),
        s.get("start_line").and_then(Value::as_u64).unwrap_or(0),
        s.get("end_line").and_then(Value::as_u64).unwrap_or(0),
        s.get("uid").and_then(Value::as_str).unwrap_or("?"),
    )
}

/// Whether the worst finding in the report meets the `--fail-on` threshold.
/// Separated from the match arm so the boundary is unit-testable.
fn review_gate_blocked(data: &Value, threshold: u8) -> bool {
    let worst = data
        .get("findings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|f| f.get("severity").and_then(Value::as_str))
        .map(pixel_graph::review::severity_rank)
        .max()
        .unwrap_or(0);
    worst >= threshold
}

/// Findings list for `review-gate`: severity, anchor, rule, witness, fix —
/// the code review's problems and where they are, nothing else.
fn pretty_review_gate(d: &Value) -> Option<String> {
    let findings = d.get("findings")?.as_array()?;
    let snapshot = d.get("snapshot");
    let anchor = snapshot
        .map(|s| {
            format!(
                "{} @ {}",
                s.get("branch").and_then(Value::as_str).unwrap_or("?"),
                s.get("head")
                    .and_then(Value::as_str)
                    .map_or("?", |h| &h[..h.len().min(7)]),
            )
        })
        .unwrap_or_default();
    let mut output = String::new();
    if findings.is_empty() {
        let status = if d
            .get("caps")
            .and_then(Value::as_array)
            .is_some_and(|caps| !caps.is_empty())
        {
            "incomplete"
        } else {
            "clean"
        };
        output.push_str(&format!("{status} — 0 findings ({anchor})\n"));
    } else {
        for f in findings {
            let file = f.get("file").and_then(Value::as_str);
            let line = f.get("line").and_then(Value::as_u64);
            let at = match (file, line) {
                (Some(file), Some(line)) => format!("{file}:{line}"),
                (Some(file), None) => file.to_string(),
                _ => "repo-wide".to_string(),
            };
            let severity = pixel_graph::review::severity_label(
                f.get("severity").and_then(Value::as_str).unwrap_or("?"),
            );
            output.push_str(&format!(
                "{:<10} {}  {}\n",
                severity,
                at,
                f.get("rule").and_then(Value::as_str).unwrap_or("?"),
            ));
            let evidence = f.get("evidence").and_then(Value::as_str).unwrap_or("");
            if !evidence.is_empty() {
                output.push_str(&format!("          {evidence}\n"));
            }
            let hint = f.get("fix_hint").and_then(Value::as_str).unwrap_or("");
            if !hint.is_empty() {
                output.push_str(&format!("          fix: {hint}\n"));
            }
            output.push('\n');
        }
        output.push_str(&format!("{} finding(s) ({anchor})\n", findings.len()));
    }
    if let Some(caps) = d.get("caps").and_then(Value::as_array) {
        for cap in caps {
            output.push_str(&format!("cap: {}\n", cap.as_str().unwrap_or("?")));
        }
    }
    Some(output)
}

/// Tiered pretty rendering for `targets`.
fn pretty_targets(d: &Value) -> Option<String> {
    let targets = d.get("targets")?.as_array()?;
    let mut output = String::new();
    for (tier, title) in [
        ("P0", "P0 — primary (start here)"),
        ("P1", "P1 — likely needed"),
        ("P2", "P2 — peripheral (droppable)"),
    ] {
        let group: Vec<&Value> = targets.iter().filter(|t| t["tier"] == tier).collect();
        if group.is_empty() {
            continue;
        }
        output.push_str(title);
        output.push('\n');
        for t in group {
            output.push_str(&format!(
                "  {:<50} {:.6}\n",
                t.get("path").and_then(Value::as_str).unwrap_or("?"),
                t.get("score").and_then(Value::as_f64).unwrap_or(0.0),
            ));
            if let Some(notes) = t.get("notes").and_then(Value::as_array) {
                for n in notes {
                    output.push_str(&format!(
                        "      note [{}]: {}\n",
                        n.get("target").and_then(Value::as_str).unwrap_or("?"),
                        n.get("note").and_then(Value::as_str).unwrap_or(""),
                    ));
                }
            }
            if let Some(reasons) = t.get("reasons").and_then(Value::as_array) {
                for r in reasons {
                    output.push_str(&format!("      {}\n", r.as_str().unwrap_or("")));
                }
            }
        }
    }
    let limit = d
        .get("stats")
        .and_then(|s| s.get("limit"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    output.push_str(&format!(
        "closed list: {} files (limit {limit})\n",
        targets.len()
    ));
    if let Some(cw) = d.get("closed_world").and_then(Value::as_str) {
        output.push_str(cw);
        output.push('\n');
    }
    envelope_note(d);
    Some(output)
}

/// TTL for a scoped task inside the targets manifest — matches the guard's
/// `MANIFEST_MAX_AGE_SECS` (crates/pixel/src/guard.rs).
const TARGETS_TTL_SECS: u64 = 24 * 3600;

/// Maximum concurrent tasks in the manifest. Without a cap, an agent that
/// runs `pixel targets` repeatedly without `--clear` stacks tasks
/// indefinitely, growing the manifest file and confusing the edit guard
/// (which may use the wrong task's scoping). The cap evicts the OLDEST
/// tasks first — newest wins because the agent's current task is the one
/// it just ran.
const MAX_MANIFEST_TASKS: usize = 8;

/// Stable short id for a task string (FNV-1a 64, hex). Deliberately NOT
/// `DefaultHasher` — the id must survive across pixel builds so a re-run of
/// the same task replaces its own entry instead of appending a duplicate.
fn targets_task_id(task: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in task.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")[..12].to_string()
}

/// Merge a new task entry into an existing manifest (v2 multi-task, legacy
/// single-task, or absent/corrupt), producing the v2 shape:
/// `{version: 2, tasks: [{id, task, created_unix, targets: [...]}]}`.
/// Tasks older than the 24h TTL are dropped; a task with the same id as the
/// new one is replaced. Pure function — file I/O stays in the caller.
fn merge_targets_manifest(existing: Option<&str>, new_task: Value, now: u64) -> Value {
    let new_id = new_task
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut tasks: Vec<Value> = Vec::new();
    if let Some(text) = existing
        && let Ok(v) = serde_json::from_str::<Value>(text)
    {
        if v.get("version").and_then(Value::as_u64) == Some(2) {
            tasks = v
                .get("tasks")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
        } else if let Some(old_task) = v.get("task").and_then(Value::as_str) {
            // Legacy single-task shape — wrap it as one v2 task so a
            // concurrent agent's active scope survives this write.
            tasks = vec![serde_json::json!({
                "id": targets_task_id(old_task),
                "task": old_task,
                "created_unix": v.get("created_unix").cloned().unwrap_or(Value::Null),
                "head_oid": v.get("head_oid").cloned().unwrap_or(Value::Null),
                "limit": v.get("limit").cloned().unwrap_or(Value::Null),
                "targets": v.get("files").cloned().unwrap_or_else(|| Value::Array(vec![])),
            })];
        }
    }
    tasks.retain(|t| {
        let created = t.get("created_unix").and_then(Value::as_u64).unwrap_or(0);
        let id = t.get("id").and_then(Value::as_str).unwrap_or("");
        now.saturating_sub(created) <= TARGETS_TTL_SECS && id != new_id
    });
    tasks.push(new_task);
    // Enforce the concurrent-task cap. Tasks are sorted by created_unix
    // ascending (oldest first in the vec after the retain+push). If we're
    // over the cap, drop the oldest entries until we're back under it.
    // This prevents unbounded manifest growth from agents that run
    // `pixel targets` repeatedly without `--clear`.
    if tasks.len() > MAX_MANIFEST_TASKS {
        tasks.sort_by_key(|t| t.get("created_unix").and_then(Value::as_u64).unwrap_or(0));
        let overflow = tasks.len() - MAX_MANIFEST_TASKS;
        tasks.drain(0..overflow);
    }
    serde_json::json!({ "version": 2, "tasks": tasks })
}

/// Write the enforcement manifest atomically (tmp + rename), merging into
/// any manifest already on disk so concurrent agents' tasks coexist instead
/// of clobbering each other. Returns the number of active tasks.
fn write_targets_manifest(manifest_path: &Path, task: &str, data: &Value) -> Result<usize, String> {
    let files: Vec<Value> = data
        .get("targets")
        .and_then(Value::as_array)
        .map(|ts| {
            ts.iter()
                .map(|t| {
                    serde_json::json!({
                        "path": t.get("path").cloned().unwrap_or(Value::Null),
                        "tier": t.get("tier").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let created_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let new_task = serde_json::json!({
        "id": targets_task_id(task),
        "task": task,
        "created_unix": created_unix,
        "head_oid": data
            .get("stats")
            .and_then(|s| s.get("commit_oid"))
            .cloned()
            .unwrap_or(Value::Null),
        "limit": data
            .get("stats")
            .and_then(|s| s.get("limit"))
            .cloned()
            .unwrap_or(Value::Null),
        "targets": files,
    });
    let existing = std::fs::read_to_string(manifest_path).ok();
    let manifest = merge_targets_manifest(existing.as_deref(), new_task, created_unix);
    let active = manifest
        .get("tasks")
        .and_then(Value::as_array)
        .map_or(1, Vec::len);
    if let Some(parent) = manifest_path.parent() {
        pixel_git::sidecar::private_dir(parent)
            .map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    // A fresh temporary file renamed over the name: a link committed at
    // either name is replaced, never written through.
    pixel_git::nofollow::write_replace(
        manifest_path,
        &serde_json::to_vec_pretty(&manifest).unwrap_or_default(),
        pixel_git::nofollow::PRIVATE_MODE,
    )
    .map_err(|e| format!("publish {}: {e}", manifest_path.display()))?;
    Ok(active)
}

/// Issue #814: write `.pixel/regions.json` beside `targets.json`. The
/// manifest is single-task (each `--regions` run replaces it): it describes
/// the symbol territories of the CURRENT task's P0 set, and a harness mixing
/// two tasks' regions would lose the association. It is evidence for
/// orchestration — agent count, scheduling and merging stay with the harness.
fn write_regions_manifest(root: &Path, task: &str, data: &Value) -> Result<(), String> {
    let report = data.get("regions").cloned().unwrap_or(Value::Null);
    if report.is_null() {
        return Err("the daemon returned no regions manifest".to_string());
    }
    // A regions generation failure arrives as {"error": "..."} — reject it
    // rather than publishing a manifest with null regions/conflicts/layers.
    if let Some(err) = report.get("error").and_then(Value::as_str) {
        return Err(format!("regions generation failed: {err}"));
    }
    let created_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let manifest = serde_json::json!({
        "version": 1,
        "kind": "evidence",
        "note": "evidence for orchestration (agent count, scheduling, merging); not an action recommendation",
        "task": task,
        "created_unix": created_unix,
        "head_oid": data
            .get("stats")
            .and_then(|s| s.get("commit_oid"))
            .cloned()
            .unwrap_or(Value::Null),
        "regions": report.get("regions").cloned().unwrap_or(Value::Null),
        "conflicts": report.get("conflicts").cloned().unwrap_or(Value::Null),
        "merge_order": {
            "layers": report.get("layers").cloned().unwrap_or(Value::Null),
        },
        "shared_files": report.get("shared_files").cloned().unwrap_or(Value::Null),
        "epistemics": {
            "lower_bound": report.get("lower_bound").cloned().unwrap_or(Value::Null),
            "caps": report.get("caps").cloned().unwrap_or(Value::Null),
        },
    });
    let regions_path = root
        .join(pixel_index::index::SHARD_DIR)
        .join("regions.json");
    if let Some(parent) = regions_path.parent() {
        pixel_git::sidecar::private_dir(parent)
            .map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    // A fresh temporary file renamed over the name, as for targets.json.
    pixel_git::nofollow::write_replace(
        &regions_path,
        &serde_json::to_vec_pretty(&manifest).unwrap_or_default(),
        pixel_git::nofollow::PRIVATE_MODE,
    )
    .map_err(|e| format!("publish {}: {e}", regions_path.display()))?;
    eprintln!(
        "regions manifest active: {} — evidence for orchestration, not an action recommendation",
        regions_path.display()
    );
    Ok(())
}

/// Circuit-breaker guard for retrieval commands. Call at the top of
/// each guarded command handler. If the breaker fires, prints the
/// guidance message to stderr and returns `true` (caller should return
/// early with an error). If the call is allowed, returns `false`.
fn call_guard_check(command: &str, args: &str) -> bool {
    // Test mode: skip the circuit breaker entirely so integration tests
    // that call pixel search/resolve multiple times don't hit it.
    if std::env::var("PIXEL_TEST").is_ok() {
        return false;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match call_guard::check_and_record(command, args, &cwd) {
        call_guard::CallGuardResult::Allow => false,
        call_guard::CallGuardResult::Warn(msg) => {
            eprintln!("{msg}");
            false
        }
    }
}

fn envelope_note(data: &Value) {
    if let Some(env) = data.get("envelope")
        && env
            .get("lower_bound")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        let n = env
            .get("unresolved_same_name")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        eprintln!("note: lower bound — {n} same-name call site(s) unresolved");
    }
}

// ---------------------------------------------------------------------------
// repo-root discovery
// ---------------------------------------------------------------------------

/// Walk up from `path` (file or directory) to the nearest ancestor holding a
/// `.pixel` index or a `.git` dir/file (worktrees). Falls back to the
/// starting directory. This lets every command accept a subdirectory or file
/// where an LLM would naturally point it, instead of requiring the repo root.
pub(crate) fn discover_root(path: &Path) -> Result<PathBuf, String> {
    let abs = path
        .canonicalize()
        .map_err(|e| format!("bad path {}: {e}", path.display()))?;
    let start = if abs.is_file() {
        abs.parent().map_or_else(|| abs.clone(), Path::to_path_buf)
    } else {
        abs.clone()
    };
    // The nearest `.git` defines the repo boundary and always wins — a
    // nested `.pixel` left behind by indexing a subdirectory must never
    // shadow the real repo root. `.pixel` anchors a non-git tree only when
    // it actually holds a shard: a journal-only `.pixel` (actions/history —
    // e.g. the global `$HOME/.pixel` state dir) must never anchor, or every
    // gitless invocation below it silently re-roots to that ancestor and
    // plain-walk-indexes the entire home directory.
    let mut nearest_index: Option<PathBuf> = None;
    let mut cur = start.clone();
    loop {
        if cur.join(".git").exists() {
            return Ok(cur);
        }
        if nearest_index.is_none()
            && cur
                .join(pixel_index::index::SHARD_DIR)
                .join(pixel_index::index::SHARD_FILE)
                .is_file()
        {
            nearest_index = Some(cur.clone());
        }
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => return Ok(nearest_index.unwrap_or(start)),
        }
    }
}

// ---------------------------------------------------------------------------
// legacy index/search helpers (kept behavior)
// ---------------------------------------------------------------------------

fn make_extractor(kind: ExtractorKind, max_gram: usize) -> Box<dyn GramExtractor> {
    match kind {
        ExtractorKind::Sparse => Box::new(SparseGramExtractor::with_lengths(
            Crc32Weigher,
            pixel_index::gram::DEFAULT_MIN_GRAM,
            max_gram,
        )),
        ExtractorKind::Trigram => Box::new(TrigramExtractor),
    }
}

/// One stdout line for a `search` match: the compact object in `--json`
/// mode (with `context` when the CLI enriched the match), else the
/// `path:line:text` row — or the `--- path:line ---` block once the match
/// carries surrounding lines.
fn search_match_line(m: &Value, json: bool) -> String {
    let path = m.get("path").and_then(Value::as_str).unwrap_or("");
    let line = m.get("line").and_then(Value::as_u64).unwrap_or(0);
    let text = m.get("text").and_then(Value::as_str).unwrap_or("");
    let context = m.get("context").and_then(Value::as_str);
    if json {
        let mut entry = serde_json::json!({"path": path, "line": line, "text": text});
        if let Some(ctx) = context {
            entry["context"] = Value::String(ctx.to_string());
        }
        format!("{entry}\n")
    } else if let Some(ctx) = context {
        format!("--- {path}:{line} ---\n{ctx}\n")
    } else {
        format!("{path}:{line}:{text}\n")
    }
}

/// The NDJSON line a `--json` search page ends with: the page state no match
/// line carries, because a page cut by a cap is otherwise byte-for-byte a
/// complete answer. `epistemics`/`warnings` are the envelope honesty fields
/// [`unwrap_response`] folded into the response. Every key is always present
/// (`null` for the absent ones), so the trailer's size is bounded by its
/// values alone.
fn search_meta(data: &Value, truncated: bool, next_offset: Option<u64>) -> Value {
    json!({
        "truncated": truncated,
        "next_offset": next_offset,
        "epistemics": data.get("epistemics").cloned().unwrap_or(Value::Null),
        "warnings": data.get("warnings").cloned().unwrap_or(Value::Null),
    })
}

/// Whether one more `line` fits: what is already assembled plus the line plus
/// the room reserved for the metadata line stays within `cap`. Equality fits
/// — the cap is a ceiling, not a limit one byte under it.
fn fits_before_cap(assembled: usize, line: usize, reserve: usize, cap: usize) -> bool {
    assembled + line + reserve <= cap
}

/// What one `search` page wrote: `printed` match lines out of the daemon's
/// page, whether the page is partial (`truncated`, whether the daemon's own
/// caps or the stdout cap cut it), whether the stdout cap — not the daemon —
/// is what cut it, so the caller names the right cap on stderr, and the index
/// row the next page starts at when the stdout cap cut this one.
struct SearchPage {
    printed: usize,
    truncated: bool,
    cap_fired: bool,
    next_offset: u64,
}

/// Print a `search` page and report what reached stdout.
///
/// `--json` output is NDJSON: one match per line, then the [`search_meta`]
/// line, so no reader has to guess whether a short page is the whole answer.
/// The global stdout cap is enforced here, during assembly: `--context` text
/// is added by the CLI, after the daemon's own byte cap, so this is the last
/// place that can hold one.
///
/// `positions` holds the index row of each match: `offset + i` for a plain
/// page, sparser for a `-g`/`-t` filtered one, whose `next_offset` must count
/// the rows the filter dropped or `--offset` would replay them.
fn print_search_matches(
    data: &Value,
    matches: &[Value],
    positions: &[u64],
    json: bool,
) -> Result<SearchPage, String> {
    let cap = stdout_byte_cap();
    let offset = data.get("offset").and_then(Value::as_u64).unwrap_or(0);
    let daemon_truncated = data
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // Where the page ends in index rows: the daemon's resume point when it
    // cut the page, else just past the last match.
    let page_end = data
        .get("next_offset")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| offset.saturating_add(matches.len() as u64));
    // Reserve the metadata line before the matches: it is what tells the
    // caller the page is partial, so it must not be the part that falls off
    // the cap. Reserve the longer of the two reachable trailers (partial and
    // resumable, or complete) with the largest `next_offset` this page can
    // end on, so the trailer stays inside the cap. A `cap` smaller than the
    // trailer itself is the one case it exceeds — bounded, and preferable to
    // hiding the page state.
    let reserve = if json {
        let resumable = search_meta(data, true, Some(page_end));
        let complete = search_meta(data, false, None);
        serialized_len(&resumable).max(serialized_len(&complete))
    } else {
        0
    };
    let mut output = String::with_capacity(matches.len() * 80);
    let mut printed = 0;
    let mut cap_fired = false;
    for m in matches {
        let line = search_match_line(m, json);
        if !fits_before_cap(output.len(), line.len(), reserve, cap) {
            cap_fired = true;
            break;
        }
        output.push_str(&line);
        printed += 1;
    }
    let truncated = cap_fired || daemon_truncated;
    // The first match left out resumes the page; past the last one, the page end.
    let next_offset = positions.get(printed).copied().unwrap_or(page_end);
    if json {
        output.push_str(&format!(
            "{}\n",
            search_meta(data, truncated, truncated.then_some(next_offset))
        ));
    }
    write_stdout(&output)?;
    Ok(SearchPage {
        printed,
        truncated,
        cap_fired,
        next_offset,
    })
}

/// Read surrounding lines from the file and attach as a `context` field.
/// Eliminates the need for a follow-up Read call — the agent gets the full
/// definition in one pixel search response. A per-run file-content cache is
/// threaded through so each file is read from disk at most once even when
/// many matches land in the same file.
fn enrich_with_context(
    m: &Value,
    root: &Path,
    context: usize,
    qualify: bool,
    cache: &mut HashMap<PathBuf, Option<String>>,
) -> Value {
    let rel = m.get("path").and_then(Value::as_str).unwrap_or("");
    let line_no = m.get("line").and_then(Value::as_u64).unwrap_or(0) as usize;
    let abs = root.join(rel);
    let content = match cache.entry(abs.clone()) {
        std::collections::hash_map::Entry::Occupied(e) => e.get().clone(),
        std::collections::hash_map::Entry::Vacant(e) => {
            e.insert(std::fs::read_to_string(&abs).ok()).clone()
        }
    };
    let Some(content) = content else {
        return m.clone();
    };
    let lines: Vec<&str> = content.lines().collect();
    let start = line_no.saturating_sub(context + 1).min(lines.len());
    let end = (line_no + context).min(lines.len());
    let mut ctx_lines = Vec::with_capacity(end - start);
    for (i, l) in lines[start..end].iter().enumerate() {
        let ln = start + i + 1;
        let marker = if ln == line_no { ">>" } else { "  " };
        ctx_lines.push(format!("{marker} {ln:>5}: {l}"));
    }
    let mut enriched = m.clone();
    enriched["context"] = Value::String(ctx_lines.join("\n"));
    if qualify {
        enriched["path"] = Value::String(abs.display().to_string());
    }
    enriched
}

/// Attach inline source context to every `resolve` match, same rationale as
/// `enrich_with_context` for `search`: without this, a `resolve` response
/// gives only a location, forcing a mandatory follow-up Read on every call —
/// measured as a real cost on trivial lookups (docs/bench/agent-ab-2026-08-30
/// clean-postfix.txt, s1-locate). Spans `[start_line - MARGIN, end_line +
/// MARGIN]` (not a fixed radius around one line) so a multi-line symbol's
/// full body is included, not just its first line.
fn enrich_resolve_matches_with_context(data: &mut Value, root: &Path) {
    const MARGIN: usize = 2;
    let Some(matches) = data.get_mut("matches").and_then(Value::as_array_mut) else {
        return;
    };
    let mut cache: HashMap<PathBuf, Option<String>> = HashMap::new();
    for m in matches.iter_mut() {
        let rel = m
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if rel.is_empty() {
            continue;
        }
        let start_line = m.get("start_line").and_then(Value::as_u64).unwrap_or(0) as usize;
        let end_line = m
            .get("end_line")
            .and_then(Value::as_u64)
            .map_or(start_line, |v| v as usize)
            .max(start_line);
        if start_line == 0 {
            continue;
        }
        let abs = root.join(&rel);
        let content = match cache.entry(abs.clone()) {
            std::collections::hash_map::Entry::Occupied(e) => e.get().clone(),
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(std::fs::read_to_string(&abs).ok()).clone()
            }
        };
        let Some(content) = content else { continue };
        let lines: Vec<&str> = content.lines().collect();
        let from = start_line.saturating_sub(MARGIN + 1).min(lines.len());
        let to = (end_line + MARGIN).min(lines.len());
        let mut ctx_lines = Vec::with_capacity(to - from);
        for (i, l) in lines[from..to].iter().enumerate() {
            let ln = from + i + 1;
            let marker = if ln >= start_line && ln <= end_line {
                ">>"
            } else {
                "  "
            };
            ctx_lines.push(format!("{marker} {ln:>5}: {l}"));
        }
        m["context"] = Value::String(ctx_lines.join("\n"));
    }
}

/// The first line `find-code` prints for one match: where, what kind and
/// score, the matched text, and the handler a route concept records
/// (`→ admin/orders#create (Admin::OrdersController#create)`).
fn resolve_match_line(m: &Value) -> String {
    let path = m.get("path").and_then(Value::as_str).unwrap_or("?");
    let start_line = m.get("start_line").and_then(Value::as_u64).unwrap_or(0);
    let kind = m.get("kind").and_then(Value::as_str).unwrap_or("");
    let score = m.get("score").and_then(Value::as_f64).unwrap_or(0.0);
    let raw = m
        .get("raw")
        .or_else(|| m.get("norm"))
        .and_then(Value::as_str)
        .unwrap_or("");
    // Other kinds keep their own bookkeeping in `detail` (`component`,
    // `key`, the route text again): only a route's handler is news.
    let handler = m
        .get("detail")
        .and_then(Value::as_str)
        .filter(|d| kind == "route" && !d.is_empty());
    match handler {
        Some(detail) => {
            format!("{path}:{start_line} ({kind}, score: {score:.2}) {raw} → {detail}\n")
        }
        None => format!("{path}:{start_line} ({kind}, score: {score:.2}) {raw}\n"),
    }
}

fn print_resolve_human(data: &Value) -> Result<(), String> {
    operation_metrics::observe(data);
    let Some(matches) = data.get("matches").and_then(Value::as_array) else {
        return print_data(data, false);
    };
    if matches.is_empty() {
        println!("No matches found.");
        return Ok(());
    }
    let mut output = String::new();
    for m in matches {
        output.push_str(&resolve_match_line(m));
        if let Some(notes) = m.get("notes").and_then(Value::as_array) {
            for n in notes {
                output.push_str(&format!(
                    "  note [{}]: {}\n",
                    n.get("target").and_then(Value::as_str).unwrap_or("?"),
                    n.get("note").and_then(Value::as_str).unwrap_or("")
                ));
            }
        }
        if let Some(ctx) = m.get("context").and_then(Value::as_str) {
            output.push_str(ctx);
            output.push('\n');
        }
        output.push('\n');
    }
    let confidence = data
        .get("confidence")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let basis = data.get("basis").and_then(Value::as_str).unwrap_or("");
    output.push_str(&format!("Confidence: {confidence} ({basis})\n"));
    write_stdout(&output)
}

/// Group user-supplied paths by their discovered repo root, mapping each to a
/// repo-relative prefix ("" = whole repo).
fn group_by_root(paths: &[PathBuf]) -> Result<Vec<(PathBuf, Vec<String>)>, String> {
    let mut groups: Vec<(PathBuf, Vec<String>)> = Vec::new();
    for p in paths {
        let abs = p
            .canonicalize()
            .map_err(|e| format!("bad path {}: {e}", p.display()))?;
        let root = discover_root(&abs)?;
        let rel = abs
            .strip_prefix(&root)
            .map(|r| r.to_string_lossy().into_owned())
            .unwrap_or_default();
        match groups.iter_mut().find(|(r, _)| *r == root) {
            Some((_, rels)) => {
                if rel.is_empty() {
                    rels.clear();
                    rels.push(String::new());
                } else if !rels.iter().any(String::is_empty) {
                    rels.push(rel);
                }
            }
            None => groups.push((root, vec![rel])),
        }
    }
    Ok(groups)
}

/// The stderr line of a search that matched nothing: where it ran and where
/// the repository starts, so a search from a subdirectory is not read as
/// "absent from the repo".
fn no_match_note(cwd: &Path, root: &Path) -> String {
    format!(
        "0 matches under {}; repo root {}",
        cwd.display(),
        root.display()
    )
}

#[allow(clippy::too_many_arguments)]
fn run_search(
    pattern: String,
    paths: Vec<PathBuf>,
    json: bool,
    stats: bool,
    limit: Option<usize>,
    offset: usize,
    no_daemon: bool,
    scope: Option<String>,
    context: usize,
    ignore_case: bool,
    globs: &[String],
    types: &[String],
    files_only: bool,
    fallback_query: Option<String>,
    logger: &pixel_actionlog::ActionLog,
) -> Result<(), String> {
    let effective_pattern = if ignore_case && !pattern.starts_with("(?i)") {
        format!("(?i){pattern}")
    } else {
        pattern
    };
    let groups = group_by_root(&paths)?;
    if fallback_query.is_some() && groups.len() != 1 {
        return Err("--fallback-query requires exactly one repository root".to_string());
    }
    let multi_root = groups.len() > 1;
    let mut seen_files = HashSet::new();
    for (root, rels) in groups {
        let whole_repo = rels.iter().any(String::is_empty);
        let req_paths = if whole_repo { None } else { Some(rels) };
        run_search_one(
            &effective_pattern,
            &root,
            req_paths,
            multi_root,
            json,
            stats,
            limit,
            offset,
            no_daemon,
            scope.clone(),
            context,
            globs,
            types,
            files_only,
            fallback_query.as_deref(),
            &mut seen_files,
            logger,
        )?;
    }
    Ok(())
}

/// Run the task-aware resolver once when an opted-in exact search finds no hits.
fn run_task_aware_search_fallback(
    root: &Path,
    phrase: &str,
    limit: Option<usize>,
    no_daemon: bool,
    json: bool,
) -> Result<bool, String> {
    call_guard_check("find-code", &format!("{phrase} {}", root.display()));
    if overview_intent::is_overview_prompt(phrase) {
        let repo_root = discover_root(root).unwrap_or_else(|_| root.to_path_buf());
        let answer = overview_intent::overview_answer(&repo_root);
        if json {
            print_data(&json!({ "matches": [], "note": answer }), true)?;
        } else {
            println!("{answer}");
        }
        return Ok(true);
    }
    let mut data = execute(
        root,
        Request::Resolve {
            phrase: phrase.to_string(),
            limit,
        },
        no_daemon,
    )?;
    if let Ok(repo_root) = discover_root(root) {
        enrich_resolve_matches_with_context(&mut data, &repo_root);
    }
    let found = data
        .get("matches")
        .and_then(Value::as_array)
        .is_some_and(|matches| !matches.is_empty());
    if json {
        print_data(&data, true)?;
    } else {
        eprintln!("exact lookup returned no matches; ran one task-aware find-code fallback");
        print_resolve_human(&data)?;
    }
    Ok(found)
}

/// Print `paths` one per line within the stdout byte cap, stopping before a
/// path that would cross it so no path is ever cut. Returns the text and how
/// many paths it holds.
fn files_only_output(paths: &[String], cap: usize) -> (String, usize) {
    let mut out = String::new();
    let mut printed = 0;
    for path in paths {
        if !fits_before_cap(out.len(), path.len() + 1, 0, cap) {
            break;
        }
        out.push_str(path);
        out.push('\n');
        printed += 1;
    }
    (out, printed)
}

/// Whether an empty exact-search page is the complete answer — first page,
/// no matches, zero counted, and not cut by the daemon — so the task-aware
/// fallback may replace it. A `truncated` empty page is partial evidence,
/// not a no-hit, and must keep its own result.
fn empty_exact_page_is_complete(offset: usize, matches: &[Value], data: &Value) -> bool {
    offset == 0
        && matches.is_empty()
        && data.get("match_count").and_then(Value::as_u64) == Some(0)
        && !data
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

#[allow(clippy::too_many_arguments)]
fn run_search_one(
    pattern: &str,
    root: &Path,
    req_paths: Option<Vec<String>>,
    qualify: bool,
    json: bool,
    stats: bool,
    limit: Option<usize>,
    offset: usize,
    no_daemon: bool,
    scope: Option<String>,
    context: usize,
    globs: &[String],
    types: &[String],
    files_only: bool,
    fallback_query: Option<&str>,
    seen_files: &mut HashSet<String>,
    _logger: &pixel_actionlog::ActionLog,
) -> Result<(), String> {
    // `-g`/`-t` travel with the request: the daemon drops the files they
    // exclude before paging, so `limit`, `offset` and `next_offset` count
    // kept matches here exactly as they do without a filter.
    let data = execute(
        root,
        Request::Search {
            pattern: pattern.to_string(),
            json,
            limit,
            offset: Some(offset),
            paths: req_paths,
            scope,
            globs: globs.to_vec(),
            types: types.to_vec(),
        },
        no_daemon,
    )?;
    let matches = data
        .get("matches")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(phrase) =
        fallback_query.filter(|_| empty_exact_page_is_complete(offset, &matches, &data))
    {
        operation_metrics::observe(&data);
        match run_task_aware_search_fallback(root, phrase, limit, no_daemon, json) {
            Ok(found) => {
                if !found && !json {
                    let cwd = std::env::current_dir().unwrap_or_else(|_| root.to_path_buf());
                    eprintln!("{}", no_match_note(&cwd, root));
                }
                return Ok(());
            }
            Err(error) => {
                eprintln!(
                    "task-aware find-code fallback unavailable; preserving the exact-search result: {error}"
                );
            }
        }
    }
    let positions: Vec<u64> = (offset as u64..).take(matches.len()).collect();
    // Enrich matches with surrounding context lines if requested.
    let mut cache: HashMap<PathBuf, Option<String>> = HashMap::new();
    let enriched: Vec<Value> = if context > 0 {
        matches
            .iter()
            .map(|m| enrich_with_context(m, root, context, qualify, &mut cache))
            .collect()
    } else if qualify {
        matches
            .iter()
            .map(|m| {
                let mut m = m.clone();
                if let Some(rel) = m.get("path").and_then(Value::as_str) {
                    let full = root.join(rel).display().to_string();
                    m["path"] = Value::String(full);
                }
                m
            })
            .collect()
    } else {
        matches.to_vec()
    };
    let page = if files_only {
        let paths = search_filter::files_with_matches(&enriched, seen_files);
        let (out, printed) = files_only_output(&paths, stdout_byte_cap());
        write_stdout(&out)?;
        let cap_fired = printed < paths.len();
        // Resume at the first match of the first path the cap held back.
        let resume = paths.get(printed).and_then(|cut| {
            enriched
                .iter()
                .position(|m| m.get("path").and_then(Value::as_str) == Some(cut.as_str()))
                .map(|i| positions[i])
        });
        SearchPage {
            printed,
            truncated: cap_fired || data.get("truncated").and_then(Value::as_bool) == Some(true),
            cap_fired,
            next_offset: resume.unwrap_or(0),
        }
    } else {
        print_search_matches(&data, &enriched, &positions, json)?
    };
    // An empty answer from a subdirectory is easy to misread as "not in the
    // repo": say where the search ran and where the repo starts. On stderr so
    // a pipe still sees an empty stdout, which the savings box counts as no
    // answer.
    if !json && page.printed == 0 && offset == 0 {
        let cwd = std::env::current_dir().unwrap_or_else(|_| root.to_path_buf());
        eprintln!("{}", no_match_note(&cwd, root));
    }
    // Warn the user when results were truncated so the default row cap
    // is never a surprise.
    let match_count = data.get("match_count").and_then(Value::as_u64).unwrap_or(0);
    let limit = data.get("limit").and_then(Value::as_u64).unwrap_or(0);
    // Whether the `⚠ results truncated` line below named the daemon's row
    // and byte caps: only then does the note further down drop them. A
    // stdout-cap cut names a different cap, so the daemon's caps stay in
    // the note there.
    let mut row_cap_warned = false;
    if page.cap_fired {
        // The `--context` text is added on this side of the daemon's byte
        // cap, so the stdout cap can be what cut this page: name that cap and
        // the offset that resumes the page instead of the daemon's row cap.
        let wrote = if files_only {
            format!("{} paths from {match_count} matches", page.printed)
        } else {
            format!("{} of {match_count} matches", page.printed)
        };
        eprintln!(
            "⚠ results truncated at the {}-byte stdout cap (PIXEL_OUTPUT_CAP_BYTES): wrote {wrote}. \
             Narrow --context/--limit, or continue with --offset {}.",
            stdout_byte_cap(),
            page.next_offset,
        );
    } else if page.truncated {
        row_cap_warned = true;
        eprintln!(
            "⚠ results truncated: returned {match_count}; more matches exist (row limit {limit}, byte cap {} bytes). \
             Continue with --offset {} or pass --limit to raise the row cap (maximum 10000).",
            data.get("byte_cap").and_then(Value::as_u64).unwrap_or(0),
            data.get("next_offset").and_then(Value::as_u64).unwrap_or(0),
        );
    }
    // Epistemics surfacing for search's line-oriented output (which prints
    // matches, not the whole response object): when the answer is a bounded
    // partial, say so on stderr with the named caps — minus the caps the
    // row-cap warning above already stated. A stdout-cap cut names a
    // different cap, so the daemon's caps stay in the note there.
    if let Some(e) = data.get("epistemics")
        && e.get("lower_bound").and_then(Value::as_bool) == Some(true)
        && let Some(basis) = e.get("basis").and_then(Value::as_str)
        && let Some(note) = bounded_result_note(
            basis,
            data.get("cap_hits")
                .and_then(Value::as_array)
                .map_or(&[][..], Vec::as_slice),
            row_cap_warned,
        )
    {
        eprintln!("note: bounded result — {note}");
    }
    if stats && let Some(s) = data.get("stats") {
        eprintln!(
            "candidates={}{} matches={} elapsed_us={}",
            s.get("candidates").and_then(Value::as_u64).unwrap_or(0),
            if s.get("scanned_all")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                " (full scan)"
            } else {
                ""
            },
            s.get("matches").and_then(Value::as_u64).unwrap_or(0),
            s.get("elapsed_us").and_then(Value::as_u64).unwrap_or(0),
        );
    }
    // One invocation record owns both duration and savings. Count distinct
    // returned evidence only; no metadata sweeps or duplicate search events.
    operation_metrics::observe(&json!({
        "matches": enriched,
        "truncated": page.truncated || offset > 0,
        "epistemics": data.get("epistemics"),
    }));
    Ok(())
}

// ---------------------------------------------------------------------------
// daemon management
// ---------------------------------------------------------------------------

/// Probe a daemon without starting one. Deliberately not `try_daemon`: that
/// one auto-starts a repository `Service` for any root it is given, which
/// turns a `status` or a `daemon start` check (the recall daemon's included)
/// into a spurious service — with `.pixel/` artifacts — on that root.
fn daemon_ping(root: &Path) -> bool {
    // Startup readiness and command routing must agree: a daemon that answers
    // Ping but cannot serve this CLI's protocol would otherwise be reported
    // ready, then make `build-index` silently use its in-process fallback.
    // A stale one is retired here too, so the start that usually follows a
    // `false` is not refused by the lock it still holds.
    match probe_daemon(root) {
        DaemonProbe::Current => true,
        DaemonProbe::Stale => {
            retire_stale_daemon_within(root, STALE_DAEMON_EXIT_CAP);
            false
        }
        DaemonProbe::Absent | DaemonProbe::Newer => false,
    }
}

fn daemon_start(path: PathBuf, foreground: bool, quiet: bool) -> Result<(), String> {
    let report = |message: &str| {
        if quiet {
            eprint!("{message}");
            Ok(())
        } else {
            write_stdout(message)
        }
    };
    if foreground {
        return daemon::run(&path).map_err(|e| e.to_string());
    }
    match probe_daemon(&path) {
        DaemonProbe::Current => {
            report(&format!(
                "daemon already running ({})\n",
                daemon::socket_path(&path).display()
            ))?;
            return Ok(());
        }
        DaemonProbe::Newer => {
            return Err(format!(
                "a newer pixel daemon serves {} (its protocol is above {PROTOCOL_VERSION}); \
                 upgrade this pixel or run `pixel daemon stop` first",
                path.display()
            ));
        }
        DaemonProbe::Stale => retire_stale_daemon_within(&path, STALE_DAEMON_EXIT_CAP),
        DaemonProbe::Absent => {}
    }
    let abs = path
        .canonicalize()
        .map_err(|e| format!("bad path {}: {e}", path.display()))?;
    spawn_daemon(&abs).map_err(|e| format!("spawn daemon: {e}"))?;
    // Wait for the socket to come up (index build can take a moment).
    for _ in 0..100 {
        if daemon_ping(&abs) {
            report(&format!(
                "daemon started ({})\n",
                daemon::socket_path(&abs).display()
            ))?;
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!(
        "daemon spawned but did not become ready ({})",
        daemon::socket_path(&abs).display()
    ))
}

fn daemon_stop(path: PathBuf) -> Result<(), String> {
    // Straight to the socket: every protocol understands Shutdown, and
    // routing it through `try_daemon` would start a daemon just to stop it.
    let stopped =
        open_daemon_stream(&path).and_then(|mut stream| roundtrip(&mut stream, &Request::Shutdown));
    match stopped {
        Some(r) if r.ok => {
            write_stdout("daemon stopped\n")?;
            Ok(())
        }
        _ => {
            write_stdout(&format!("no daemon running for {}\n", path.display()))?;
            Ok(())
        }
    }
}

/// Where `pixel upgrade` writes the new binary, and why that path won.
struct UpgradeTarget {
    path: PathBuf,
    source: &'static str,
    /// The user named this path (`--install-path`): no ownership check.
    explicit: bool,
}

/// The package manager that owns an install, and how to update it there.
#[derive(Clone, Copy)]
enum ManagedBy {
    Mise,
    Homebrew,
}

impl ManagedBy {
    /// The manager's name, as the refusal message spells it.
    fn name(self) -> &'static str {
        match self {
            ManagedBy::Mise => "mise",
            ManagedBy::Homebrew => "Homebrew",
        }
    }

    /// The command that updates an install this manager owns. `brew update`
    /// comes first: with auto-update disabled, `brew upgrade` alone would
    /// install the tap's stale formula.
    fn upgrade_command(self) -> &'static str {
        match self {
            ManagedBy::Mise => "mise upgrade pixel",
            ManagedBy::Homebrew => "brew update && brew upgrade LivioGama/tap/pixel",
        }
    }
}

/// A directory whose files a package manager installed and checksummed.
struct ManagedRoot {
    root: PathBuf,
    manager: ManagedBy,
}

/// The install trees `pixel upgrade` must not write into on its own:
/// mise's `installs/` (`~/.local/share/mise`, or `$MISE_DATA_DIR`) and the
/// Homebrew Cellars (`/opt/homebrew`, `/usr/local`, Linuxbrew's
/// `/home/linuxbrew/.linuxbrew`, or `$HOMEBREW_CELLAR`
/// that `brew shellenv` exports). Roots are canonicalized when they exist
/// so they compare against a resolved binary path (`/tmp` is
/// `/private/tmp` on macOS).
fn package_manager_roots(
    home: &Path,
    mise_data_dir: Option<&std::ffi::OsStr>,
    homebrew_cellar: Option<&std::ffi::OsStr>,
) -> Vec<ManagedRoot> {
    let mut roots = vec![ManagedRoot {
        root: home.join(".local/share/mise/installs"),
        manager: ManagedBy::Mise,
    }];
    if let Some(dir) = mise_data_dir.filter(|d| !d.is_empty()) {
        roots.push(ManagedRoot {
            root: Path::new(dir).join("installs"),
            manager: ManagedBy::Mise,
        });
    }
    // Linuxbrew's default prefix: `$HOMEBREW_CELLAR` is set only where `brew
    // shellenv` ran, and a shell without it still runs the Cellar binary.
    for cellar in [
        "/opt/homebrew/Cellar",
        "/usr/local/Cellar",
        "/home/linuxbrew/.linuxbrew/Cellar",
    ] {
        roots.push(ManagedRoot {
            root: PathBuf::from(cellar),
            manager: ManagedBy::Homebrew,
        });
    }
    if let Some(cellar) = homebrew_cellar.filter(|c| !c.is_empty()) {
        roots.push(ManagedRoot {
            root: PathBuf::from(cellar),
            manager: ManagedBy::Homebrew,
        });
    }
    for managed in &mut roots {
        if let Ok(canonical) = managed.root.canonicalize() {
            managed.root = canonical;
        }
    }
    roots
}

/// The update command `update_notice` shows for the running binary, from
/// where it resolves and the package-manager roots `self-update` refuses.
#[cfg_attr(test, mutants::skip)] // reads the process environment; `upgrade_hint` is tested
fn release_upgrade_hint() -> Option<String> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let exe = std::env::current_exe().ok()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let roots = package_manager_roots(
        &home,
        std::env::var_os("MISE_DATA_DIR").as_deref(),
        std::env::var_os("HOMEBREW_CELLAR").as_deref(),
    );
    update_notice::upgrade_hint(&exe, &roots, &home)
}

/// Why `pixel upgrade` refuses `target`, or `None` when it may write there.
///
/// Overwriting a package manager's binary is silent corruption: on
/// 2026-09-14 a bare `pixel upgrade` replaced the mise-installed 0.2.4 with
/// a dirty `target/dev-release` build while mise still listed 0.2.4 (mise
/// checks the checksum at install time only), and nothing said so. The
/// path is resolved first, so a symlink into a Cellar (`/opt/homebrew/bin/
/// pixel`) is refused like the Cellar file itself. The refusal names the
/// manager's own upgrade command, and `--install-path` is the user's
/// decision and is never refused.
fn upgrade_target_refusal(target: &UpgradeTarget, roots: &[ManagedRoot]) -> Option<String> {
    if target.explicit {
        return None;
    }
    let resolved = target
        .path
        .canonicalize()
        .unwrap_or_else(|_| target.path.clone());
    let owner = roots.iter().find(|r| resolved.starts_with(&r.root))?;
    Some(format!(
        "refusing to install over {resolved} ({source}): it lies under {root}, which {manager} \
         installed; overwriting it would leave {manager} listing a version that is no longer \
         there. Update it with `{command}` instead. Run `pixel self-update --dry-run` to see \
         where an upgrade lands, pass `--install-path {resolved}` to write there anyway, or \
         `--dev` to install a side build at ~/.local/bin/pixel-dev.",
        resolved = resolved.display(),
        source = target.source,
        root = owner.root.display(),
        manager = owner.manager.name(),
        command = owner.manager.upgrade_command(),
    ))
}

/// `pixel upgrade --dev` destination: a distinct name, so a local build
/// can be exercised as `pixel-dev` while `pixel` stays the managed one.
fn dev_install_path(home: &Path) -> PathBuf {
    home.join(".local/bin")
        .join(pixel_setup::config::PIXEL_DEV_EXECUTABLE)
}

/// The opening of the `CACHEDIR.TAG` cargo writes at the root of every
/// build directory: the standard signature, then cargo's own line, so
/// another tool's cache directory is not taken for a build.
const CARGO_CACHEDIR_TAG: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55\n\
# This file is a cache directory tag created by cargo.";

/// The lock files cargo takes in a profile directory (`target/debug/`)
/// on every build: `.cargo-lock`, and `.cargo-artifact-lock` in the
/// directory final artifacts land in since cargo split the build directory
/// from the artifact one. Unlike `CACHEDIR.TAG` they exist whoever created
/// `target/`.
const CARGO_PROFILE_LOCKS: [&str; 2] = [".cargo-lock", ".cargo-artifact-lock"];

/// True when `path` is cargo build output, never an install location: it
/// has a `target` directory component, or a directory above it is a cargo
/// build directory ([`is_cargo_build_dir`]). That is what still names a
/// build directory once `canonicalize` has resolved a `target/` symlink
/// into a cache or a `CARGO_TARGET_DIR` elsewhere: without it the running
/// test binary was taken for an install and overwritten (#513, #530).
fn is_cargo_target_path(path: &Path) -> bool {
    path.components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new("target"))
        || path.ancestors().skip(1).any(is_cargo_build_dir)
}

/// True when `exe` is a build a developer runs from a checkout rather than
/// a release install: cargo build output ([`is_cargo_target_path`]) or the
/// `pixel-dev` side build ([`pixel_setup::config::is_side_build`]). Such a
/// build carries its branch's code under the release's version, so
/// whatever a release install owns (the upgrade notice) is not its to judge.
pub(crate) fn is_developer_build(exe: &Path) -> bool {
    is_cargo_target_path(exe) || pixel_setup::config::is_side_build(exe)
}

/// `dir` holds the `CACHEDIR.TAG` cargo writes at the root of a target
/// directory it creates, or one of the [`CARGO_PROFILE_LOCKS`] of a profile
/// directory. The tag alone missed a build cache that creates `target/`
/// itself (cargo then writes no tag), so its test binary was overwritten
/// again (#530).
fn is_cargo_build_dir(dir: &Path) -> bool {
    std::fs::read(dir.join("CACHEDIR.TAG")).is_ok_and(|tag| tag.starts_with(CARGO_CACHEDIR_TAG))
        || CARGO_PROFILE_LOCKS
            .iter()
            .any(|lock| dir.join(lock).is_file())
}

/// Every `pixel` executable on `path_var`, in PATH order, symlinks
/// resolved, deduplicated. Directories named `shims` (mise, asdf) are
/// skipped: a shim is a launcher that `exec`s the managed install, so it is
/// neither a place to write a binary nor a competing copy.
fn pixel_binaries_on_path(path_var: Option<&std::ffi::OsStr>) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    let Some(path_var) = path_var else {
        return found;
    };
    for dir in std::env::split_paths(path_var) {
        if dir.file_name().and_then(|n| n.to_str()) == Some("shims") {
            continue;
        }
        let candidate = dir.join("pixel");
        if !candidate.is_file() {
            continue;
        }
        let resolved = candidate.canonicalize().unwrap_or(candidate);
        if !found.contains(&resolved) {
            found.push(resolved);
        }
    }
    found
}

/// Cargo profile directory a build command writes to, from its own flags:
/// `--profile <name>` / `--profile=<name>` wins, then `--release`
/// (`release`), else cargo's default `debug`. `pixel upgrade` used to
/// hardcode `target/release`, so a `--build` with another profile installed
/// whatever stale binary sat there. A command that does not invoke cargo
/// (a wrapper script, `/usr/bin/true` in tests) keeps the historical
/// `release`: cargo's flag semantics do not apply to it.
fn cargo_profile_dir(build: &str) -> String {
    let mut args = build.split_whitespace().peekable();
    let mut release = false;
    let mut invokes_cargo = false;
    while let Some(arg) = args.next() {
        if arg == "cargo" || arg.ends_with("/cargo") {
            invokes_cargo = true;
        } else if arg == "--profile" {
            if let Some(name) = args.next() {
                return profile_dir_name(name);
            }
        } else if let Some(name) = arg.strip_prefix("--profile=") {
            return profile_dir_name(name);
        } else if arg == "--release" || arg == "-r" {
            release = true;
        }
    }
    if release || !invokes_cargo {
        "release".into()
    } else {
        "debug".into()
    }
}

/// Cargo maps the built-in `dev`/`test` profiles to `target/debug` and
/// `bench` to `target/release`; custom profiles use their own name.
fn profile_dir_name(name: &str) -> String {
    match name {
        "dev" | "test" => "debug".into(),
        "bench" => "release".into(),
        other => other.into(),
    }
}

/// Decide where `pixel upgrade` installs.
///
/// The historical fixed `~/.local/bin/pixel` was wrong on any machine whose
/// `pixel` is managed elsewhere (a mise/asdf install dir behind a shim, a
/// Homebrew cellar, `~/.cargo/bin`): it dropped a second copy that either
/// shadowed the managed one or never reached PATH. Order:
///
/// 1. `--install-path`, verbatim.
/// 2. The binary running this command: after a shim `exec`s the managed
///    install, `current_exe` IS the managed install. Skipped when it sits
///    in a cargo `target/` dir (`target/release/pixel upgrade`).
/// 3. The first `pixel` on PATH outside a `shims` dir or a `target/` dir.
/// 4. `~/.local/bin/pixel`, the legacy default.
///
/// Steps 2 to 4 only find a path; `upgrade_target_refusal` then refuses one
/// a package manager owns.
fn resolve_upgrade_target(
    explicit: Option<PathBuf>,
    current_exe: Option<PathBuf>,
    path_var: Option<&std::ffi::OsStr>,
    home: &Path,
) -> UpgradeTarget {
    if let Some(path) = explicit {
        return UpgradeTarget {
            path,
            source: "--install-path",
            explicit: true,
        };
    }
    if let Some(exe) = current_exe {
        let exe = exe.canonicalize().unwrap_or(exe);
        if exe.is_file() && !is_cargo_target_path(&exe) {
            return UpgradeTarget {
                path: exe,
                source: "running binary",
                explicit: false,
            };
        }
    }
    if let Some(path) = pixel_binaries_on_path(path_var)
        .into_iter()
        .find(|p| !is_cargo_target_path(p))
    {
        return UpgradeTarget {
            path,
            source: "first pixel on PATH",
            explicit: false,
        };
    }
    UpgradeTarget {
        path: home.join(".local").join("bin").join("pixel"),
        source: "default",
        explicit: false,
    }
}

/// The `pixel` that a shell would run INSTEAD of `installed`, if any: the
/// first PATH hit that is a different file. This is how a stale copy in
/// `~/.local/bin` silently kept serving an old version after an upgrade
/// landed in a mise install dir that came later on PATH. A binary installed
/// under another name (`pixel-dev`) is not what `pixel` runs, so no other
/// `pixel` can shadow it.
fn upgrade_shadowed_by(installed: &Path, path_var: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    if installed.file_name() != Some(std::ffi::OsStr::new("pixel")) {
        return None;
    }
    let installed = installed.canonicalize().ok()?;
    pixel_binaries_on_path(path_var)
        .into_iter()
        .next()
        .filter(|first| *first != installed)
}

/// Upgrade control messages must never inherit the retrieval client's 600s timeout.
/// Only a missing/refused socket means absent; a stalled or malformed reply is an error.
fn upgrade_daemon_request(socket: &Path, req: &Request) -> Result<Option<Response>, String> {
    let mut stream = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None);
        }
        Err(e) => return Err(format!("upgrade daemon connection: {e}")),
    };
    stream
        .set_read_timeout(Some(Duration::from_millis(1500)))
        .and_then(|_| stream.set_write_timeout(Some(Duration::from_millis(1500))))
        .map_err(|e| format!("upgrade daemon timeout: {e}"))?;
    roundtrip(&mut stream, req).map(Some).ok_or_else(|| {
        "installed binary, but daemon control timed out or returned an invalid response".into()
    })
}

/// A daemon that accepted shutdown is stopped only after it unlinks its own
/// repository socket. Pinging during that teardown can be accepted by the
/// listener after the serving loop has exited, then time out without a reply.
fn upgrade_daemon_socket_stopped(socket: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(socket) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(format!(
            "installed binary, but could not inspect daemon socket {}: {error}",
            socket.display()
        )),
    }
}

fn daemon_status(path: PathBuf) -> Result<(), String> {
    if daemon_ping(&path) {
        write_stdout(&format!(
            "daemon running ({})\n",
            daemon::socket_path(&path).display()
        ))?;
    } else {
        write_stdout(&format!("daemon not running for {}\n", path.display()))?;
    }
    Ok(())
}

/// Replace `snapshot.dirty` (the full dirty path list) with `dirty_count`.
///
/// Freshness/readiness answers (`status`, `ready`) only need to say HOW
/// dirty the tree is: the list itself belongs to `inspect`/`review`, and
/// carrying it here let one untracked `vendor/bundle` push a 200-byte
/// answer past the global output cap. The daemon now ships the compact
/// form itself for every op but `inspect`/`review`
/// (`SnapshotInfo::compact`); this stays as the client-side guard when
/// talking to an older daemon that still sends the list.
fn compact_snapshot(data: &mut Value) {
    if let Some(snap) = data.get_mut("snapshot").and_then(Value::as_object_mut)
        && let Some(dirty) = snap.remove("dirty")
    {
        let n = dirty.as_array().map_or(0, Vec::len);
        snap.insert("dirty_count".into(), json!(n));
    }
}

/// Prepare every local GitPixel prerequisite in one deterministic operation.
///
/// The JSON answer is deliberately compact — index counters, graph counters,
/// daemon state, `dirty_count` — and never embeds the status snapshot: the
/// dirty file list is irrelevant to "is the index ready" and is what blew
/// past the output cap on repos with untracked vendor trees.
fn ready(path: PathBuf, no_daemon: bool, rebuild_graph: bool, json: bool) -> Result<(), String> {
    let started = Instant::now();
    let root = discover_root(&path)?;
    let mut status = execute(&root, Request::Status {}, no_daemon)?;
    let if_stale = !rebuild_graph;
    let mut graph = execute(&root, Request::Graph { if_stale }, no_daemon)?;
    if !no_daemon {
        daemon_start(root.clone(), false, json)?;
    }
    let timings = ready_timings(&mut status, &mut graph, started.elapsed());
    let snapshot = status.get("snapshot");
    let dirty_count = snapshot
        .and_then(|s| s.get("dirty_count"))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            // Older daemon: the status snapshot still carries the list.
            snapshot
                .and_then(|s| s.get("dirty"))
                .and_then(Value::as_array)
                .map_or(0, Vec::len) as u64
        });
    let data = serde_json::json!({
        "root": root,
        "index": status.get("index").cloned().unwrap_or(Value::Null),
        "graph": graph,
        "daemon": if no_daemon { "skipped" } else { "running" },
        "dirty_count": dirty_count,
        "timings": timings,
    });
    if json {
        print_data(&data, true)
    } else {
        write_stdout(&format!(
            "ready: {}\nindex: ready\ngraph: ready\ndaemon: {}\n{}\n",
            data.get("root").and_then(Value::as_str).unwrap_or("?"),
            data.get("daemon").and_then(Value::as_str).unwrap_or("?"),
            timings_line(&data["timings"])
        ))
    }
}

/// The `timings` block of `prepare-repo`: the index open's layers (moved out
/// of `index.open`), how the graph was obtained and its phases (moved out of
/// `graph.build` and `graph.phases`), beside the command's wall time, so one
/// `jq .timings` says where the time went. An older daemon that sends none
/// of them leaves `null` in their place.
fn ready_timings(status: &mut Value, graph: &mut Value, total: Duration) -> Value {
    let index = status
        .get_mut("index")
        .and_then(Value::as_object_mut)
        .and_then(|index| index.remove("open"))
        .unwrap_or(Value::Null);
    let mut take = |key: &str| {
        graph
            .as_object_mut()
            .and_then(|graph| graph.remove(key))
            .unwrap_or(Value::Null)
    };
    let (build, phases) = (take("build"), take("phases"));
    serde_json::json!({
        "total_ms": pixel_index::indexset::millis(total),
        "index": index,
        "graph": {
            "elapsed_ms": graph.get("elapsed_ms").cloned().unwrap_or(Value::Null),
            "build": build,
            "phases": phases,
        },
    })
}

/// One human line out of [`ready_timings`]: the total, how the index base
/// was obtained, how the graph was (kept, updated, rebuilt), and its slowest
/// phase — the one to look at.
fn timings_line(timings: &Value) -> String {
    let ms = |v: &Value| {
        v.as_u64()
            .map_or_else(|| "?".to_string(), |n| format!("{n} ms"))
    };
    let index = &timings["index"];
    let index_ms = ["base_ms", "delta_ms", "overlay_ms"]
        .iter()
        .map(|key| index[*key].as_u64())
        .sum::<Option<u64>>();
    let slowest = timings["graph"]["phases"]
        .as_object()
        .and_then(|phases| {
            phases
                .iter()
                .filter_map(|(name, v)| Some((name, v.as_u64()?)))
                .max_by_key(|&(_, n)| n)
        })
        .map(|(name, n)| format!(", slowest {} {n} ms", name.trim_end_matches("_ms")))
        .unwrap_or_default();
    format!(
        "timings: total {}, index {} (base {}), graph {} ({}){slowest}",
        ms(&timings["total_ms"]),
        index_ms.map_or_else(|| "?".to_string(), |n| format!("{n} ms")),
        index["base"].as_str().unwrap_or("?"),
        ms(&timings["graph"]["elapsed_ms"]),
        timings["graph"]["build"]["mode"].as_str().unwrap_or("?"),
    )
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

/// The `facts` block of a `status` answer when history is built: the daemon
/// reports an absent history db as `{"present": false}`, which the
/// renderers must read as "no facts", like a missing block.
fn present_facts(data: &Value) -> Option<Value> {
    data.get("facts")
        .filter(|f| f.get("present").and_then(Value::as_bool) != Some(false))
        .cloned()
}

/// `bytes` as mebibytes with one decimal, for the human `status` output.
fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / 1_048_576.0)
}

/// Parse argv, dispatch, and record the invocation to the per-repo action
/// log (`<root>/.pixel/actions.jsonl`) so a session can be self-assessed
/// later. Logging is best-effort and asynchronous — it can never fail or
/// slow down the command it observes: `discover_root` failures fall back to
/// a no-op logger, and `ActionLog::finish` bounds the writer's shutdown
/// window instead of blocking on a slow disk.
fn operation_path(matches: &clap::ArgMatches) -> Option<PathBuf> {
    if let Some((_, nested)) = matches.subcommand()
        && let Some(path) = operation_path(nested)
    {
        return Some(path);
    }
    for key in ["path", "repo"] {
        if let Ok(Some(path)) = matches.try_get_one::<PathBuf>(key) {
            return Some(path.clone());
        }
    }
    matches
        .try_get_many::<PathBuf>("paths")
        .ok()
        .flatten()
        .and_then(|mut paths| paths.next().cloned())
}

/// The arguments as the action log records them. An API key is a secret:
/// the log is plain text under `.pixel/`, so every positional after
/// `config remote-key <preset>` is masked.
///
/// The auth flow's URL variable is masked for the same reason: its
/// `code`/`state` query is a bearer token for one login, and any command
/// given `--var auth_url=…` lands here, and so does a plugin run: the core
/// logs the arguments of `pixel flow …` through this function before any
/// plugin sees a log of its own. The name stays readable, the value never
/// does.
fn logged_args(args: &[String]) -> String {
    let secret_from = args
        .windows(2)
        .position(|pair| pair[0] == "config" && pair[1] == "remote-key")
        .map(|at| at + 3);
    // `install https://user:token@host/…` carries the credential of a
    // private plugin repository in its URL. Only the command word counts:
    // `search-content install` is a query.
    let with_program: Vec<String> = std::iter::once(String::new())
        .chain(args.iter().cloned())
        .collect();
    let plugin_install_from = pixel_plugin::command_word(&with_program)
        .filter(|(_, word)| *word == "install")
        .map(|(at, _)| at);
    let url_name = "auth_url";
    let url_var = format!("{url_name}=");
    // Clap takes `--var auth_url=…` and `--var=auth_url=…` as the same
    // option, so the one-token spelling carries the same one-time
    // `code`/`state` payload and is masked the same way. Matching only the
    // two-argument form left the other in the log in clear text.
    let url_flag = format!("--var={url_var}");
    args.iter()
        .enumerate()
        .map(|(i, arg)| {
            if arg.starts_with(&url_flag) {
                format!("--var={url_name}=<redacted>")
            } else if arg.starts_with(&url_var) {
                format!("{url_name}=<redacted>")
            } else if secret_from.is_some_and(|from| i >= from) && !arg.starts_with("--") {
                "<redacted>".to_string()
            } else if plugin_install_from.is_some_and(|from| i >= from) {
                pixel_plugin::redact_source(arg)
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The command labels the update close may relaunch. The relaunch executes
/// the command again, so only the read-only surface — retrieval, graph
/// queries, git and pixel reporting — is ever offered; a mutating command
/// (`build-index`, `install`, `scope-task --clear`, ...) finishes and stays
/// finished, and its notice stays informative without a prompt. Labels are
/// clap's kebab-case subcommand names, the same strings
/// `matches.subcommand_name()` reports. A plugin's commands are not here and
/// need not be: a plugin run never reaches the update close (the dispatch
/// returns the plugin's own exit code first), so what such a command may do
/// to an agent's task gate is `pixel_plugin::read_only_invocation`, which
/// `task_hook` consults.
const READ_ONLY_COMMANDS: &[&str] = &[
    "search-content",
    "search-like-rg",
    "search-meaning",
    "find-code",
    "find-symbol",
    "list-signatures",
    "call-path",
    "who-calls",
    "impact",
    "repo-map",
    "pack-context",
    "execution-brief",
    "evidence",
    "list-flows",
    "list-areas",
    "what-changed",
    "status",
    "coverage",
    "audit",
];

/// Whether the parsed invocation may be executed again by the relaunch: the
/// top-level label must be read-only. Every retained command in
/// [`READ_ONLY_COMMANDS`] is a leaf (no nested mode that mutates), so the
/// label alone decides.
fn read_only_invocation(matches: &ArgMatches) -> bool {
    let Some(label) = matches.subcommand_name() else {
        return false;
    };
    READ_ONLY_COMMANDS.contains(&label)
}

/// The bounded-result note for search's line output, with the caps the
/// `⚠ results truncated` line already stated removed: the daemon restates
/// the row and byte caps inside `basis`, and the same bound on two stderr
/// lines reads as two problems. The daemon names each cap that fired by
/// kind in `cap_hits`, so the row limit and the byte cap go whatever numbers
/// they carry (a filtered search pages the daemon with a limit of its own).
/// Every other named cap (credential hiding, the ranked-pool cap) carries
/// information the warning does not and stays. `None` when nothing unique
/// remains.
fn bounded_result_note(basis: &str, cap_hits: &[Value], truncation_warned: bool) -> Option<String> {
    if !truncation_warned {
        return Some(basis.to_string());
    }
    let tier = basis.split_once("; caps: ").map_or(basis, |(tier, _)| tier);
    let rest: Vec<&str> = cap_hits
        .iter()
        .filter(|hit| {
            !matches!(
                hit.get("kind").and_then(Value::as_str),
                Some("row_limit" | "byte_cap")
            )
        })
        .filter_map(|hit| hit.get("text").and_then(Value::as_str))
        .collect();
    (!rest.is_empty()).then(|| format!("{tier}; caps: {}", rest.join("; ")))
}

fn run() -> Result<(), String> {
    let started = std::time::Instant::now();
    let argv: Vec<String> = std::env::args().collect();
    let matches = Cli::command().try_get_matches().unwrap_or_else(|error| {
        // A hook the host cannot parse the arguments of (a newer install's
        // flag given to this older binary, or the reverse) says so on stderr
        // and exits 0: Claude Code blocks the prompt on exit 2, which is
        // what clap exits with.
        if fails_open(&argv, &error) {
            eprintln!("{error}");
            std::process::exit(0);
        }
        // An unknown subcommand may be a plugin; its exit code is the
        // plugin's own, or 1/2 for a refusal or a moved-command hint.
        if let Some(code) = plugin_cmd::dispatch(&error, &argv) {
            std::process::exit(code);
        }
        error.exit()
    });
    let command_label = matches.subcommand_name().unwrap_or("unknown").to_string();
    let path = operation_path(&matches).unwrap_or_else(|| PathBuf::from("."));
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
    let protected = matches!(
        &cli.command,
        Command::SearchLikeRg { .. }
            | Command::RunHook { .. }
            | Command::Status {
                statusline: true,
                ..
            }
            | Command::Daemon {
                cmd: DaemonCmd::Start {
                    foreground: true,
                    ..
                }
            }
            // The start a hook launches is not an operation of the user's.
            | Command::Daemon {
                cmd: DaemonCmd::Start {
                    warm_meaning: true,
                    ..
                }
            }
    );
    let root = discover_root(&path).or_else(|_| discover_root(Path::new(".")));
    // `--metrics=off` and `PIXEL_METRICS=0` veto one invocation; the
    // `metrics` key in `.pixel/config.yaml` (repo, then `~/.pixel/` global)
    // is the persistent opt-out layer beneath them.
    let live = !protected
        && cli.metrics != "off"
        && std::env::var_os("PIXEL_METRICS").is_none_or(|v| v != "0")
        && config_cmd::metrics_enabled(root.as_deref().ok());
    operation_metrics::begin(root.as_deref().unwrap_or(Path::new(".")));
    let release_check = update_notice::enabled(
        protected,
        &command_label,
        std::io::IsTerminal::is_terminal(&std::io::stderr()),
        |name| std::env::var_os(name),
    )
    .then(|| update_notice::state_path(|name| std::env::var_os(name)))
    .flatten()
    .map(|path| {
        update_notice::begin(
            path,
            task_runtime::now_unix(),
            update_notice::fetch_latest_tag,
        )
    });
    // Compatibility fallback must exec the original before any logging changes
    // its search corpus; its successful Pixel branch retains existing logging.
    let mut logger = match &root {
        // The compatibility search exits through its own path before any
        // event is logged; spawning the writer would only create `.pixel` in
        // unindexed repos, which the search then mistakes for an index.
        _ if matches!(
            &cli.command,
            Command::SearchLikeRg { .. } | Command::Brief { .. }
        ) =>
        {
            pixel_actionlog::ActionLog::noop()
        }
        Ok(root) => pixel_actionlog::ActionLog::spawn_for_root(root),
        Err(_) => pixel_actionlog::ActionLog::noop(),
    };
    // An exit code a command owns end to end. `pixel evaluate` answers on
    // a three-way contract (0 evaluated / 2 usage / 3 technical) that
    // `main`'s two-way `Result` cannot carry, and it prints its own
    // envelope, so it must not return `Err` — that would add a second
    // diagnostic to stderr — nor exit before the action log is written.
    let owned_exit: std::cell::Cell<Option<i32>> = std::cell::Cell::new(None);
    let result = run_command(cli.command, &logger, &owned_exit);
    let owned_exit = owned_exit.get();
    if let Err(error) = &result {
        // The stdout contract under `--json`: a failing command answers with a
        // parsable failure envelope carrying `error.code`, so an agent can
        // tell NOT_FOUND (widen the query) from INVALID_INPUT (fix the call)
        // without reading prose. Human mode is unchanged — stdout stays empty,
        // the reason goes to stderr, the exit status stays 1 — and a command
        // that owns stdout or already wrote part of an answer keeps it.
        if failure_envelope_wanted(&matches, protected) {
            write_failure_envelope(&command_label, error);
        }
        // The diagnostic precedes the authoritative metrics line. Its failure
        // is best-effort and must never change the operation's result.
        let _ = operation_metrics::Counted(std::io::stderr().lock())
            .write_all(format!("pixel: {error}\n").as_bytes());
    }
    // A live metrics block would otherwise end stderr, and an agent reading
    // `2>&1 | tail -N` would see the block without the failure. The error is
    // repeated after it; with metrics off there is no block and no repeat.
    let trailer = result
        .as_ref()
        .err()
        .filter(|_| live)
        .map(|error| format!("pixel: {error}\n"));
    let _ = std::io::stdout().flush();
    let elapsed = started.elapsed();
    // After the answer, before the metrics block: a person at a terminal
    // reads it last-but-one, and nothing else ever sees it. After `elapsed`
    // too: waiting on the release check is not the command's cost.
    let update_line = release_check.and_then(|check| {
        update_notice::finish(
            check,
            task_runtime::now_unix(),
            env!("CARGO_PKG_VERSION"),
            release_upgrade_hint,
            std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()),
        )
    });
    if let Some(notice) = &update_line {
        eprint!("{notice}");
    }
    // A command that owns its exit code still reports its outcome to the
    // journal: a non-zero code is a failure there, even though it never
    // travelled as an `Err`.
    let logged_result = match owned_exit {
        Some(code) if code != 0 => Err(format!("{command_label} exited {code}")),
        _ => result.clone(),
    };
    let mut event = pixel_actionlog::ActionEvent::new(&command_label, logged_args(&argv[1..]))
        .with_result(&logged_result, elapsed);
    if let Ok(root) = &root
        && let Some(correlation) = task_commands::action_correlation(root)
    {
        event = event.with_task(correlation);
    }
    event.serve = serve_trace::take();
    if !protected {
        // The repeated error is rendered output the caller reads, written
        // after the block: count it here, before the block is sized.
        let output_bytes = operation_metrics::output_bytes()
            + trailer.as_ref().map_or(0, |text| text.len() as u64);
        let mut metrics = match operation_metrics::evidence(&command_label, result.is_ok()) {
            Ok(evidence) => {
                pixel_actionlog::OperationMetrics::new(elapsed, output_bytes, Some(evidence))
            }
            Err(gap) => pixel_actionlog::OperationMetrics::new(elapsed, output_bytes, None)
                .with_comparison_gap(gap),
        };
        // Optional policy input, never a measured LLM latency. Invalid or
        // non-Unicode values retain the versioned default without affecting
        // command success, diagnostics, or protected streams.
        if let Some(round_trip_ms) = std::env::var("PIXEL_METRICS_ROUND_TRIP_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
        {
            metrics = metrics.with_round_trip_ms(round_trip_ms);
        }
        metrics.output_scope = Some("cli-rendered-streams".to_owned());
        // The answer alone, which a measured whole-file read compares with
        // the file: the diagnostics above are not what the command returned.
        metrics.answer_bytes = Some(operation_metrics::stdout_bytes());
        event = event.with_metrics(metrics);
    }
    if live && let Some(line) = event.finalize_metrics_line() {
        // A blank record creates a distinct terminal block so transcript UIs
        // cannot visually attach the metrics matrix to command output.
        // Reporting bytes include this separator and the trailing newline.
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "\n{line}");
        if let Some(trailer) = &trailer {
            let _ = stderr.write_all(trailer.as_bytes());
        }
    }
    logger.log(event);
    // The record must be on disk before exit: `pixel action-log`, the
    // metrics history and the next invocation's footer read it back.
    // `finish` would drop it whenever the process exits first.
    logger.finish_flush();
    // The update question closes the command, after the journal: an exec
    // here must not lose the record of what just ran. `close_with_update`
    // re-checks the updater, the opt-outs and the owned exit code, asks,
    // upgrades, and on a taken update replaces this process with the new
    // binary on the same arguments.
    update_notice::close_with_update(
        update_notice::Close {
            notice: update_line.as_deref(),
            owned_exit,
            command_label: &command_label,
            stdin_is_terminal: std::io::IsTerminal::is_terminal(&std::io::stdin()),
            read_only: read_only_invocation(&matches),
            hint: release_upgrade_hint(),
            // A path that survives the upgrade: the stable PATH entry, not
            // the versioned store path `current_exe` resolves to.
            exe: std::env::current_exe()
                .ok()
                .map(pixel_setup::install::stable_exe_path),
            args: &argv[1..],
        },
        |name| std::env::var_os(name),
        &mut |prompt| {
            eprint!("{prompt}");
            let _ = std::io::stderr().flush();
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer).map(|_| answer)
        },
        &mut update_notice::run_upgrade,
        &mut |exe, args| update_notice::relaunch(exe, args),
    );
    if let Some(code) = owned_exit {
        std::process::exit(code);
    }
    result
}

/// `pixel setup`: deploy the agent integrations (globally, or into one
/// repository with `--repo`) and report what changed.
fn setup_install(json: bool, repo: Option<PathBuf>) -> Result<(), String> {
    let is_global_install = repo.is_none();
    let config_root = repo.clone();
    let stdout_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let interactive_banner = should_render_install_banner(json, stdout_tty);
    let color = banner_color(std::env::var_os("NO_COLOR").as_deref());
    if interactive_banner {
        // #863: the four-act intro animation is retired; the card header
        // (logo over version line) is the whole preamble now.
        write_stdout(&pixel_setup::banner::render_start(color))?;
    }
    let report = pixel_setup::install::install(&pixel_setup::install::InstallOptions {
        repo,
        ..Default::default()
    })
    .map_err(|e| e.to_string())?;
    config_cmd::ensure_template(config_root.as_deref())?;
    // Interactive UX goes to stderr so `--json` stdout stays pure.
    let stdin_tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let stderr_tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
    if should_offer_classify_setup(is_global_install, json, stdin_tty, stderr_tty) {
        config_cmd::setup()?;
    }
    let report_value = serde_json::to_value(&report).map_err(|e| e.to_string())?;
    // A person at a terminal reads the banner; `--json` and a piped
    // stdout keep the machine-readable report an agent parses.
    if json || !stdout_tty {
        print_data(&report_value, json)
    } else {
        operation_metrics::observe(&report_value);
        write_stdout(&pixel_setup::banner::render_result(&report, color))
    }
}

/// `pixel setup --remove`: take back everything `pixel setup` wrote.
fn setup_remove(json: bool, dry_run: bool, binary_path: Option<PathBuf>) -> Result<(), String> {
    let report = pixel_setup::uninstall::uninstall(&pixel_setup::uninstall::UninstallOptions {
        binary_path,
        running_binary: std::env::current_exe().ok(),
        dry_run,
        ..Default::default()
    })
    .map_err(|e| e.to_string())?;
    print_data(
        &serde_json::to_value(&report).map_err(|e| e.to_string())?,
        json,
    )
}

fn run_command(
    command: Command,
    logger: &pixel_actionlog::ActionLog,
    owned_exit: &std::cell::Cell<Option<i32>>,
) -> Result<(), String> {
    match command {
        Command::SearchLikeRg { tool, args } => search_compat::run(tool, args),
        Command::BuildIndex {
            path,
            extractor,
            max_gram,
            history,
        } => {
            let path = discover_root(&path)?;
            // Route through the daemon when available (singleton build —
            // no concurrent build races). Fall back to in-process build.
            if let Some(resp) = try_daemon(&path, &Request::Reindex {}) {
                let v = unwrap_response(resp)?;
                eprintln!(
                    "indexed via daemon: base_files={} delta_files={} overlay_files={}",
                    v.pointer("/index/base_files")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                    v.pointer("/index/delta_files")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                    v.pointer("/index/overlay_files")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                );
                if history {
                    let mut store =
                        pixel_facts::FactsStore::open(&path).map_err(|e| e.to_string())?;
                    let opts = pixel_facts::ingest::IngestOptions::default();
                    let report = pixel_facts::ingest::ingest_until_fresh(&mut store, &opts)
                        .map_err(|e| e.to_string())?;
                    eprintln!(
                        "facts: phase={} commits={} diff_coverage={:.0}% fresh={}",
                        report.phase,
                        report.commits_indexed,
                        report.diff_indexed_pct * 100.0,
                        report.fresh
                    );
                }
                return Ok(());
            }
            // In-process fallback (with build lock to prevent concurrent
            // build races when multiple CLI invocations hit the same root).
            let _lock =
                pixel_index::BuildLock::acquire(&path).map_err(|e| format!("build lock: {e}"))?;
            let ex = make_extractor(extractor, max_gram);
            let stats = build(&path, ex.as_ref()).map_err(|e| e.to_string())?;
            eprintln!(
                "indexed {} files ({} bytes) -> {} grams, shard {} bytes, {} ms",
                stats.files, stats.bytes, stats.grams, stats.shard_bytes, stats.elapsed_ms
            );
            if history {
                let mut store = pixel_facts::FactsStore::open(&path).map_err(|e| e.to_string())?;
                let opts = pixel_facts::ingest::IngestOptions::default();
                let report = pixel_facts::ingest::ingest_until_fresh(&mut store, &opts)
                    .map_err(|e| e.to_string())?;
                eprintln!(
                    "facts: phase={} commits={} diff_coverage={:.0}% fresh={}",
                    report.phase,
                    report.commits_indexed,
                    report.diff_indexed_pct * 100.0,
                    report.fresh
                );
            }
            Ok(())
        }
        Command::SearchContent {
            pattern,
            paths,
            json,
            stats,
            limit,
            offset,
            no_daemon,
            scope,
            context,
            ignore_case,
            globs,
            types,
            files_with_matches,
            fixed_strings,
            fallback_query,
            line_number: _,
        } => {
            if call_guard_check("search-content", &format!("{pattern} {paths:?}")) {
                return Err("circuit breaker: repeated search calls".to_string());
            }
            if fallback_query.as_deref().is_some_and(str::is_empty) {
                return Err("--fallback-query must not be empty".to_string());
            }
            if fallback_query.is_some() && !fixed_strings {
                return Err("--fallback-query requires -F/--fixed-strings".to_string());
            }
            // Fail on a bad `-g`/`-t` before any daemon round trip; the
            // daemon builds the same filter from the same rules.
            pixel_index::path_filter::PathFilter::new(&globs, &types)?;
            let pattern = if fixed_strings {
                regex::escape(&pattern)
            } else {
                pattern
            };
            run_search(
                pattern,
                paths,
                json,
                stats,
                limit,
                offset,
                no_daemon,
                scope,
                context,
                ignore_case,
                &globs,
                &types,
                files_with_matches,
                fallback_query,
                logger,
            )
        }
        Command::RunRecipe {
            intent,
            path,
            kind,
            budget,
            json,
            no_daemon,
        } => run_query(intent, path, &kind, budget, json, no_daemon, logger),
        Command::SearchMeaning {
            question,
            path,
            limit,
            max_files,
            json,
        } => run_ask(question, path, limit, max_files, json),
        Command::ScopeTask {
            task,
            path,
            json,
            limit,
            no_manifest,
            read_only,
            clear,
            max_tier,
            precision,
            regions,
        } => {
            if clear {
                // With --clear the sole positional (if any) is a path, not a
                // task: `gitpixel targets --clear .` must just work.
                let clear_path = match task {
                    Some(t) => {
                        let p = PathBuf::from(&t);
                        if p.exists() {
                            p
                        } else {
                            return Err("--clear takes no task argument".to_string());
                        }
                    }
                    None => path,
                };
                let root = discover_root(&clear_path)?;
                let shard_dir = root.join(pixel_index::index::SHARD_DIR);
                let manifest_path = shard_dir.join("targets.json");
                let regions_path = shard_dir.join("regions.json");
                if manifest_path.exists() {
                    std::fs::remove_file(&manifest_path)
                        .map_err(|e| format!("remove {}: {e}", manifest_path.display()))?;
                    println!("targets manifest cleared");
                } else {
                    println!("no active targets manifest");
                }
                // The regions manifest is single-task evidence — clear it
                // alongside targets.json so a stale task's regions never
                // outlives its enforcement manifest.
                if regions_path.exists() {
                    std::fs::remove_file(&regions_path)
                        .map_err(|e| format!("remove {}: {e}", regions_path.display()))?;
                }
                return Ok(());
            }
            let task =
                task.ok_or_else(|| "missing task description (or pass --clear)".to_string())?;
            let root = discover_root(&path)?;
            if overview_intent::is_overview_prompt(&task) {
                // Keyword targets for "what does this repo do" are files named
                // `repo*`; write no manifest, so nothing is scoped to them.
                println!("{}", overview_intent::overview_answer(&root));
                return Ok(());
            }
            let manifest_path = root
                .join(pixel_index::index::SHARD_DIR)
                .join("targets.json");
            if read_only && (max_tier.is_some() || precision) {
                return Err("--read-only does not support --max-tier or --precision".to_string());
            }
            let data = if read_only {
                execute_targets_facts_read_only(&path, task.clone(), limit)?
            } else {
                execute(
                    &path,
                    Request::Targets {
                        task: task.clone(),
                        limit,
                        max_tier: max_tier.clone(),
                        precision,
                        regions,
                    },
                    false,
                )?
            };
            let active_tasks = if no_manifest || read_only {
                None
            } else {
                Some(write_targets_manifest(&manifest_path, &task, &data)?)
            };
            // Issue #814: the regions manifest rides beside targets.json.
            // --no-manifest and --read-only suppress it with the enforcement
            // manifest; the JSON output still carries the regions either way.
            // A new targets.json without --regions removes any stale
            // regions.json so the two files stay coherent.
            if regions && !no_manifest && !read_only {
                write_regions_manifest(&root, &task, &data)?;
            } else if !no_manifest && !read_only {
                let regions_path = root
                    .join(pixel_index::index::SHARD_DIR)
                    .join("regions.json");
                if regions_path.exists() {
                    std::fs::remove_file(&regions_path)
                        .map_err(|e| format!("remove stale {}: {e}", regions_path.display()))?;
                }
            }
            if read_only {
                print_data(&data, true)?;
            } else {
                finish_graph_cmd(data, json, pretty_targets)?;
            }
            if let Some(active) = active_tasks {
                eprintln!(
                    "targets manifest active: {} ({active} task(s)) — scoping enforced; run `pixel scope-task --clear` when the task ends",
                    manifest_path.display()
                );
            }
            Ok(())
        }
        Command::Brief {
            prompt,
            path,
            json,
            parts,
        } => {
            let root = discover_root(&path)?;
            if json {
                let finished = execution_brief::chain::try_start_for_command(&prompt, &root, parts)
                    .map(execution_brief::chain::Pending::finish_with_record);
                let (record, text) = match finished {
                    Ok(finished) => (finished.record, finished.text),
                    Err(declined) => (
                        execution_brief::chain::declined_record(declined, &prompt),
                        None,
                    ),
                };
                let mut decision = record.to_json();
                decision["brief"] = text.map_or(Value::Null, Value::from);
                write_stdout(&format!("{decision}\n"))
            } else if let Some(text) =
                execution_brief::chain::start_for_command(&prompt, &root, parts)
                    .and_then(execution_brief::chain::Pending::finish)
            {
                write_stdout(&text)
            } else {
                Ok(())
            }
        }
        Command::ExecutionBrief {
            task,
            path,
            json,
            limit,
            max_tier,
            precision,
            no_manifest: _,
            no_daemon,
        } => {
            let data = execute(
                &path,
                Request::Targets {
                    task: task.clone(),
                    limit,
                    max_tier,
                    precision,
                    regions: false,
                },
                no_daemon,
            )?;
            let brief = execution_brief::from_scope_task(&task, &data);
            if json {
                print_data(&brief, true)
            } else {
                write_stdout(&execution_brief::pretty(&brief))
            }
        }
        Command::FindSymbol { name, path, json } => {
            let data = execute(&path, Request::Symbol { name }, false)?;
            finish_graph_cmd(data, json, |d| {
                let syms = d.get("symbols")?.as_array()?;
                let mut output = String::new();
                if syms.is_empty() {
                    output.push_str("no symbols found\n");
                } else {
                    for s in syms {
                        output.push_str(&symbol_line(s));
                        output.push('\n');
                    }
                }
                envelope_note(d);
                Some(output)
            })?;
            Ok(())
        }
        Command::ListSignatures { file, path, json } => {
            let data = execute(&path, Request::Skeleton { file }, false)?;
            finish_graph_cmd(data, json, |d| {
                let syms = d.get("symbols")?.as_array()?;
                let fname = d.get("file")?.as_str().unwrap_or("");
                let lang = d.get("lang")?.as_str().unwrap_or("");
                let rows = syms
                    .iter()
                    .map(|s| {
                        Some((
                            s.get("start_line")?.as_u64().unwrap_or(0),
                            s.get("kind")?.as_str().unwrap_or(""),
                            s.get("sig")?.as_str().unwrap_or(""),
                        ))
                    })
                    .collect::<Option<Vec<_>>>()?;
                Some(audit_cmd::render_outline(fname, lang, rows))
            })?;
            Ok(())
        }
        Command::RepoMap {
            path,
            markdown,
            json,
        } => {
            let data = execute(&path, Request::Map { markdown }, false)?;
            operation_metrics::observe(&data);
            if json {
                return print_data(&data, true);
            }
            announce_graph_build(&data);
            if markdown && let Some(md) = data.get("markdown").and_then(Value::as_str) {
                return write_stdout(md);
            }
            // Compact outline: one line per file, indented symbol names.
            let files = data.get("files").and_then(Value::as_array);
            let Some(files) = files else {
                return print_data(&data, false);
            };
            let mut output = String::new();
            for f in files {
                let path = f.get("path").and_then(Value::as_str).unwrap_or("?");
                output.push_str(path);
                output.push('\n');
                if let Some(syms) = f.get("symbols").and_then(Value::as_array) {
                    for s in syms {
                        output.push_str(&format!(
                            "  {} {}\n",
                            s.get("kind").and_then(Value::as_str).unwrap_or("?"),
                            s.get("name").and_then(Value::as_str).unwrap_or("?"),
                        ));
                    }
                }
            }
            write_stdout(&output)
        }
        Command::PackContext {
            uid,
            path,
            budget,
            json,
        } => {
            if call_guard_check("pack-context", &format!("{uid} {}", path.display())) {
                return Err("circuit breaker: repeated context calls".to_string());
            }
            let data = execute(
                &path,
                Request::Context {
                    uid,
                    budget_tokens: budget,
                },
                false,
            )?;
            finish_graph_cmd(data, json, |d| {
                let mut output = String::new();
                if let Some(s) = d.get("symbol") {
                    output.push_str(&symbol_line(s));
                    output.push('\n');
                }
                let text = d.get("text").and_then(Value::as_str).unwrap_or("");
                if !text.is_empty() {
                    output.push('\n');
                    output.push_str(text);
                    output.push('\n');
                } else {
                    output.push_str(&format!(
                        "\nincoming: {}\n",
                        serde_json::to_string_pretty(d.get("incoming").unwrap_or(&Value::Null))
                            .unwrap_or_default()
                    ));
                    output.push_str(&format!(
                        "outgoing: {}\n",
                        serde_json::to_string_pretty(d.get("outgoing").unwrap_or(&Value::Null))
                            .unwrap_or_default()
                    ));
                }
                envelope_note(d);
                Some(output)
            })?;
            Ok(())
        }
        Command::Impact {
            uid_or_name,
            path,
            direction,
            depth,
            workspace,
            no_daemon,
            no_refresh,
            json,
        } => {
            if call_guard_check("impact", &format!("{uid_or_name} {}", path.display())) {
                return Err("circuit breaker: repeated impact calls".to_string());
            }
            let dir = match direction {
                DirectionArg::Upstream => "upstream",
                DirectionArg::Downstream => "downstream",
            };
            if no_refresh {
                let data = impact_read::query(path, uid_or_name, dir, depth)?;
                finish_graph_cmd(data, json, |_| None)?;
                return Ok(());
            }
            if workspace {
                let results = workspace_cmd::fan_out(&path, &|| Request::Impact {
                    uid_or_name: uid_or_name.clone(),
                    direction: dir.to_string(),
                    depth,
                })?;
                return workspace_cmd::print_fan_out(&results, json);
            }
            let data = execute(
                &path,
                Request::Impact {
                    uid_or_name,
                    direction: dir.to_string(),
                    depth,
                },
                no_daemon,
            )?;
            finish_graph_cmd(data, json, |_| None)?;
            Ok(())
        }
        Command::WhoCalls {
            uid_or_name,
            path,
            role,
            offset,
            workspace,
            no_daemon,
            json,
        } => {
            let role_s = match role {
                RoleArg::Callers => "callers",
                RoleArg::Callees => "callees",
            };
            if workspace {
                let results = workspace_cmd::fan_out(&path, &|| Request::Uses {
                    uid_or_name: uid_or_name.clone(),
                    role: role_s.to_string(),
                    offset: Some(offset),
                })?;
                return workspace_cmd::print_fan_out(&results, json);
            }
            let data = execute(
                &path,
                Request::Uses {
                    uid_or_name,
                    role: role_s.to_string(),
                    offset: Some(offset),
                },
                no_daemon,
            )?;
            finish_graph_cmd(data, json, |d| {
                let edges = d.get("edges")?.as_array()?;
                let role = d.get("role").and_then(Value::as_str).unwrap_or("?");
                let mut output = String::new();
                if let Some(s) = d.get("symbol") {
                    output.push_str(&symbol_line(s));
                    output.push('\n');
                }
                output.push_str(&format!(
                    "{role}: {}/{} (offset {})\n",
                    edges.len(),
                    d.get("total_edges").and_then(Value::as_u64).unwrap_or(0),
                    d.get("offset").and_then(Value::as_u64).unwrap_or(0),
                ));
                for e in edges {
                    let tier = e.get("tier").and_then(Value::as_str).unwrap_or("?");
                    let line = e.get("site_line").and_then(Value::as_u64).unwrap_or(0);
                    match e.get("symbol").filter(|s| !s.is_null()) {
                        Some(s) => output
                            .push_str(&format!("  [{tier}] line {line}  {}\n", symbol_line(s))),
                        None => {
                            output.push_str(&format!("  [{tier}] line {line}  <unknown symbol>\n"))
                        }
                    }
                }
                envelope_note(d);
                Some(output)
            })?;
            Ok(())
        }
        Command::Rename {
            name,
            new_name,
            path,
            file,
            uid,
            dry_run,
            json,
        } => {
            let data = execute(
                &path,
                Request::Rename {
                    name,
                    new_name,
                    file,
                    uid,
                    dry_run,
                },
                false,
            )?;
            finish_graph_cmd(data, json, |d| {
                let mut output = String::new();
                let old = d.get("old_name").and_then(Value::as_str).unwrap_or("?");
                let new = d.get("new_name").and_then(Value::as_str).unwrap_or("?");
                let count = d.get("edit_count").and_then(Value::as_u64).unwrap_or(0);
                let verb = if d.get("dry_run").and_then(Value::as_bool) == Some(true) {
                    "would rename"
                } else {
                    "renamed"
                };
                output.push_str(&format!("{verb} {old} → {new} ({count} sites)\n"));
                for f in d.get("edits")?.as_array()? {
                    let path = f.get("path").and_then(Value::as_str).unwrap_or("?");
                    let kinds: Vec<String> = f
                        .get("edits")
                        .and_then(Value::as_array)
                        .map(|es| {
                            es.iter()
                                .map(|e| {
                                    format!(
                                        "L{}:{}",
                                        e.get("line").and_then(Value::as_u64).unwrap_or(0),
                                        e.get("kind").and_then(Value::as_str).unwrap_or("?"),
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    output.push_str(&format!("  {path}  ({})\n", kinds.join(", ")));
                }
                for s in d.get("skipped")?.as_array()? {
                    output.push_str(&format!(
                        "  skipped {}:{} — {}\n",
                        s.get("path").and_then(Value::as_str).unwrap_or("?"),
                        s.get("line").and_then(Value::as_u64).unwrap_or(0),
                        s.get("reason").and_then(Value::as_str).unwrap_or("?"),
                    ));
                }
                if let Some(obj) = d.get("unclaimed_text").and_then(Value::as_object)
                    && !obj.is_empty()
                {
                    let details: Vec<String> = obj
                        .iter()
                        .map(|(p, n)| format!("{p} ({})", n.as_u64().unwrap_or(0)))
                        .collect();
                    output.push_str(&format!(
                        "  note: unclaimed occurrences of the old name remain: {}\n",
                        details.join(", ")
                    ));
                }
                Some(output)
            })?;
            Ok(())
        }
        Command::CallPath {
            from,
            to,
            path,
            json,
        } => {
            let successor = call_path_successor(&from, &to, &path);
            let mut data = execute(&path, Request::Trace { from, to }, false)?;
            if let Some(fields) = data.as_object_mut() {
                fields.insert("successor".into(), successor);
            }
            finish_graph_cmd(data, json, |_| None)?;
            Ok(())
        }
        Command::Evaluate {
            cmd:
                EvaluateCmd::Path {
                    from,
                    to,
                    traversal,
                    tiers,
                    max_depth,
                    time_budget_ms,
                    scope,
                    at_snapshot,
                    path,
                    json,
                },
        } => {
            owned_exit.set(Some(evaluate_cmd::run(evaluate_cmd::EvaluateOptions {
                from,
                to,
                traversal,
                tiers,
                max_depth,
                time_budget_ms,
                scope,
                at_snapshot,
                path,
                json,
            })));
            Ok(())
        }
        Command::Cycles {
            tiers,
            max_nodes,
            max_edges,
            time_budget_ms,
            max_components,
            path,
            json,
        } => {
            owned_exit.set(Some(cycles_cmd::run(cycles_cmd::CyclesOptions {
                tiers,
                max_nodes,
                max_edges,
                time_budget_ms,
                max_components,
                path,
                json,
            })));
            Ok(())
        }
        Command::ListFlows { path, offset, json } => {
            let data = execute(
                &path,
                Request::Processes {
                    offset: Some(offset),
                },
                false,
            )?;
            finish_graph_cmd(data, json, |_| None)?;
            Ok(())
        }
        Command::ListAreas { path, offset, json } => {
            let data = execute(
                &path,
                Request::Clusters {
                    offset: Some(offset),
                },
                false,
            )?;
            finish_graph_cmd(data, json, |_| None)?;
            Ok(())
        }
        Command::WhatChanged {
            path,
            base,
            offset,
            tests,
            json,
        } => {
            if call_guard_check("what-changed", &format!("{} {:?}", path.display(), base)) {
                return Err("circuit breaker: repeated changes calls".to_string());
            }
            let data = execute(
                &path,
                Request::Changes {
                    base,
                    offset: Some(offset),
                    include_tests: tests,
                },
                false,
            )?;
            finish_graph_cmd(data, json, |_| None)?;
            Ok(())
        }
        Command::ReviewGate {
            path,
            base,
            fail_on,
            json,
        } => {
            let data = execute(&path, Request::ReviewGate { base }, false)?;
            if let Some(fail_on) = fail_on
                && review_gate_blocked(&data, fail_on.threshold())
            {
                owned_exit.set(Some(1));
            }
            finish_graph_cmd(data, json, pretty_review_gate)?;
            Ok(())
        }
        Command::RebuildGraph { path, json } => {
            let v = execute(&path, Request::Graph { if_stale: false }, false)?;
            eprintln!(
                "graph built in {} ms -> {}",
                v.get("elapsed_ms").and_then(Value::as_u64).unwrap_or(0),
                path.join(pixel_index::index::SHARD_DIR)
                    .join(pixel_daemon::api::GRAPH_DB_FILE)
                    .display()
            );
            print_data(&v, json)?;
            Ok(())
        }
        Command::GraphShadow {
            lang,
            path,
            examples,
            json,
        } => graph_shadow_cmd::run(&path, lang, examples, json),
        Command::Coverage { path, json } => {
            coverage_cmd::run(coverage_cmd::CoverageOptions { path, json })
        }
        Command::Audit { path, top, json } => {
            audit_cmd::run(audit_cmd::AuditOptions { path, top, json })
        }
        Command::Workspace { cmd } => workspace_cmd::run(cmd),
        Command::IndexPack {
            out,
            include_history,
            path,
        } => index_cmd::run(index_cmd::IndexCmd::Pack {
            out,
            include_history,
            path,
        }),
        Command::IndexUnpack {
            source,
            force,
            path,
        } => index_cmd::run(index_cmd::IndexCmd::Unpack {
            source,
            force,
            path,
        }),
        Command::Reference { cmd } => reference_cmd::run(cmd),
        Command::Status {
            path,
            json,
            statusline,
        } => {
            let mut data = execute(&path, Request::Status {}, false)?;
            // `status` is the compact freshness answer: keep the snapshot's
            // head/branch but collapse the dirty list to a count.
            compact_snapshot(&mut data);
            // The daemon/service attaches the `facts` block itself. A daemon
            // too old to send one gets the same block from the same producer,
            // never a client-side recount.
            if data.get("facts").is_none_or(serde_json::Value::is_null) {
                data["facts"] = pixel_daemon::api::facts_visibility(&path);
            }
            let facts_block = present_facts(&data);
            if statusline {
                // Compact one-liner — size + freshness + enrichment, so a
                // shell prompt/statusline shows staleness & coverage without
                // ever needing an explicit `pixel doctor`:
                //   `pixel: 285f 12k sym 45k edges 180/200 commits 90%diff fresh`
                let files = data
                    .get("index")
                    .and_then(|i| i.get("base_files").and_then(Value::as_u64))
                    .unwrap_or(0);
                let (sym, edges) = match data.get("graph") {
                    Some(g) if g.get("present").and_then(Value::as_bool).unwrap_or(false) => (
                        g.get("symbols").and_then(Value::as_u64).unwrap_or(0),
                        g.get("edges").and_then(Value::as_u64).unwrap_or(0),
                    ),
                    _ => (0, 0),
                };
                let fmt = |n: u64| -> String {
                    if n >= 1000 {
                        format!("{}k", n / 1000)
                    } else {
                        n.to_string()
                    }
                };
                let mut line = format!(
                    "pixel: {}f {} sym {} edges",
                    fmt(files),
                    fmt(sym),
                    fmt(edges)
                );
                // Enrichment coverage (only when the facts db exists and has
                // enough history to report a meaningful fraction): commits
                // indexed + diff-text coverage %, plus the fresh/stale state.
                if let Some(f) = facts_block.as_ref() {
                    if let (Some(ci), Some(tc)) = (
                        f.get("commits_indexed").and_then(Value::as_u64),
                        f.get("total_commits").and_then(Value::as_u64),
                    ) {
                        if tc > 0 {
                            line.push_str(&format!(" {ci}/{tc}"));
                        }
                        let covered = ci == tc;
                        if let Some(pct) = f.get("diff_indexed_pct").and_then(Value::as_f64) {
                            line.push_str(&format!(
                                " {:.0}%diff",
                                if covered {
                                    // Full commit coverage implies the diff
                                    // text is indexed too; report a clean 100%.
                                    if pct >= 99.0 { 100.0 } else { pct }
                                } else {
                                    pct
                                }
                            ));
                        }
                    }
                    let state = if f.get("fresh").and_then(Value::as_bool).unwrap_or(false) {
                        "fresh"
                    } else {
                        "stale"
                    };
                    line.push_str(&format!(" {state}"));
                } else {
                    line.push_str(" ?facts");
                }
                write_stdout(&format!("{line}\n"))?;
                return Ok(());
            }
            if json {
                print_data(&data, true)?;
            } else {
                let mut output = format!(
                    "root: {}\n",
                    data.get("root").and_then(Value::as_str).unwrap_or("?")
                );
                if let Some(i) = data.get("index") {
                    output.push_str(&format!(
                        "index: commit={} base_files={} delta_files={} overlay_files={} tombstones={}\n",
                        i.get("commit_oid").and_then(Value::as_str).unwrap_or("-"),
                        i.get("base_files").and_then(Value::as_u64).unwrap_or(0),
                        i.get("delta_files").and_then(Value::as_u64).unwrap_or(0),
                        i.get("overlay_files").and_then(Value::as_u64).unwrap_or(0),
                        i.get("tombstones").and_then(Value::as_u64).unwrap_or(0),
                    ));
                }
                match data.get("graph") {
                    Some(g) if g.get("present").and_then(Value::as_bool).unwrap_or(false) => {
                        output.push_str(&format!(
                            "graph: files={} symbols={} edges={} unresolved_calls={}\n",
                            g.get("files").and_then(Value::as_u64).unwrap_or(0),
                            g.get("symbols").and_then(Value::as_u64).unwrap_or(0),
                            g.get("edges").and_then(Value::as_u64).unwrap_or(0),
                            g.get("unresolved_calls")
                                .and_then(Value::as_u64)
                                .unwrap_or(0),
                        ));
                    }
                    _ => output.push_str("graph: not built (runs on first graph command)\n"),
                }
                if let Some(f) = facts_block.as_ref() {
                    output.push_str(&format!(
                        "facts: phase={} commits={}/{} diff_coverage={:.0}% fresh={} schema_version={}\n",
                        f.get("phase").and_then(Value::as_str).unwrap_or("?"),
                        f.get("commits_indexed").and_then(Value::as_u64).unwrap_or(0),
                        f.get("total_commits").and_then(Value::as_u64).unwrap_or(0),
                        f.get("diff_indexed_pct").and_then(Value::as_f64).unwrap_or(0.0) * 100.0,
                        f.get("fresh").and_then(Value::as_bool).unwrap_or(false),
                        f.get("schema_version").and_then(Value::as_i64).unwrap_or(0),
                    ));
                    // Only the daemon-side block carries the text and size
                    // counters; print them when present.
                    if let (Some(h), Some(used), Some(budget)) = (
                        f.get("hunks_with_text").and_then(Value::as_u64),
                        f.get("used_bytes").and_then(Value::as_u64),
                        f.get("budget_bytes").and_then(Value::as_u64),
                    ) {
                        output.push_str(&format!(
                            "facts-size: {} of {} budget, hunks_with_text={h} diffs_evicted={} diff_since={}\n",
                            mib(used),
                            mib(budget),
                            f.get("diffs_evicted").and_then(Value::as_u64).unwrap_or(0),
                            f.get("diff_coverage_since")
                                .and_then(Value::as_str)
                                .unwrap_or("-"),
                        ));
                    }
                } else {
                    output.push_str("facts: not built (runs on first history command)\n");
                }
                output.push_str(&format!(
                    "daemon: {}\n",
                    if daemon_ping(&path) {
                        "running"
                    } else {
                        "not running"
                    }
                ));
                write_stdout(&output)?;
            }
            Ok(())
        }
        Command::PrepareRepo {
            path,
            no_daemon,
            rebuild_graph,
            json,
        } => ready(path, no_daemon, rebuild_graph, json),
        Command::Evidence { path, jsonl } => {
            if !jsonl {
                return Err("evidence requires --jsonl".into());
            }
            let root = discover_root(&path)?;
            pixel_daemon::evidence::serve(&root, std::io::stdin().lock(), std::io::stdout())
        }
        Command::Daemon { cmd } => match cmd {
            DaemonCmd::Start {
                path,
                foreground,
                warm_meaning,
            } => {
                let root = discover_root(&path)?;
                daemon_start(root.clone(), foreground, false)?;
                if warm_meaning {
                    kick_meaning(&root);
                }
                Ok(())
            }
            DaemonCmd::Stop { path } => daemon_stop(discover_root(&path)?),
            DaemonCmd::Status { path } => daemon_status(discover_root(&path)?),
        },
        Command::Classify(options) => classify::run(options),
        Command::ClassifyEval {
            json,
            model_error_rate,
            model_latency_ms,
        } => {
            // The evaluation's verdict is its exit code (0 Go, 1 NoGo); keep
            // it rather than returning success for a NoGo.
            owned_exit.set(Some(classify_eval::run(
                classify_eval::ClassifyEvalOptions {
                    json,
                    model_error_rate,
                    model_latency_ms,
                },
            )));
            Ok(())
        }
        Command::ClassifyHistory { cmd } => classify_history_cmd::run_classify_history(cmd),
        // -------------------------------------------------------------
        // M3/M4 — engines
        // -------------------------------------------------------------
        Command::FindCode {
            phrase,
            path,
            limit,
            json,
        } => {
            if call_guard_check("find-code", &format!("{phrase} {}", path.display())) {
                return Err("circuit breaker: repeated resolve calls".to_string());
            }
            if overview_intent::is_overview_prompt(&phrase) {
                let root = discover_root(&path).unwrap_or_else(|_| path.clone());
                let answer = overview_intent::overview_answer(&root);
                return if json {
                    print_data(&json!({ "matches": [], "note": answer }), true)
                } else {
                    println!("{answer}");
                    Ok(())
                };
            }
            let mut data = execute(&path, Request::Resolve { phrase, limit }, false)?;
            if let Ok(root) = discover_root(&path) {
                enrich_resolve_matches_with_context(&mut data, &root);
            }
            if json {
                print_data(&data, true)
            } else {
                print_resolve_human(&data)
            }
        }
        // -------------------------------------------------------------
        // M5/M6 — install / doctor / hook
        // -------------------------------------------------------------
        Command::Setup {
            json,
            repo,
            yes,
            print,
            remove,
            dry_run,
            binary_path,
            selected_agents,
            selected_features,
            dummy_apply,
        } => {
            if remove {
                setup_remove(json, dry_run, binary_path)
            } else {
                let options = setup_cmd::SetupOptions {
                    repo,
                    print,
                    // TEMPORARY (dev-flags): this one field, the three args above
                    // and the three clap args in the `Setup` variant are the
                    // whole removal in the command surface.
                    dev: setup_cmd::DevOptions {
                        selected_agents,
                        selected_features,
                        dummy_apply,
                    },
                };
                // TEMPORARY (dev-flags): a released binary refuses the flags
                // here, before the guided/unattended decision, so a refused
                // flag never reaches a write.
                match setup_cmd::refuse_dev_flags(&options.dev) {
                    Some(refusal) => Err(refusal),
                    None if print || setup_cmd::is_guided(&options, yes, json) => {
                        setup_cmd::run(options)
                    }
                    None => setup_install(json, options.repo),
                }
            }
        }
        Command::Install { source, name } => {
            plugin_cmd::install(&source, name.as_deref(), owned_exit)
        }
        Command::Uninstall { name } => plugin_cmd::uninstall(&name),
        Command::CheckRelease {
            version,
            repo,
            json,
        } => {
            let report = pixel_release::run(&repo, &version)?;
            if json {
                print_data(&report.to_json(), true)?;
            } else {
                write_stdout(&report.render())?;
            }
            if report.ok() {
                Ok(())
            } else {
                Err("release-check failed".to_string())
            }
        }
        Command::SelfUpdate {
            build,
            install_path,
            dev,
            restart_daemon,
            repo,
            dry_run,
        } => {
            let home = std::env::var("HOME").map_err(|_| "HOME not set".to_string())?;
            let path_var = std::env::var_os("PATH");
            let target = if dev {
                UpgradeTarget {
                    path: dev_install_path(Path::new(&home)),
                    source: "--dev",
                    explicit: false,
                }
            } else {
                resolve_upgrade_target(
                    install_path,
                    std::env::current_exe().ok(),
                    path_var.as_deref(),
                    Path::new(&home),
                )
            };
            let refusal = upgrade_target_refusal(
                &target,
                &package_manager_roots(
                    Path::new(&home),
                    std::env::var_os("MISE_DATA_DIR").as_deref(),
                    std::env::var_os("HOMEBREW_CELLAR").as_deref(),
                ),
            );
            let dest = target.path;
            eprintln!("Install path: {} ({})", dest.display(), target.source);
            if dry_run {
                if let Some(other) = upgrade_shadowed_by(&dest, path_var.as_deref()) {
                    eprintln!("warning: {} precedes that path on PATH", other.display());
                }
                write_stdout(&format!("{}\n", dest.display()))?;
            }
            // A dry run still fails on a refused path, so
            // `pixel self-update --dry-run && pixel self-update` never writes.
            if let Some(reason) = refusal {
                return Err(reason);
            }
            if dry_run {
                return Ok(());
            }
            // 1. Build.
            eprintln!("Building: {build}");
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg(&build)
                .status()
                .map_err(|e| format!("build failed: {e}"))?;
            if !status.success() {
                return Err(format!("build exited with status {status}"));
            }
            // 2. Find the built binary (target/<profile>/pixel relative to
            //    cwd, profile taken from the build command's own flags).
            let src = std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join("target")
                .join(cargo_profile_dir(&build))
                .join("pixel");
            if !src.is_file() {
                return Err(format!("built binary not found at {}", src.display()));
            }
            // 3. Address only this repository; never signal unrelated processes.
            // Use the non-starting client: stopping must not launch a daemon.
            let repo_path = repo
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
                .canonicalize()
                .map_err(|e| format!("upgrade repository path: {e}"))?;
            // 4. Atomic install: copy to temp, then rename. In-place cp
            //    overwrites a mapped Mach-O on macOS, invalidating the
            //    ad-hoc code signature and causing SIGKILL on next run.
            eprintln!("Installing to {}", dest.display());
            // `$$` is a shell idiom and is NOT expanded by Rust, so a literal
            // name would collide between concurrent upgrades. Use the real pid,
            // and keep the temp file in `dest`'s directory so the rename stays
            // atomic (same filesystem). Mirrors scripts/release/install.sh's `.pixel.tmp.$$`.
            let tmp = dest.with_file_name(format!(
                ".{}.tmp.{}",
                dest.file_name().and_then(|n| n.to_str()).unwrap_or("pixel"),
                std::process::id()
            ));
            if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent).map_err(|e| format!("install directory: {e}"))?;
            }
            std::fs::copy(&src, &tmp).map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                format!("copy failed: {e}")
            })?;
            std::fs::rename(&tmp, &dest).map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                format!("rename failed: {e}")
            })?;
            eprintln!("Installed binary: {} -> {}", src.display(), dest.display());
            if let Some(other) = upgrade_shadowed_by(&dest, path_var.as_deref()) {
                eprintln!(
                    "warning: {} precedes the upgraded binary on PATH; `pixel` will keep \
                     running that copy until it is removed or PATH is reordered",
                    other.display()
                );
            }
            // 5. Stop only the selected repository after installation succeeds.
            // Resolve once: the runtime directory can become invalid while the
            // daemon unlinks its socket, but inspection must stay on that socket.
            let daemon_socket = daemon::socket_path(&repo_path);
            if let Some(response) = upgrade_daemon_request(&daemon_socket, &Request::Shutdown)? {
                if !response.ok {
                    return Err("installed binary, but repository daemon refused shutdown".into());
                }
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                loop {
                    if upgrade_daemon_socket_stopped(&daemon_socket)? {
                        break;
                    }
                    if std::time::Instant::now() >= deadline {
                        return Err("installed binary, but repository daemon did not stop".into());
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            if restart_daemon {
                eprintln!("Starting daemon in {}...", repo_path.display());
                let status = std::process::Command::new(&dest)
                    .arg("daemon")
                    .arg("start")
                    .arg(&repo_path)
                    .status()
                    .map_err(|e| format!("installed binary, but restart failed: {e}"))?;
                if !status.success() {
                    return Err(format!(
                        "installed binary, but restart exited with {status}"
                    ));
                }
            }
            eprintln!("Upgrade complete: {}", dest.display());
            Ok(())
        }
        Command::Doctor {
            path,
            json,
            shell,
            only,
            skip,
            fail_on,
            list,
            fix,
        } => {
            if list {
                let catalogue =
                    serde_json::to_value(pixel_setup::doctor::CHECKS).map_err(|e| e.to_string())?;
                return if json {
                    print_data(&catalogue, true)
                } else {
                    write_stdout(&pixel_setup::doctor::render_catalogue(
                        pixel_setup::doctor::CHECKS,
                    ))
                };
            }
            let options = discover_root(&path).map(|root| pixel_setup::doctor::DoctorOptions {
                repo_root: Some(root),
                shell,
                only,
                skip,
                ..Default::default()
            });
            let run = |options: &pixel_setup::doctor::DoctorOptions| {
                pixel_setup::doctor::doctor(options).map_err(|e| e.to_string())
            };
            // Exit 2 keeps "the checks could not run" apart from exit 1,
            // "a check found a problem", for a script gating on doctor.
            let options = options.inspect_err(|_| owned_exit.set(Some(2)))?;
            let mut report = run(&options).inspect_err(|_| owned_exit.set(Some(2)))?;
            let plan = pixel_setup::doctor::repair_plan(&report);
            let mut outcomes = None;
            if fix {
                let exe = std::env::current_exe().map_err(|e| e.to_string())?;
                let (plan, left) = pixel_setup::doctor::split_home_repairs(
                    plan.clone(),
                    pixel_setup::config::is_side_build(&exe),
                );
                for repair in &left {
                    eprintln!(
                        "pixel doctor --fix: left {} to the managed pixel: a pixel-dev side build does not rewrite the home install",
                        repair.command
                    );
                }
                let runs: Vec<_> = plan
                    .iter()
                    .map(|repair| {
                        eprintln!("pixel doctor --fix: running {}", repair.command);
                        pixel_setup::doctor::run_repair(&exe, repair)
                    })
                    .collect();
                if !plan.is_empty() {
                    report = run(&options).inspect_err(|_| owned_exit.set(Some(2)))?;
                }
                outcomes = Some(
                    plan.iter()
                        .cloned()
                        .zip(runs)
                        .map(|(repair, ran)| {
                            pixel_setup::doctor::judge_repair(repair, ran, &report)
                        })
                        .collect::<Vec<_>>(),
                );
            }
            let mut value = serde_json::to_value(&report).map_err(|e| e.to_string())?;
            if let Some(outcomes) = &outcomes {
                value["repairs"] = serde_json::to_value(outcomes).map_err(|e| e.to_string())?;
            }
            if json {
                print_data(&value, true)?;
            } else {
                operation_metrics::observe(&value);
                let mut text = String::new();
                if let Some(outcomes) = &outcomes {
                    text.push_str(&pixel_setup::doctor::render_repairs(outcomes));
                }
                text.push_str(&report.to_string());
                if outcomes.is_none() && !plan.is_empty() {
                    text.push_str(&format!(
                        "rerun with `--fix` to apply the {} repair command(s) above\n",
                        plan.len()
                    ));
                }
                write_stdout(&text)?;
            }
            if report.fails(fail_on.threshold()) {
                owned_exit.set(Some(1));
            }
            Ok(())
        }
        Command::RunHook { cmd } => match cmd {
            HookCmd::TaskEvent {
                provider,
                event,
                part,
            } => task_hook::run(provider, event, part),
            HookCmd::Metrics { provider, event } => {
                hook_metrics::run_metrics_hook(provider, event);
            }
        },
        Command::Config { cmd } => match cmd {
            None => config_cmd::overview(Path::new(".")),
            Some(ConfigCmd::Setup) => config_cmd::setup(),
            Some(ConfigCmd::Classify { value }) => config_cmd::set_classify_enabled(value == "on"),
            Some(ConfigCmd::Metrics {
                value,
                global,
                path,
            }) => config_cmd::run_metrics(&path, global, value.as_deref().map(|v| v == "on")),
            Some(ConfigCmd::RemoteKey {
                preset,
                value,
                clear,
            }) => config_cmd::key_from_arg(value, &mut std::io::stdin().lock())
                .and_then(|key| config_cmd::run_remote_key(preset, key, clear)),
            Some(ConfigCmd::RemotePreset {
                preset,
                model,
                base,
            }) => {
                config_cmd::set_classify_remote_model(preset, model, base.as_deref())?;
                println!("classify remote preset: {} stored", preset.display());
                Ok(())
            }
            Some(ConfigCmd::ClassifyEngine { value }) => {
                // `jev` and the `clef-*` names are remote engines on that preset.
                let hosted = decide_remote::Preset::parse_name(&value)
                    .filter(|preset| *preset == decide_remote::Preset::Jev || preset.is_clef());
                if let Some(preset) = hosted {
                    config_cmd::set_classify_remote_model(preset, None, None)?;
                    println!("classify engine: remote ({}) stored", preset.display());
                } else {
                    config_cmd::set_classify_engine(&value)?;
                    println!("classify engine: {value} stored");
                }
                Ok(())
            }
        },
        Command::TaskState { cmd } => task_commands::run(cmd),
        Command::Plan {
            prompt,
            path,
            query,
            tag,
            limit,
            format,
            no_verify,
            no_gates,
            max_todos,
            status,
            done,
            undone,
            prune,
            json,
        } => {
            let format = if json { "json".to_string() } else { format };
            plan_cmd::run(plan_cmd::PlanOptions {
                prompt,
                path,
                query,
                tag,
                limit,
                format,
                no_verify,
                no_gates,
                max_todos,
                status,
                done,
                undone,
                prune,
                json,
            })
        }
        Command::Plugin { cmd } => plugin_cmd::run(cmd),
    }
}

fn should_offer_classify_setup(
    is_global_install: bool,
    json: bool,
    stdin_tty: bool,
    stderr_tty: bool,
) -> bool {
    is_global_install && !json && stdin_tty && stderr_tty
}

fn run_query(
    intent: String,
    path: PathBuf,
    kind: &str,
    budget: usize,
    json_output: bool,
    no_daemon: bool,
    _logger: &pixel_actionlog::ActionLog,
) -> Result<(), String> {
    let kind = match kind {
        "auto" => QueryKind::Auto,
        "locate" => QueryKind::Locate,
        "scope" => QueryKind::Scope,
        "impact" => QueryKind::Impact,
        "history-recovery" => QueryKind::HistoryRecovery,
        "status" => QueryKind::Status,
        _ => return Err(format!("unsupported query kind '{kind}'")),
    };
    let mut result = compile_query(&intent, kind);
    if result.status == QueryStatus::Ranked {
        let output = serde_json::to_value(&result).map_err(|error| error.to_string())?;
        return print_data(&output, true);
    }
    if result.plan[0].recipe == "locate.v2" {
        return run_locate(result, &path, budget, json_output, no_daemon);
    }
    let operation = result.plan[0].operations[0].as_str();
    let evidence = match operation {
        "resolve" => {
            let phrase = intent
                .trim()
                .trim_start_matches("where is `")
                .trim_end_matches('`');
            execute(
                &path,
                Request::Resolve {
                    phrase: phrase.into(),
                    limit: None,
                },
                no_daemon,
            )?
        }
        "targets" => execute(
            &path,
            Request::Targets {
                task: intent.clone(),
                limit: None,
                max_tier: None,
                precision: false,
                regions: false,
            },
            no_daemon,
        )?,
        "impact" => {
            let target = intent.trim().trim_start_matches("show impact of ");
            execute(
                &path,
                Request::Impact {
                    uid_or_name: target.into(),
                    direction: "upstream".into(),
                    depth: Some(3),
                },
                no_daemon,
            )?
        }
        "excavate" => execute(
            &path,
            Request::Excavate {
                phrase: Some(intent.clone()),
                path: None,
                from: None,
                to: None,
                limit: None,
                read_only: false,
            },
            no_daemon,
        )?,
        "inspect" => execute(&path, Request::Inspect { files: None }, no_daemon)?,
        _ => return Err(format!("unsupported query operation '{operation}'")),
    };
    result.evidence.push(evidence);
    let output = json!({
        "op": "query",
        "result": result,
        "metrics": {"budget_tokens": budget, "operations": 1},
        "epistemics": {"closed_world": false, "lower_bound": true, "basis": "compiled bounded recipe"}
    });
    print_data(&output, json_output)
}

/// At most this many symbols get their context in a locate answer.
const LOCATE_MAX_TARGETS: usize = 3;

/// What every locate answer says about the test files it lists.
const LOCATE_TESTS_NOTE: &str = "test files among the callers at depth 1; Rust #[test] \
     functions are not indexed, so an empty list is not an absence of tests";

/// Tokens a rendered value costs, by the bytes/4 convention
/// `pixel_context::estimate_tokens` uses.
fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// `run-recipe --kind locate`: resolve the phrase, show the context of the
/// symbols it singles out (fresh source only, one shared budget), list the
/// test files among the best one's callers, and rank files for the task
/// when nothing resolved. Every operation goes through [`execute`], so the
/// daemon and the in-process service give the same answer.
fn run_locate(
    mut result: pixel_proto::QueryResult,
    path: &Path,
    budget: usize,
    json_output: bool,
    no_daemon: bool,
) -> Result<(), String> {
    use pixel_proto::query::{
        LocateStatus, caller_test_files, candidate_uid_in, context_is_stale, fit_locate_to_budget,
        locate_candidates, locate_next_action, locate_status, locate_targets, same_snapshot,
        split_context_budget,
    };
    let phrase = result
        .intent
        .trim_start_matches("where is `")
        .trim_end_matches('`')
        .to_owned();
    let mut operations = 0usize;
    let mut limits: Vec<String> = Vec::new();
    let mut responses: Vec<Value> = Vec::new();

    let resolve = execute(
        path,
        Request::Resolve {
            phrase: phrase.clone(),
            limit: None,
        },
        no_daemon,
    )?;
    operations += 1;
    let candidates = locate_candidates(&resolve);
    let picked = locate_targets(&candidates, LOCATE_MAX_TARGETS);
    let shares = split_context_budget(budget, picked.len());
    let mut targets: Vec<Value> = Vec::new();
    let mut target_uids: Vec<String> = Vec::new();
    let mut best_fresh = false;
    for (index, (candidate, share)) in picked.iter().zip(shares).enumerate() {
        let name = candidate.symbol.clone().unwrap_or_default();
        let ask = |uid: String| {
            execute(
                path,
                Request::Context {
                    uid,
                    budget_tokens: Some(share),
                },
                no_daemon,
            )
        };
        // Ask by the likely uid first: a homonym's candidate list may not fit
        // a small share, while one symbol's context does. A small share may
        // also leave the `symbol` field out, so the uid asked is the one kept.
        let mut asked: Option<(Value, Option<String>)> = None;
        if let Some(uid) = candidate.likely_uid() {
            operations += 1;
            if let Ok(context) = ask(uid.clone())
                && !context["candidates"].is_array()
            {
                asked = Some((context, Some(uid)));
            }
        }
        if asked.is_none() {
            operations += 1;
            match ask(name.clone()) {
                // A homonym answers with candidates: ask again for the one in
                // the matched file.
                Ok(context) => match candidate_uid_in(&context, &candidate.path) {
                    Some(uid) => {
                        operations += 1;
                        match ask(uid.clone()) {
                            Ok(again) => asked = Some((again, Some(uid))),
                            Err(error) => {
                                limits.push(format!("context of `{name}` unavailable: {error}"));
                            }
                        }
                    }
                    None => asked = Some((context, None)),
                },
                Err(error) => limits.push(format!("context of `{name}` unavailable: {error}")),
            }
        }
        let Some((context, asked_uid)) = asked else {
            continue;
        };
        let text = context["text"].as_str().unwrap_or_default().to_owned();
        let uid = if context_is_stale(&context) {
            None
        } else {
            context["symbol"]["uid"]
                .as_str()
                .map(str::to_owned)
                .or(asked_uid)
        };
        if index == 0 {
            best_fresh = uid.is_some();
        }
        if uid.is_none() {
            limits.push(format!(
                "`{name}` in {}: no fresh source to show",
                candidate.path
            ));
        }
        target_uids.extend(uid.clone());
        targets.push(json!({
            "uid": uid,
            "path": candidate.path,
            "start_line": candidate.start_line,
            "end_line": candidate.end_line,
            "reasons": candidate.reasons,
            "context_layer": context["context_layer"],
            "truncated": context["truncated"],
            "text": text,
        }));
        responses.push(context);
    }

    let status = locate_status(resolve["tier"].as_str(), &candidates, best_fresh);
    let mut tests_found: Vec<String> = Vec::new();
    if let Some(uid) = target_uids.first() {
        let uses = execute(
            path,
            Request::Uses {
                uid_or_name: uid.clone(),
                role: "callers".into(),
                offset: None,
            },
            no_daemon,
        )?;
        operations += 1;
        tests_found = caller_test_files(&uses);
        if uses["truncated"] == true {
            limits.push("callers were capped: more test files may exist".into());
        }
        responses.push(uses);
    }
    let mut files: Vec<Value> = Vec::new();
    if status == LocateStatus::NeedsSearch {
        let ranked = execute(
            path,
            Request::Targets {
                task: result.intent.clone(),
                limit: Some(5),
                max_tier: None,
                precision: false,
                regions: false,
            },
            no_daemon,
        )?;
        operations += 1;
        files = ranked["targets"]
            .as_array()
            .map(|targets| {
                targets
                    .iter()
                    .map(|t| json!({"path": t["path"], "tier": t["tier"], "reasons": t["reasons"]}))
                    .collect()
            })
            .unwrap_or_default();
        responses.push(ranked);
    }
    responses.push(resolve);
    let all: Vec<&Value> = responses.iter().collect();
    if !same_snapshot(&all) {
        limits.push(
            "the composed calls saw different snapshots: re-run for one consistent answer".into(),
        );
    }
    let next_action = locate_next_action(
        status,
        &phrase,
        &result.intent,
        &target_uids,
        candidates.first(),
    );

    let mut locate = json!({
        "status": status,
        "phrase": phrase,
        "targets": targets,
        "tests_found": tests_found,
        "tests_note": LOCATE_TESTS_NOTE,
        "files": files,
        "next_action": next_action,
        "limits": limits,
    });
    let tokens = |v: &Value| estimate_tokens(&v.to_string());
    fit_locate_to_budget(&mut locate, budget, tokens);
    result.evidence.clear();
    let output = json!({
        "op": "query",
        "result": result,
        "locate": locate,
        "metrics": {
            "budget_tokens": budget,
            "rendered_tokens_estimate": tokens(&locate),
            "operations": operations,
        },
        "epistemics": {"closed_world": false, "lower_bound": true, "basis": "composed bounded recipe: resolve, context, callers"}
    });
    if json_output {
        return print_data(&output, true);
    }
    operation_metrics::observe(&output);
    write_stdout(&render_locate(&output["locate"]))
}

/// The human rendering of a locate answer: status, each target with its
/// context, the test files, the next step and every limit.
fn render_locate(locate: &Value) -> String {
    let mut out = format!(
        "locate: {} — `{}`\n",
        locate["status"].as_str().unwrap_or("?"),
        locate["phrase"].as_str().unwrap_or_default()
    );
    for target in locate["targets"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "\n{}:{}-{} {}\n",
            target["path"].as_str().unwrap_or("?"),
            target["start_line"],
            target["end_line"],
            target["uid"].as_str().unwrap_or("(no fresh source)")
        ));
        out.push_str(target["text"].as_str().unwrap_or_default());
    }
    for file in locate["files"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "candidate file: {} ({})\n",
            file["path"].as_str().unwrap_or("?"),
            file["tier"].as_str().unwrap_or("?")
        ));
    }
    let tests: Vec<&str> = locate["tests_found"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    out.push_str(&format!(
        "\ntests found: {} ({})\n",
        if tests.is_empty() {
            "none".to_owned()
        } else {
            tests.join(", ")
        },
        locate["tests_note"].as_str().unwrap_or_default()
    ));
    if let Some(next) = locate["next_action"].as_str() {
        out.push_str(&format!("next: {next}\n"));
    }
    for limit in locate["limits"].as_array().into_iter().flatten() {
        out.push_str(&format!("limit: {}\n", limit.as_str().unwrap_or_default()));
    }
    out
}

/// `pixel ask "<question>"` — semantic code search via static embeddings.
/// Ranked answer, not resolved certainty. On model-embedding failure, reports
/// the reason and defers rather than crashing.
fn run_ask(
    question: String,
    path: PathBuf,
    limit: usize,
    max_files: Option<usize>,
    json: bool,
) -> Result<(), String> {
    let root = discover_root(&path)?;
    let result = pixel_recall::code_search::ask_with_metadata(&root, &question, limit, max_files)
        .map_err(|e| format!("ask: {e}"))?;
    let hits = &result.hits;
    if json {
        let rows: Vec<serde_json::Value> = hits
            .iter()
            .map(|h| {
                serde_json::json!({
                    "path": h.path,
                    "score": h.score,
                    "semantic_score": h.semantic_score,
                    "ranking_score": h.ranking_score,
                    "lexical_matches": h.lexical_matches,
                    "lexical_score": h.lexical_score,
                    "demoted": h.demoted.map(pixel_recall::code_search::FileKind::as_str),
                    "query_terms": h.query_terms,
                    "snippet": h.snippet,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "question": question,
                "hits": rows,
                "coverage": result.coverage,
                "note": "hybrid semantic/lexical ranking — score is cosine; ranking_score determines order; verify before acting",
            })
        );
        return Ok(());
    }
    let notes: Vec<String> = [
        result.coverage.sample_note(),
        result.coverage.vector_cache_note(),
    ]
    .into_iter()
    .flatten()
    .collect();
    if hits.is_empty() {
        println!("no matches found for \"{question}\" in {}", root.display());
        for note in &notes {
            println!("note: {note}");
        }
        return Ok(());
    }
    println!("hybrid matches for \"{question}\" (RRF ranking score; cosine semantic score):");
    for (i, h) in hits.iter().enumerate() {
        println!(
            "  {}. RRF {:.6}  cosine {:.3}  {} : \"{}\"",
            i + 1,
            h.ranking_score,
            h.semantic_score,
            h.path,
            h.snippet
        );
    }
    for note in &notes {
        println!("note: {note}");
    }
    Ok(())
}

/// Whether a parse error belongs to a `run-hook` invocation and is a real
/// error (help and version requests are not): a host hook never fails with
/// clap's exit code 2.
fn fails_open(argv: &[String], error: &clap::Error) -> bool {
    matches!(argv.get(1).map(String::as_str), Some("run-hook" | "hook")) && error.use_stderr()
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

/// Dry-runs one `pixel …` argv against the real clap definition; the
/// prompt-asset parity tests hold the bundled prompt's commands to it.
#[cfg(test)]
fn validate_cli_syntax(args: &[String]) -> Result<(), String> {
    let args = args.to_vec();
    std::thread::Builder::new()
        .stack_size(4 * 1024 * 1024)
        .spawn(move || match Cli::try_parse_from(&args) {
            Ok(_) => variadic_sentinel_misfit(&args).map_or(Ok(()), Err),
            Err(e) => Err(parse_error_summary(&e.to_string())),
        })
        .map_err(|e| e.to_string())?
        .join()
        .map_err(|_| "parse thread panicked".to_string())?
}

/// Clap's error paragraph on one line: the first line alone would say
/// "the following required arguments were not provided:" without naming
/// them, and the name is what a doctor report or a failing test needs.
#[cfg(test)]
fn parse_error_summary(rendered: &str) -> String {
    let summary = rendered
        .lines()
        .take_while(|line| !line.trim().is_empty())
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ");
    if summary.is_empty() {
        "parse error".to_string()
    } else {
        summary
    }
}

/// A parsed argv whose `...` placeholder put its second value
/// (`VARIADIC_SENTINEL`) in another argument than its first: the documented
/// shape parses only because the extra value fell into another slot, a
/// defaulted `PATH` or a multi-value positional after a one-value flag.
/// `None` when the argv has no sentinel or both values bound to one argument.
#[cfg(test)]
fn variadic_sentinel_misfit(args: &[String]) -> Option<String> {
    use pixel_setup::doctor::VARIADIC_SENTINEL;
    let sentinel = args.iter().position(|a| a == VARIADIC_SENTINEL)?;
    let mut root = Cli::command();
    root.build();
    let matches = root.clone().try_get_matches_from(args).ok()?;
    let (mut cmd, mut m) = (&root, &matches);
    // clap counts a subcommand's indices from its own name, so the
    // sentinel's index there is its argv position less the nesting depth.
    let mut depth = 0;
    while let Some((name, sub)) = m.subcommand() {
        cmd = cmd.find_subcommand(name)?;
        m = sub;
        depth += 1;
    }
    let sentinel = sentinel.checked_sub(depth)?;
    let holds = |id: &str, index: usize| {
        m.indices_of(id)
            .is_some_and(|mut indices| indices.any(|i| i == index))
    };
    let arg = cmd
        .get_arguments()
        .find(|a| holds(a.get_id().as_str(), sentinel))?;
    (!holds(arg.get_id().as_str(), sentinel - 1)).then(|| {
        format!(
            "a `...` placeholder's second value lands in `{}`, not in the argument its first value went to",
            arg.get_id()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// `jq .timings` is the one place a CI log reads the cost split from:
    /// the index layers and the graph phases move there (not copied, so a
    /// reader never has two answers), beside the wall time.
    #[test]
    fn ready_timings_should_gather_the_index_layers_and_graph_phases() {
        let mut status = serde_json::json!({
            "index": {"base_files": 3, "open": {"base": "reused", "base_ms": 7}},
        });
        let mut graph = serde_json::json!({
            "symbols": 9, "elapsed_ms": 40, "build": {"mode": "fresh"}, "phases": {"extract_ms": 30},
        });
        let timings = ready_timings(&mut status, &mut graph, Duration::from_millis(52));
        assert_eq!(
            timings,
            serde_json::json!({
                "total_ms": 52,
                "index": {"base": "reused", "base_ms": 7},
                "graph": {"elapsed_ms": 40, "build": {"mode": "fresh"}, "phases": {"extract_ms": 30}},
            })
        );
        assert_eq!(status, serde_json::json!({"index": {"base_files": 3}}));
        assert_eq!(graph, serde_json::json!({"symbols": 9, "elapsed_ms": 40}));

        // An older daemon sends neither block: nulls, never an error.
        let old = ready_timings(
            &mut serde_json::json!({"index": {}}),
            &mut serde_json::json!({"elapsed_ms": 5}),
            Duration::ZERO,
        );
        assert_eq!(old["index"], Value::Null);
        assert_eq!(
            old["graph"],
            serde_json::json!({"elapsed_ms": 5, "build": null, "phases": null})
        );
    }

    /// The human line names the phase to look at: the slowest one, by value
    /// and not by position, with the index cost summed over its layers.
    #[test]
    fn timings_line_should_name_the_index_source_and_the_slowest_phase() {
        let timings = serde_json::json!({
            "total_ms": 160,
            "index": {"base": "reused", "base_ms": 1, "delta_ms": 2, "overlay_ms": 4},
            "graph": {"elapsed_ms": 150, "build": {"mode": "full"}, "phases": {
                "collect_ms": 20, "extract_ms": 90, "verify_ms": 30,
            }},
        });
        assert_eq!(
            timings_line(&timings),
            "timings: total 160 ms, index 7 ms (base reused), graph 150 ms (full), slowest extract 90 ms"
        );
        assert_eq!(
            timings_line(&Value::Null),
            "timings: total ?, index ? (base ?), graph ? (?)"
        );
    }

    /// A uid embeds a file path, and a path may hold `$`, a space or a
    /// quote: every argument is single-quoted so the pasted command runs
    /// with the symbols and the repository `call-path` was given, without
    /// the shell expanding or splitting any of them.
    #[test]
    fn call_path_successor_should_quote_uids_names_and_the_repo_for_the_shell() {
        let successor = call_path_successor(
            "src/$(x).rs#Svc::run#method",
            "it's",
            Path::new("/tmp/my repo"),
        );
        assert_eq!(
            successor["command"],
            "pixel evaluate path --from 'src/$(x).rs#Svc::run#method' --to 'it'\\''s' '/tmp/my repo'"
        );
        let why = successor["why"].as_str().unwrap_or_default();
        assert!(why.contains("depth cap"), "{why}");
    }

    #[test]
    fn present_facts_drops_an_absent_history_db_and_keeps_the_rest() {
        assert_eq!(
            present_facts(&serde_json::json!({"facts": {"present": false}})),
            None
        );
        assert_eq!(present_facts(&serde_json::json!({})), None);
        let daemon = serde_json::json!({"present": true, "phase": "fresh"});
        assert_eq!(
            present_facts(&serde_json::json!({"facts": daemon.clone()})),
            Some(daemon)
        );
        let client = serde_json::json!({"phase": "phase_c"});
        assert_eq!(
            present_facts(&serde_json::json!({"facts": client.clone()})),
            Some(client),
            "the client-side block carries no `present` key"
        );
    }

    #[test]
    fn mib_prints_mebibytes_with_one_decimal() {
        assert_eq!(mib(0), "0.0 MiB");
        assert_eq!(mib(268_435_456), "256.0 MiB");
        assert_eq!(mib(1_572_864), "1.5 MiB");
    }

    #[test]
    fn read_only_targets_refuse_an_absent_daemon_without_creating_an_index() {
        let dir = std::env::temp_dir().join(format!(
            "pixel-targets-facts-read-only-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.as_path();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );

        let data = execute_targets_facts_read_only(root, "change login".into(), Some(8)).unwrap();

        assert_eq!(data["status"], "unavailable");
        assert_eq!(data["reason"], "daemon_unavailable");
        assert!(
            !root.join(".pixel").exists(),
            "a read-only fact query must not create index state"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--read-only` is a pure facts protocol; it takes none of the scoping
    /// tuning flags and must refuse each of them before any daemon/index
    /// work. Regression for the `&&` twin of the `max_tier || precision`
    /// guard: an `--read-only --max-tier P0` call must error, not silently
    /// proceed and drop the tunings.
    #[test]
    fn read_only_scope_task_rejects_tuning_flags() {
        let logger = pixel_actionlog::ActionLog::noop();
        let owned_exit = std::cell::Cell::new(None);
        // Isolated git repo: no pre-existing `.pixel/targets.json`, and the
        // read-only pass-through path (no tuning flags) must not create one.
        let dir =
            std::env::temp_dir().join(format!("pixel-read-only-tunings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.as_path();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );

        let base = || Command::ScopeTask {
            task: Some("a task".into()),
            path: root.to_path_buf(),
            json: false,
            limit: None,
            no_manifest: false,
            read_only: true,
            clear: false,
            max_tier: None,
            precision: false,
            regions: false,
        };

        let reject = |read_only: bool, max_tier: Option<&str>, precision: bool| {
            let mut cmd = base();
            match &mut cmd {
                Command::ScopeTask {
                    read_only: ro,
                    max_tier: mt,
                    precision: p,
                    ..
                } => {
                    *ro = read_only;
                    *mt = max_tier.map(str::to_string);
                    *p = precision;
                }
                _ => unreachable!(),
            }
            run_command(cmd, &logger, &owned_exit).err()
        };

        assert_eq!(
            reject(true, Some("P0"), false).as_deref(),
            Some("--read-only does not support --max-tier or --precision")
        );
        assert_eq!(
            reject(true, None, true).as_deref(),
            Some("--read-only does not support --max-tier or --precision")
        );
        // With neither tuning flag a read-only scope-task passes the guard
        // and still never writes an enforcement manifest (read-only arm of
        // `no_manifest || read_only`).
        assert!(reject(true, None, false).is_none());
        assert!(
            !root
                .join(pixel_index::index::SHARD_DIR)
                .join("targets.json")
                .exists(),
            "a read-only scope-task must never write a targets manifest"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `--json` failure envelope is decided from the parsed flag, not
    /// from argv: `--json` counts only where the command declares it, and it
    /// counts through a nested subcommand (`reference list --json`), whose
    /// flag the top-level matches cannot see. Parsed on a thread with a 4 MiB
    /// stack — the derived command tree overflows the 2 MiB a test thread
    /// gets, the same reason `validate_cli_syntax` spawns one.
    #[test]
    fn json_requested_reads_the_deepest_subcommands_own_flag() {
        fn parsed(argv: &[&str]) -> bool {
            let argv: Vec<String> = argv.iter().map(ToString::to_string).collect();
            std::thread::Builder::new()
                .stack_size(4 * 1024 * 1024)
                .spawn(move || json_requested(&Cli::command().get_matches_from(&argv)))
                .unwrap()
                .join()
                .unwrap()
        }

        assert!(parsed(&["pixel", "status", ".", "--json"]));
        assert!(parsed(&["pixel", "reference", "list", "--json"]));
        assert!(!parsed(&["pixel", "status", "."]));
        // A nested command with no `json` argument of its own: false, not a
        // flag inherited from somewhere in argv.
        assert!(!parsed(&["pixel", "daemon", "status", "."]));
    }

    /// A `.pixel` holding only the global journal (no `base.shard`) — e.g.
    /// the `$HOME/.pixel` state dir — must NOT anchor root discovery, or
    /// every gitless invocation below it re-roots to that ancestor and
    /// plain-walk-indexes the whole home directory. Regression for the
    /// `pixel resolve`/`search` in `~/.zcode` hang (9+ min, 1.7GB RSS via a
    /// journal-only `~/.pixel`).
    #[test]
    fn discover_root_ignores_journal_only_pixel_dir() {
        let base = std::env::temp_dir().join(format!(
            "pixel-discover-journal-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(base.join("home/.pixel")).unwrap();
        std::fs::create_dir_all(base.join("home/work")).unwrap();
        let work = std::fs::canonicalize(base.join("home/work")).unwrap();

        // Journal-only `.pixel` (actions/history, no shard): no anchor.
        assert_eq!(discover_root(&work).unwrap(), work);

        // With a shard present, the `.pixel` ancestor anchors as before.
        std::fs::write(
            base.join("home/.pixel")
                .join(pixel_index::index::SHARD_FILE),
            b"shard",
        )
        .unwrap();
        assert_eq!(
            discover_root(&work).unwrap(),
            std::fs::canonicalize(base.join("home")).unwrap()
        );
        std::fs::remove_dir_all(&base).ok();
    }

    /// End-to-end rule-vs-binary parity of the doctor's normalizer against
    /// THIS binary's real clap definition: every canonical rule command
    /// shape must normalize and dry-run parse. This is the compile-time-side
    /// twin of the runtime `rule.parity` doctor check.
    #[test]
    fn canonical_rule_command_lines_parse_against_the_real_cli() {
        let canonical = [
            r#"pixel find-code "<phrase>" /path/to/repo [--json] [--limit N]"#,
            r#"pixel search-content "<pattern>" /path/to/repo --context 5 [--json] [--limit N]"#,
            r#"pixel scope-task "<one-line task description>" /path/to/repo [--json] [--limit N]"#,
            r#"pixel scope-task --clear /path/to/repo"#,
            r#"pixel impact <symbol_name_or_uid> /path/to/repo [--direction upstream|downstream] [--depth N] [--json]"#,
            r#"pixel what-changed /path/to/repo [--base <ref>] [--json]"#,
            r#"pixel status /path/to/repo"#,
            r#"pixel build-index --history ."#,
            r#"pixel setup"#,
            r#"pixel doctor"#,
        ];
        let mut failures = Vec::new();
        for line in canonical {
            match pixel_setup::doctor::normalize_rule_command(line) {
                None => failures.push(format!("`{line}` did not normalize")),
                Some(argv) => {
                    if let Err(e) = validate_cli_syntax(&argv) {
                        failures.push(format!("`{line}` → argv {argv:?} → {e}"));
                    }
                }
            }
        }
        assert!(
            failures.is_empty(),
            "canonical rule command lines must parse against the real CLI:\n{}",
            failures.join("\n")
        );
    }

    /// A flag that takes one value per occurrence must not be shown with
    /// `...`: a rule line that does teaches an agent a command the CLI
    /// rejects, so the parity check must go red on it, not validate the
    /// single-value reading. The extra value lands in a multi-value
    /// positional (`paths`), which takes several values but not this flag's.
    #[test]
    fn variadic_single_value_flags_are_rejected_as_documented() {
        let line = r#"pixel search-content "<re>" --glob <g>..."#;
        let argv = pixel_setup::doctor::normalize_rule_command(line)
            .unwrap_or_else(|| panic!("`{line}` did not normalize"));
        assert!(
            validate_cli_syntax(&argv).is_err(),
            "`{line}` → argv {argv:?} should be rejected by the CLI parser"
        );
    }

    /// An argument that does take several values per occurrence keeps its
    /// `...` placeholder: the parity check must not go red on a true shape.
    #[test]
    fn variadic_placeholder_on_a_multi_value_argument_parses() {
        // The sentinel is the last value, so it is held only if its first
        // value's argument is found at the index just before it.
        let line = "pixel search-content \"<re>\" <path>...";
        let argv = pixel_setup::doctor::normalize_rule_command(line)
            .unwrap_or_else(|| panic!("`{line}` did not normalize"));
        assert_eq!(validate_cli_syntax(&argv), Ok(()), "argv {argv:?}");
    }

    /// A knowingly-wrong documented command must be REJECTED — this is what
    /// makes the parity check able to go red at all.
    #[test]
    fn known_bad_rule_command_lines_are_rejected() {
        for bad in [
            vec![
                "pixel".to_string(),
                "search-content".into(),
                "--no-such-flag".into(),
            ],
            vec!["pixel".to_string(), "frobnicate".into()],
            // Retired with the task worker runtime: a rule line naming one
            // must fail the parity check.
            vec![
                "pixel".to_string(),
                "task-state".into(),
                "accept".into(),
                "change greeting behavior".into(),
            ],
            vec![
                "pixel".to_string(),
                "task-state".into(),
                "worker-start".into(),
                "task-100-1".into(),
                "candidate-1".into(),
            ],
        ] {
            assert!(
                validate_cli_syntax(&bad).is_err(),
                "argv {bad:?} should be rejected by the CLI parser"
            );
        }
    }

    #[test]
    fn claude_task_runtime_commands_parse() {
        for argv in [
            vec![
                "pixel".to_string(),
                "task-state".into(),
                "begin".into(),
                "add durable task state".into(),
                "--provider".into(),
                "claude".into(),
                "--session".into(),
                "session-123".into(),
                "/repo".into(),
                "--json".into(),
            ],
            vec![
                "pixel".to_string(),
                "task-state".into(),
                "prepare".into(),
                "task-100-1".into(),
                "/repo".into(),
            ],
            vec![
                "pixel".to_string(),
                "task-state".into(),
                "status".into(),
                "task-100-1".into(),
                "/repo".into(),
                "--json".into(),
            ],
            vec![
                "pixel".to_string(),
                "task-state".into(),
                "events".into(),
                "task-100-1".into(),
                "/repo".into(),
            ],
            vec![
                "pixel".to_string(),
                "task-state".into(),
                "show".into(),
                "--session".into(),
                "session-123".into(),
                "/repo".into(),
                "--json".into(),
            ],
        ] {
            assert!(validate_cli_syntax(&argv).is_ok(), "argv {argv:?}");
        }
    }

    #[test]
    fn enrich_with_context_returns_surrounding_lines() {
        // Create a temp file with known content
        let dir = std::env::temp_dir();
        let path = dir.join("pixel_ctx_test.rs");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "line 1").unwrap();
        writeln!(f, "line 2").unwrap();
        writeln!(f, "line 3").unwrap();
        writeln!(f, "pub const FOO: &str =").unwrap();
        writeln!(f, "    \"bar\";").unwrap();
        writeln!(f, "line 6").unwrap();
        writeln!(f, "line 7").unwrap();
        drop(f);

        let root = dir;
        let match_val = serde_json::json!({
            "path": "pixel_ctx_test.rs",
            "line": 4,
            "text": "pub const FOO: &str ="
        });

        let enriched = enrich_with_context(&match_val, &root, 2, false, &mut HashMap::new());
        let ctx = enriched
            .get("context")
            .and_then(Value::as_str)
            .unwrap_or("");

        // Should contain lines 2-6 (context=2 around line 4)
        assert!(
            ctx.contains(">>     4: pub const FOO"),
            "match line should be marked with >>"
        );
        assert!(
            ctx.contains("      2: line 2"),
            "should include 2 lines before"
        );
        assert!(
            ctx.contains("      6: line 6"),
            "should include 2 lines after"
        );
        assert!(
            !ctx.contains("line 1"),
            "should not include lines outside context window"
        );
        assert!(
            !ctx.contains("line 7"),
            "should not include lines outside context window"
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn enrich_with_context_zero_context_returns_original() {
        let match_val = serde_json::json!({"path": "nonexistent.rs", "line": 1, "text": "foo"});
        let enriched =
            enrich_with_context(&match_val, Path::new("/tmp"), 0, false, &mut HashMap::new());
        // context=0 means no enrichment — original returned
        assert!(
            enriched.get("context").is_none(),
            "context=0 should not add context field"
        );
    }

    #[test]
    fn enrich_with_context_missing_file_returns_original() {
        let match_val =
            serde_json::json!({"path": "does_not_exist_xyz.rs", "line": 1, "text": "foo"});
        let enriched =
            enrich_with_context(&match_val, Path::new("/tmp"), 5, false, &mut HashMap::new());
        // File doesn't exist — should return original without context
        assert!(
            enriched.get("context").is_none(),
            "missing file should not add context"
        );
    }

    #[test]
    fn enrich_with_context_clamps_at_file_boundaries() {
        let dir = std::env::temp_dir();
        let path = dir.join("pixel_ctx_short.rs");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "only line").unwrap();
        drop(f);

        let root = dir;
        let match_val = serde_json::json!({
            "path": "pixel_ctx_short.rs",
            "line": 1,
            "text": "only line"
        });

        // Request 10 lines of context but file only has 1
        let enriched = enrich_with_context(&match_val, &root, 10, false, &mut HashMap::new());
        let ctx = enriched
            .get("context")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(
            ctx.contains(">>     1: only line"),
            "should contain the match line"
        );
        assert!(!ctx.contains("line 0"), "should not go before line 1");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn enrich_with_context_caches_file_content() {
        let dir = std::env::temp_dir();
        let path = dir.join("pixel_ctx_cache.rs");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "line 1").unwrap();
        writeln!(f, "line 2").unwrap();
        writeln!(f, "line 3").unwrap();
        drop(f);

        let root = dir;
        let mut cache: HashMap<PathBuf, Option<String>> = HashMap::new();
        let m1 = serde_json::json!({"path": "pixel_ctx_cache.rs", "line": 1, "text": "line 1"});
        let m2 = serde_json::json!({"path": "pixel_ctx_cache.rs", "line": 2, "text": "line 2"});
        let e1 = enrich_with_context(&m1, &root, 1, false, &mut cache);
        let e2 = enrich_with_context(&m2, &root, 1, false, &mut cache);
        assert!(e1.get("context").and_then(Value::as_str).is_some());
        assert!(e2.get("context").and_then(Value::as_str).is_some());
        // The cache holds the file content so the second call did not re-read.
        let key = root.join("pixel_ctx_cache.rs");
        assert!(cache.contains_key(&key));
        assert!(cache.get(&key).unwrap().is_some());

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn resolve_context_covers_full_multiline_span_not_just_start_line() {
        let dir = std::env::temp_dir();
        let path = dir.join("pixel_resolve_ctx_test.rs");
        let mut f = std::fs::File::create(&path).unwrap();
        for i in 1..=10 {
            writeln!(f, "line {i}").unwrap();
        }
        drop(f);

        let root = dir;
        let mut data = serde_json::json!({
            "matches": [
                {"path": "pixel_resolve_ctx_test.rs", "start_line": 4, "end_line": 7}
            ]
        });
        enrich_resolve_matches_with_context(&mut data, &root);
        let ctx = data["matches"][0]
            .get("context")
            .and_then(Value::as_str)
            .unwrap_or("");
        // Full span [4,7] must be marked, not just the start line.
        assert!(ctx.contains(">>     4: line 4"));
        assert!(ctx.contains(">>     5: line 5"));
        assert!(ctx.contains(">>     6: line 6"));
        assert!(ctx.contains(">>     7: line 7"));
        // Margin lines present but unmarked.
        assert!(ctx.contains("  2: line 2") || ctx.contains(" 2: line 2"));
        assert!(!ctx.contains(">>     2: line 2"));
        assert!(!ctx.contains(">>     9: line 9"));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn resolve_context_skips_match_with_no_start_line() {
        let mut data = serde_json::json!({
            "matches": [{"path": "whatever.rs"}]
        });
        enrich_resolve_matches_with_context(&mut data, Path::new("/tmp"));
        assert!(data["matches"][0].get("context").is_none());
    }

    #[test]
    fn files_only_output_cuts_between_paths_at_the_cap() {
        let paths = ["ab".to_string(), "cde".to_string()];
        assert_eq!(
            files_only_output(&paths, 7),
            ("ab\ncde\n".into(), 2),
            "7 bytes fit both"
        );
        assert_eq!(files_only_output(&paths, 6), ("ab\n".into(), 1));
        assert_eq!(files_only_output(&paths, 3), ("ab\n".into(), 1));
        assert_eq!(files_only_output(&paths, 2), (String::new(), 0));
        assert_eq!(files_only_output(&[], 0), (String::new(), 0));
    }

    /// The `--json` page trailer: the page state a match row cannot carry,
    /// plus the envelope honesty fields, with every key always present so a
    /// strict NDJSON reader can tell a partial page from a complete one.
    #[test]
    fn search_meta_carries_the_page_state_and_the_envelope_honesty() {
        let data = serde_json::json!({
            "offset": 0,
            "epistemics": {"basis": "text index", "lower_bound": false},
            "warnings": [{"code": "cap", "message": "row cap"}],
        });
        let resumable = search_meta(&data, true, Some(3));
        assert_eq!(resumable["truncated"], true);
        assert_eq!(resumable["next_offset"], 3);
        assert_eq!(resumable["epistemics"]["basis"], "text index");
        assert_eq!(resumable["warnings"][0]["code"], "cap");
        let complete = search_meta(&data, false, None);
        assert_eq!(complete["truncated"], false);
        assert!(complete["next_offset"].is_null());
        let bare = search_meta(&serde_json::json!({}), true, None);
        assert!(bare["epistemics"].is_null());
        assert!(bare["warnings"].is_null());
    }

    /// The stdout cap counts the reserved metadata line, and a line that ends
    /// exactly on the cap still prints: the cut lands between lines, so a
    /// reader never gets a half-written JSON row.
    #[test]
    fn a_match_line_that_ends_exactly_on_the_cap_still_fits() {
        assert!(fits_before_cap(0, 10, 0, 10));
        assert!(!fits_before_cap(0, 11, 0, 10));
        assert!(fits_before_cap(4, 4, 2, 10));
        assert!(!fits_before_cap(4, 5, 2, 10), "the reserved trailer counts");
    }

    /// Both renderings of one match: the JSON object agents parse, and the
    /// human row or `--- path:line ---` context block.
    #[test]
    fn search_match_lines_render_the_json_and_human_forms() {
        let plain = serde_json::json!({"path": "a.rs", "line": 3, "text": "x"});
        assert_eq!(
            search_match_line(&plain, true),
            "{\"line\":3,\"path\":\"a.rs\",\"text\":\"x\"}\n"
        );
        assert_eq!(search_match_line(&plain, false), "a.rs:3:x\n");
        let enriched =
            serde_json::json!({"path": "a.rs", "line": 3, "text": "x", "context": ">> 3: x"});
        assert_eq!(
            search_match_line(&enriched, true),
            "{\"context\":\">> 3: x\",\"line\":3,\"path\":\"a.rs\",\"text\":\"x\"}\n"
        );
        assert_eq!(
            search_match_line(&enriched, false),
            "--- a.rs:3 ---\n>> 3: x\n"
        );
    }
}

/// Every `pixel …` line in the fenced blocks and table cells of the two
/// bundled prompt assets must parse against this binary's clap definition.
/// `pixel doctor`'s `rule.parity` only covers the rule text installed on a
/// machine; the assets themselves are what every wrapped `claude`, every
/// Codex session and every print-mode sub-agent reads, so their drift has to
/// fail the build.
#[cfg(test)]
mod prompt_asset_parity {
    use pixel_setup::doctor::{extract_rule_commands, normalize_rule_command};

    const ASSETS: [(&str, &str); 2] = [
        (
            "pixel-agent-prompt.md",
            include_str!("../../pixel-setup/assets/pixel-agent-prompt.md"),
        ),
        (
            "pixel-subagent-prompt.md",
            include_str!("../../pixel-setup/assets/pixel-subagent-prompt.md"),
        ),
    ];

    /// Why an asset's command lines would mislead an agent, one entry per
    /// line, and how many lines were dry-run parsed.
    ///
    /// Agents copy these lines word for word, so a line the parser rejects
    /// is a command that fails in their hands (`pixel new-branch name`
    /// without `--request-id` exited 2, and the agent's next `pixel commit`
    /// landed on `main`). A line the normalizer cannot read is a failure
    /// too, never a silent skip: that is how the table's shapes escaped
    /// this test. The one exception is an elision, `pixel <command> …`,
    /// which stands for "see the full form": it passes only when the same
    /// asset spells that command out in a line that parses.
    fn asset_failures(name: &str, text: &str) -> (usize, Vec<String>) {
        let mut checked = 0;
        let mut failures = Vec::new();
        let mut parsed_commands = std::collections::BTreeSet::new();
        let mut elided = Vec::new();
        for line in extract_rule_commands(text) {
            let command = line.split_whitespace().nth(1).unwrap_or("").to_string();
            let Some(argv) = normalize_rule_command(&line) else {
                if line.ends_with(" …") {
                    elided.push((line, command));
                } else {
                    failures.push(format!(
                        "{name}: `{line}` has placeholders the normalizer cannot read"
                    ));
                }
                continue;
            };
            checked += 1;
            match super::validate_cli_syntax(&argv) {
                Ok(()) => {
                    parsed_commands.insert(command);
                }
                Err(error) => failures.push(format!("{name}: `{line}` → {error}")),
            }
        }
        for (line, command) in elided {
            if !parsed_commands.contains(&command) {
                failures.push(format!(
                    "{name}: `{line}` elides its arguments, but no full `pixel {command} …` line in this asset parses"
                ));
            }
        }
        (checked, failures)
    }

    #[test]
    fn every_documented_command_line_parses_against_the_cli() {
        let mut checked = 0;
        let mut failures = Vec::new();
        for (name, text) in ASSETS {
            let (n, f) = asset_failures(name, text);
            assert!(
                n > 0,
                "{name}: no `pixel …` line parsed — the extractor or the asset changed shape"
            );
            checked += n;
            failures.extend(f);
        }
        assert!(checked > 0, "nothing was checked");
        assert!(
            failures.is_empty(),
            "documented command lines the CLI rejects:\n{}",
            failures.join("\n")
        );
    }

    /// Rows that show a command without its required arguments must go red,
    /// each named with clap's reason, or the test above cannot catch their
    /// like.
    #[test]
    fn the_shipped_replacement_map_rows_are_rejected_by_name() {
        let text = "\
| Instead of | Run |
| --- | --- |
| `git log --oneline` | `pixel search-content --json` / `pixel impact` / `pixel find-code …` |
";
        let (_, failures) = asset_failures("sample.md", text);
        assert_eq!(failures.len(), 3, "{failures:#?}");
        assert!(
            failures[0].starts_with("sample.md: `pixel search-content --json` → ")
                && failures[0].contains("required"),
            "{}",
            failures[0]
        );
        assert!(
            failures[1].starts_with("sample.md: `pixel impact` → ")
                && failures[1].contains("required"),
            "{}",
            failures[1]
        );
        assert_eq!(
            failures[2],
            "sample.md: `pixel find-code …` elides its arguments, but no full `pixel find-code …` line in this asset parses"
        );
    }

    /// A placeholder for an integer argument is a correct line: the agent
    /// substitutes a real id, so the dummy must not be what fails it.
    #[test]
    fn a_placeholder_for_an_integer_argument_parses() {
        let text = "| a | `pixel scope-task \"<task>\" . --limit <n>` / `pixel search-content \"<re>\" . --limit <n>` |\n";
        assert_eq!(asset_failures("sample.md", text), (2, vec![]));
    }

    /// An elision is accepted once the asset spells the command out in a
    /// line that parses, and a line with an unreadable placeholder is
    /// reported, not skipped.
    #[test]
    fn an_elision_needs_a_full_form_and_an_unreadable_line_is_reported() {
        let text = "\
| a | `pixel find-code …` |
| b | `pixel find-code \"token\" .` |
| c | `pixel impact a…b` |
";
        let (checked, failures) = asset_failures("sample.md", text);
        assert_eq!(checked, 1);
        assert_eq!(
            failures,
            vec!["sample.md: `pixel impact a…b` has placeholders the normalizer cannot read"]
        );
    }
}

#[cfg(test)]
mod hook_prompt_tests {
    use super::{bounded_result_note, is_developer_build, logged_args};

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_remote_key_never_reaches_the_action_log() {
        let args = |words: &[&str]| words.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(
            logged_args(&args(&[
                "--metrics",
                "off",
                "config",
                "remote-key",
                "openrouter",
                "sk-secret"
            ])),
            "--metrics off config remote-key openrouter <redacted>"
        );
        assert_eq!(
            logged_args(&args(&["config", "remote-key", "ollama", "--clear"])),
            "config remote-key ollama --clear",
            "flags stay readable"
        );
        assert_eq!(
            logged_args(&args(&["search-content", "config", "src"])),
            "search-content config src",
            "other commands are logged verbatim"
        );
    }

    #[test]
    fn a_plugin_source_url_is_logged_without_its_credentials() {
        let argv = |words: &[&str]| words.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(
            logged_args(&argv(&[
                "install",
                "https://user:ghp_secret@github.com/acme/private.git",
                "--name",
                "x"
            ])),
            "install https://github.com/acme/private.git --name x"
        );
        assert_eq!(
            logged_args(&argv(&[
                "--metrics",
                "off",
                "install",
                "https://t@h.test/p"
            ])),
            "--metrics off install https://h.test/p",
            "a global option before the command does not move the mask"
        );
        assert_eq!(
            logged_args(&argv(&["search-content", "https://a:b@c.test/d"])),
            "search-content https://a:b@c.test/d",
            "other commands are logged verbatim"
        );
        assert_eq!(
            logged_args(&argv(&[
                "search-content",
                "install",
                "https://a:b@c.test/d"
            ])),
            "search-content install https://a:b@c.test/d",
            "the word `install` in a query does not start the mask"
        );
    }

    #[test]
    fn only_the_remote_key_command_starts_the_mask() {
        // The mask starts at the pair `config remote-key`, not at either
        // word on its own: `config metrics off .` names no key at all, and
        // a search for the words `remote-key` is a query rather than a
        // credential. Either half of that pair being enough on its own
        // would redact the tail of both commands, and the log would stop
        // being readable exactly where it is asked to be.
        assert_eq!(
            logged_args(&argv(&["config", "metrics", "off", "--global", "."])),
            "config metrics off --global .",
            "a `config` command that is not `remote-key` carries no secret"
        );
        assert_eq!(
            logged_args(&argv(&["search-content", "remote-key", "src", "crates"])),
            "search-content remote-key src crates",
            "the word `remote-key` in someone else's argument is a search term"
        );
    }

    #[test]
    fn an_auth_url_never_reaches_the_action_log() {
        // The URL the auth flow is replayed with carries a one-time
        // `code`/`state` payload: the log is plain text under `.pixel/`, so
        // the value is masked and the variable's name stays readable.
        assert_eq!(
            logged_args(&argv(&[
                "flow",
                "replay",
                "claude-code-auth-flow",
                "--execute",
                "--account",
                "someone@example.com",
                "--var",
                "auth_url=https://platform.claude.com/oauth/authorize?code=secret&state=8f2a",
            ])),
            "flow replay claude-code-auth-flow --execute --account someone@example.com --var auth_url=<redacted>"
        );
        // The one-token spelling is the same option to clap and the same
        // secret to the log, so it is masked with the name kept.
        assert_eq!(
            logged_args(&argv(&[
                "flow",
                "replay",
                "claude-code-auth-flow",
                "--execute",
                "--var=auth_url=https://platform.claude.com/oauth/authorize?code=secret&state=8f2a",
            ])),
            "flow replay claude-code-auth-flow --execute --var=auth_url=<redacted>"
        );
        // A plain `--var token=…` is somebody else's variable and stays as it
        // was: only the URL this repository's own chain writes is masked.
        assert_eq!(
            logged_args(&argv(&["flow", "replay", "x", "--var", "token=kept"])),
            "flow replay x --var token=kept"
        );
    }

    const ROW_CAP: &str =
        "match list truncated at row limit 100; more matches exist — continue via next_offset";
    const BYTE_CAP: &str =
        "output truncated by the 65536-byte response cap; continue via next_offset";
    const CREDENTIAL_CAP: &str = "2 match(es) in credential-shaped files hidden by the daemon; continue via next_offset for adjacent matches";
    const RANKED_CAP: &str = "ranked candidate pool capped at 10000 matches; ranking never saw candidates beyond the cap";

    /// `cap_hits` as the daemon sends them, and the `basis` the envelope
    /// builds from the same caps.
    fn cap_hits(caps: &[(&str, &str)]) -> (String, Vec<serde_json::Value>) {
        let texts: Vec<&str> = caps.iter().map(|(_, text)| *text).collect();
        let basis = if texts.is_empty() {
            "text index".to_string()
        } else {
            format!("text index; caps: {}", texts.join("; "))
        };
        let hits = caps
            .iter()
            .map(|(kind, text)| serde_json::json!({"kind": kind, "text": text}))
            .collect();
        (basis, hits)
    }

    #[test]
    fn bounded_result_note_keeps_the_basis_when_no_truncation_was_warned() {
        // Without a ⚠ line above it, the cap text inside the note is the
        // only place the bound is named; it must stay.
        for caps in [
            &[("row_limit", ROW_CAP)][..],
            &[("credential_hidden", CREDENTIAL_CAP)][..],
        ] {
            let (basis, hits) = cap_hits(caps);
            assert_eq!(
                bounded_result_note(&basis, &hits, false).as_deref(),
                Some(basis.as_str())
            );
        }
    }

    #[test]
    fn bounded_result_note_drops_only_the_caps_the_warning_named() {
        let (basis, hits) = cap_hits(&[
            ("byte_cap", BYTE_CAP),
            ("row_limit", ROW_CAP),
            ("credential_hidden", CREDENTIAL_CAP),
            ("ranked_pool", RANKED_CAP),
        ]);
        assert_eq!(
            bounded_result_note(&basis, &hits, true).as_deref(),
            Some(format!("text index; caps: {CREDENTIAL_CAP}; {RANKED_CAP}").as_str())
        );
    }

    #[test]
    fn bounded_result_note_is_dropped_when_the_warning_said_everything() {
        for caps in [
            &[("row_limit", ROW_CAP)][..],
            &[("byte_cap", BYTE_CAP)][..],
            &[("byte_cap", BYTE_CAP), ("row_limit", ROW_CAP)][..],
            &[][..],
        ] {
            let (basis, hits) = cap_hits(caps);
            assert_eq!(bounded_result_note(&basis, &hits, true), None, "{basis}");
        }
    }

    /// A `-g`/`-t` search pages the daemon 10 000 rows at a time and warns
    /// with its own limit: the daemon's row cap names another number and is
    /// still the bound the warning stated. Matching the sentence printed it
    /// twice.
    #[test]
    fn bounded_result_note_drops_the_row_cap_whatever_limit_it_names() {
        let filtered_page = "match list truncated at row limit 10000; more matches exist — continue via next_offset";
        let (basis, hits) = cap_hits(&[("row_limit", filtered_page)]);
        assert_eq!(bounded_result_note(&basis, &hits, true), None);
    }

    /// #549: a binary run from a checkout's `target/` prints the release's
    /// version over its branch's code, so it is a developer build like
    /// `pixel-dev`; a `pixel` anywhere else is an install and keeps the
    /// upgrade notice.
    #[test]
    fn a_cargo_build_is_a_developer_build_and_an_install_is_not() {
        let dir = std::path::Path::new("/nonexistent-pixel-549");
        assert!(is_developer_build(&dir.join("pixel/target/debug/pixel")));
        assert!(is_developer_build(
            &dir.join("pixel/target/dev-release/pixel")
        ));
        assert!(is_developer_build(&dir.join(".local/bin/pixel-dev")));
        assert!(!is_developer_build(&dir.join(".local/bin/pixel")));
        assert!(!is_developer_build(
            &dir.join("mise/installs/pixel/0.6.1/bin/pixel")
        ));
    }
}

#[cfg(test)]
#[path = "main_tests/render_data_tests.rs"]
mod render_data_tests;

#[cfg(test)]
#[path = "main_tests/targets_manifest_tests.rs"]
mod targets_manifest_tests;

#[cfg(test)]
#[path = "main_tests/daemon_ping_tests.rs"]
mod daemon_ping_tests;

#[cfg(test)]
#[path = "main_tests/upgrade_target_tests.rs"]
mod upgrade_target_tests;

#[cfg(test)]
#[path = "main_tests/update_close_tests.rs"]
mod update_close_tests;

#[cfg(test)]
#[path = "main_tests/classify_setup_prompt_tests.rs"]
mod classify_setup_prompt_tests;

#[cfg(test)]
#[path = "main_tests/review_gate_pretty_tests.rs"]
mod review_gate_pretty_tests;

#[cfg(test)]
#[path = "main_tests/search_fallback_gate_tests.rs"]
mod search_fallback_gate_tests;
