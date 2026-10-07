// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! pixel CLI — index/search plus the graph command surface, speaking to a
//! per-root daemon over its Unix socket when one is up, else in-process.

use std::collections::{HashMap, HashSet};
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
// Dev tool behind the `readify` feature: probing and rewriting agents'
// CLI configs is not part of the shipped binary (issue #602).
#[cfg(feature = "readify")]
mod ai_cli_readify;
mod audit_cmd;
mod call_guard;
mod classify;
mod classify_setup;
mod config_cmd;
mod config_file;
mod coverage_cmd;
mod decide_infisical;
mod decide_jev;
mod decide_ollaya;
mod decide_remote;
mod evaluate_cmd;
mod execution_brief;
mod hook_input;
mod impact_read;
mod index_cmd;
mod install_intro;
mod operation_metrics;
mod overview_intent;
mod plan_cmd;
mod plan_state;
mod prompt_continuation;
mod prompt_intent;
mod prompt_key;
mod recall_cmd;
mod rescue_cmd;
mod search_compat;
mod search_filter;
mod serve_trace;
mod sniper_cmd;
mod space_cmd;
mod task_bridge;
mod task_commands;
mod task_config;
mod task_hook;
mod task_prepare;
mod task_route;
mod task_runtime;
use task_commands::TaskCmd;
mod ultraflow_cmd;

mod select;
mod update_notice;
mod web_search;
mod web_search_setup;
mod workspace_cmd;
use pixel_actionlog::{InProcessReason, ServeRoute, ServeStep};
use pixel_daemon::api::{PROTOCOL_VERSION, Request, Response, Service, failure_response};
use pixel_daemon::daemon;
use pixel_index::index::{build, shard_path};
use pixel_index::shard::Shard;
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
    fn threshold(self) -> pixel_install::doctor::CheckStatus {
        match self {
            Self::Yellow => pixel_install::doctor::CheckStatus::Yellow,
            Self::Red => pixel_install::doctor::CheckStatus::Red,
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
    #[command(alias = "index")]
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
    #[command(alias = "search")]
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
    #[command(alias = "search-compat")]
    SearchLikeRg {
        #[arg(value_enum)]
        tool: search_compat::SearchTool,
        #[arg(last = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Compile and execute one bounded deterministic retrieval recipe.
    #[command(alias = "query")]
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
    #[command(alias = "ask")]
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
    #[command(alias = "targets")]
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
    /// Surgical revert planner: locate the files a problem points at, list
    /// recent versions with the likely-breaking commit flagged, recommend a
    /// last-known-good candidate. Plan only — nothing is written without
    /// --apply. Never resets; never touches the index or HEAD.
    #[command(alias = "rescue")]
    PlanRollback {
        /// Problem description ("login was working before ...").
        problem: Option<String>,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Explicit target file(s), repo-relative; skips target discovery.
        #[arg(long = "file")]
        files: Vec<String>,
        /// Commits of per-file history to inspect.
        #[arg(long, default_value_t = 10)]
        depth: usize,
        /// Restore the --file targets to this commit (gated action).
        #[arg(long)]
        apply: Option<String>,
        /// With --apply on dirty files: deterministic 3-way merge that keeps
        /// in-progress edits (may leave conflict markers).
        #[arg(long)]
        merge: bool,
        /// With --apply: `git stash push` the dirty planned files first.
        #[arg(long)]
        stash_first: bool,
        /// With --apply: overwrite dirty files (loses in-progress work).
        #[arg(long)]
        allow_dirty: bool,
        #[arg(long)]
        json: bool,
    },
    /// Probe provider readiness for Codex, Claude Code, Antigravity, and
    /// Devin by sending one real `POST /chat/completions` ("Reply exactly
    /// READY.", a 1024-token reservation, 20 s timeout) to Ollama Cloud. A
    /// 200 that answers with the READY it asked for means the provider is
    /// ready; a 200 whose body reaches no model carries its own failure
    /// rather than counting as one. With
    /// `--apply`, the provider that answered is written into
    /// `~/.codex/config.toml` (`model`, `model_provider`,
    /// `[model_providers.recording_cloud]`),
    /// `~/.claude/settings.json` (`env.ANTHROPIC_BASE_URL` and the model
    /// slots), and
    /// `~/.gemini/antigravity-cli/settings.json`
    /// (`AGY_LLM_GATEWAY_URL`); Devin is verified-only, never rewritten.
    /// Claude's gateway credential and its `x-litellm-api-key` header are
    /// left to the shell — a settings file expands neither, and would
    /// replace the working value — so the report names both exports instead.
    /// With `--approve`, the workspace named by `--workspace` has its
    /// startup gate cleared as well.
    #[cfg(feature = "readify")]
    AiCliReadify {
        /// Also rewrite the agents' config files; without this flag the
        /// command only probes and verifies (safe to re-run).
        #[arg(long)]
        apply: bool,
        /// Probe timeout per provider, in seconds.
        #[arg(long, default_value_t = 20)]
        timeout: u64,
        /// Verify only this subset of agents (repeatable). Default: all
        /// four (codex, claude, antigravity, devin).
        #[arg(long = "agent", value_enum)]
        agents: Vec<ai_cli_readify::AgentFlag>,
        /// Emit a machine-readable JSON report on stdout.
        #[arg(long)]
        json: bool,
        /// Answer the startup prompts this command knows the key for, instead
        /// of reporting them and stopping. Off by default: a trust dialog is
        /// not this command's decision to take, and a prompt with no verified
        /// answer is reported either way.
        #[arg(long)]
        answer_prompts: bool,
        /// Clear each agent's own startup gate for `--workspace`: Codex's
        /// workspace trust and its hooks' trust at their current hashes
        /// (through Codex's own `config/batchWrite` RPC, never by editing
        /// `config.toml`), and Claude's onboarding and trust dialog in
        /// `~/.claude.json`. Off by default — a trust write outlives the run,
        /// and it means "run this folder's hooks and code without asking
        /// again". Antigravity and Devin have no approval path: their trust
        /// state is only ever read. `~/.claude.json` belongs to Claude Code,
        /// so close a running one first: the file is read, merged and
        /// rewritten, and a write Claude Code makes in between is lost.
        #[arg(long)]
        approve: bool,
        /// The folder `--approve` is about. Defaults to the working
        /// directory. Exactly this path is written, never a parent: a trust
        /// level granted to a directory covers everything below it, so a run
        /// that walked up would grant more than it was asked to.
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
        /// Hand a Claude lane stuck on an auth wall to the installed
        /// `claude-code-auth-flow`: spawn `claude auth login`, replay the
        /// flow with the authorize URL it prints (which drives `agent-browser`
        /// against the user's real browser profile), wait for the login to
        /// exit, then re-probe the lane once. Off by default, and it fires
        /// only for the two auth wordings — a quota, a rate limit, a
        /// transport error or a model-not-found failure leaves the browser
        /// untouched. A login that prints no authorize URL is refused rather
        /// than opened at an empty one.
        #[arg(long)]
        authenticate: bool,
        /// The account `--authenticate` should use: passed to
        /// `claude auth login --email` and to the flow's own account
        /// shortcut. Only consulted when the chain runs.
        #[arg(long)]
        account: Option<String>,
    },
    /// Look up symbols by name in the code graph.
    #[command(alias = "symbol")]
    FindSymbol {
        name: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// All signatures in a file — the skeleton view at ~10% of Read cost.
    #[command(alias = "skeleton")]
    ListSignatures {
        file: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Human notes on the map: durable annotations keyed by file + symbol
    /// name (or concept norm). Survive rebuilds; merged into `resolve` and
    /// `targets` results. `pixel note set <file> <target> <note>`,
    /// `get`/`rm <file> <target>`, `list [file]`.
    Note {
        /// set | get | rm | list
        action: String,
        /// File the note is attached to (repo-relative or absolute).
        file: Option<String>,
        /// Symbol name or concept norm the note targets.
        target: Option<String>,
        /// Note text (required for `set`).
        note: Option<String>,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Structural repo map: every indexed file with its symbols. `--markdown`
    /// emits the exportable document form — the human-editable projection of
    /// the graph that `note` annotations key onto.
    #[command(alias = "map")]
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
    #[command(alias = "context")]
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
    #[command(alias = "uses")]
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
    #[command(alias = "trace")]
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
    /// Discovered execution flows.
    #[command(alias = "processes")]
    ListFlows {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long)]
        json: bool,
    },
    /// Functional-area clusters.
    #[command(alias = "clusters")]
    ListAreas {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long)]
        json: bool,
    },
    /// Symbols/flows affected by working-tree changes.
    #[command(alias = "changes")]
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
    #[command(alias = "graph")]
    RebuildGraph {
        #[arg(default_value = ".")]
        path: PathBuf,
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
    /// Audit how much disk the pixel index (`.pixel/`) takes across every
    /// project under this tree: per-project shard size plus the accumulated
    /// total, and a one-shot `--delete` cleanup of the rebuildable shards.
    Space {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Emit structured JSON instead of the table.
        #[arg(long)]
        json: bool,
        /// Remove every found index shard after a single confirmation.
        #[arg(long)]
        delete: bool,
        /// Skip the deletion confirmation (assume yes).
        #[arg(long)]
        yes: bool,
    },
    /// Make a repository ready for agent work: index, graph, and warm daemon.
    #[command(alias = "ready")]
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
    /// Show raw shard metadata (legacy).
    #[command(alias = "stats")]
    IndexStats {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Manage the per-root background daemon.
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
    /// Search and browse LLM CLI transcripts (machine-wide corpus).
    Recall {
        #[command(subcommand)]
        cmd: recall_cmd::RecallCmd,
    },
    /// One-look error capture: query the sniper error sink.
    #[command(alias = "sniper")]
    ListErrors {
        #[command(subcommand)]
        cmd: sniper_cmd::SniperCmd,
    },
    /// Decision over a bounded label set through a remote LLM: one
    /// OpenAI-compatible chat completion maps the state, shared framing,
    /// labels and criteria onto a probability distribution — the shape a
    /// Jev-class decision model returns. Non-deterministic and
    /// network-bound; `--remote-preset` picks the provider. `--jsonl`
    /// serves one decision per stdin line.
    Classify(classify::ClassifyOptions),
    /// Deterministic web lookup for terms the index cannot know — the
    /// refine step of a gated `pixel plan`. No LLM, no daemon.
    WebSearch {
        /// The term or question to resolve.
        query: String,
        #[arg(long, default_value_t = web_search::DEFAULT_LIMIT)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    // -----------------------------------------------------------------
    // M2 — safe git mutation ops (pixel-ops)
    // -----------------------------------------------------------------
    /// Show repo state: HEAD, branch, dirty files, fingerprints.
    ///
    /// The answer is compact: the exact `dirty_count`/`clean_count` and the
    /// dirty files. The tracked-clean file list (200 paths, the bulk of the
    /// answer on a clean tree) needs `--include-clean`.
    #[command(alias = "inspect")]
    RepoState {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Restrict the snapshot to these repo-relative paths.
        #[arg(long = "files")]
        files: Vec<String>,
        /// Include the capped tracked-clean file list (default: count only).
        #[arg(long)]
        include_clean: bool,
        #[arg(long)]
        json: bool,
    },
    /// Review working-tree changes (staged, unstaged, untracked, conflicted).
    #[command(alias = "review")]
    ReviewChanges {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Pagination cursor (opaque).
        #[arg(long)]
        cursor: Option<String>,
        /// Cap output bytes.
        #[arg(long)]
        byte_cap: Option<usize>,
        #[arg(long)]
        json: bool,
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
    /// Commit history with detail levels and byte caps.
    #[command(alias = "history")]
    CommitHistory {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Ref to log (default HEAD).
        #[arg(long = "ref")]
        ref_: Option<String>,
        /// Max commits (capped at 100).
        #[arg(long)]
        limit: Option<usize>,
        /// compact (oid+subject) or full (oid+author+date+subject+body).
        #[arg(long, default_value = "compact")]
        detail: String,
        /// Pagination cursor (skip N commits).
        #[arg(long)]
        cursor: Option<String>,
        /// Cap output bytes.
        #[arg(long)]
        byte_cap: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// Structured diff between two refs (or ref → working tree).
    Diff {
        from: String,
        /// Optional target ref; if omitted, diff to working tree.
        to: Option<String>,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Restrict diff to these paths.
        #[arg(long)]
        paths: Vec<String>,
        /// Cap diff text bytes.
        #[arg(long)]
        byte_cap: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// Stage files, commit, and optionally push (crash-safe, idempotent).
    #[command(alias = "publish")]
    Commit {
        /// Commit message. Use `--message-file` for a multi-paragraph body.
        #[arg(
            short = 'm',
            long = "message",
            required_unless_present = "message_file"
        )]
        message: Option<String>,
        /// Read the commit message from this file (`-` for stdin), verbatim
        /// except for trailing whitespace. Mutually exclusive with `-m`.
        #[arg(short = 'F', long = "message-file", conflicts_with = "message")]
        message_file: Option<PathBuf>,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Files to stage (repo-relative). Repeat the flag once per file
        /// (`--files a --files b`) — a single `--files a b` does NOT work:
        /// `b` silently becomes the trailing PATH argument instead of a
        /// second file, or errors if a PATH was already given.
        #[arg(long = "files")]
        files: Vec<String>,
        /// Also push after committing.
        #[arg(long)]
        push: bool,
        /// Amend the current commit instead of creating a new one.
        #[arg(long)]
        amend: bool,
        /// Reject if HEAD does not match this OID.
        #[arg(long)]
        expected_head: Option<String>,
        /// Idempotency / recovery key.
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Leased push to a remote (crash-safe, idempotent).
    Push {
        remote: String,
        refspec: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        force_with_lease: bool,
        /// Idempotency / recovery key.
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Publish + push in one op (commit then leased push).
    #[command(alias = "ship")]
    CommitAndPush {
        /// Commit message. Use `--message-file` for a multi-paragraph body.
        #[arg(
            short = 'm',
            long = "message",
            required_unless_present = "message_file"
        )]
        message: Option<String>,
        /// Read the commit message from this file (`-` for stdin), verbatim
        /// except for trailing whitespace. Mutually exclusive with `-m`.
        #[arg(short = 'F', long = "message-file", conflicts_with = "message")]
        message_file: Option<PathBuf>,
        // Positional order matches Push: required remote + refspec first,
        // then the defaulted path. (A defaulted positional BEFORE required
        // ones trips clap's debug assertions — every debug-build parse
        // panicked before this reorder.)
        remote: String,
        refspec: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Files to stage (repo-relative). Repeat the flag once per file
        /// (`--files a --files b`) — a single `--files a b` does NOT work:
        /// `b` silently becomes the trailing PATH argument instead of a
        /// second file, or errors if a PATH was already given.
        #[arg(long = "files")]
        files: Vec<String>,
        /// Leased force-push (`--force-with-lease`) for the push phase.
        #[arg(long)]
        force_with_lease: bool,
        /// Idempotency / recovery key.
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Create a new branch from HEAD (or --from <ref>).
    #[command(alias = "branch")]
    NewBranch {
        name: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Base ref (default HEAD).
        #[arg(long)]
        from: Option<String>,
        /// Idempotency / recovery key.
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Fast-forward merge to a target OID (refuses non-ff + dirty intersection).
    #[command(alias = "update")]
    FastForward {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Reject if HEAD does not match this OID.
        #[arg(long)]
        expected_head: String,
        /// Fast-forward target OID.
        #[arg(long)]
        target_oid: String,
        /// Idempotency / recovery key.
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Fetch from a remote (idempotent).
    #[command(alias = "sync")]
    Fetch {
        remote: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Optional refspec.
        #[arg(long)]
        refspec: Option<String>,
        #[arg(long)]
        json: bool,
    },
    // -----------------------------------------------------------------
    // M3/M4 — engines (resolve, history, lifecycle, excavate, reconcile)
    // -----------------------------------------------------------------
    /// Engine 1: resolve a phrase to code via the concept index.
    #[command(alias = "resolve")]
    FindCode {
        phrase: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// M3: history-wide fact + diff search.
    #[command(alias = "history-search")]
    SearchHistory {
        query: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// message | path | diff | all
        #[arg(long, default_value = "all")]
        facet: String,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// Engine 2: lifecycle of a path or token.
    #[command(alias = "lifecycle")]
    FileHistory {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Repo-relative path to inspect.
        #[arg(long)]
        file: Option<String>,
        /// Token to inspect.
        #[arg(long)]
        token: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Engine 2: history-wide discovery (rescue v2).
    #[command(alias = "excavate")]
    DigHistory {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Phrase to search for in diff text.
        #[arg(long)]
        phrase: Option<String>,
        /// Restrict to a repo-relative path (with --show: the path to read).
        #[arg(long)]
        file: Option<String>,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
        /// Print the FULL content of --file at this commit (safe
        /// `git show <oid>:<path>` equivalent). When the file does not
        /// exist at <oid> but does at <oid>^ — i.e. <oid> is the deletion
        /// commit — the parent's pre-deletion content is returned and
        /// flagged. One pixel call replaces the `git show` follow-ups.
        #[arg(long, value_name = "OID")]
        show: Option<String>,
        /// With --show: read from the commit's first parent (<oid>^)
        /// directly, skipping the read at <oid> itself.
        #[arg(long)]
        parent: bool,
        #[arg(long)]
        json: bool,
    },
    /// Engine 4: one-call deterministic branch sync.
    #[command(alias = "reconcile")]
    SyncBranch {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// report (default) | rebase-if-clean
        #[arg(long, default_value = "report")]
        strategy: String,
        /// auto (default) | none
        #[arg(long, default_value = "auto")]
        push: String,
        /// Integrate: rebase current branch onto origin/<TARGET>, then
        /// fast-forward local <TARGET> to the rebased head (never merge).
        /// `--strategy` is ignored in this mode.
        #[arg(long, value_name = "TARGET")]
        into: Option<String>,
        /// Idempotency / recovery key.
        #[arg(long)]
        request_id: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// M5: journal a session event (fire-and-forget).
    #[command(alias = "journal")]
    RecordEvent {
        kind: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Repo-relative path the event concerns.
        #[arg(long)]
        file: Option<String>,
        #[arg(long)]
        detail: Option<String>,
        #[arg(long)]
        json: bool,
    },
    // -----------------------------------------------------------------
    // M5/M6 — install / doctor / migrate / hook
    // -----------------------------------------------------------------
    /// Idempotently deploy agent integrations while preserving native retrieval.
    Install {
        #[arg(long)]
        json: bool,
        /// Shell whose profile is checked for the retired `claude()` wrapper
        /// (default: account login shell, then $SHELL). Pass e.g. `fish`
        /// to select the profile explicitly.
        #[arg(long)]
        shell: Option<String>,
        /// Install project-local integrations into this repository only,
        /// skipping every global step. Remove retired Claude and Codex
        /// retrieval hooks, restore adopted RTK/foreign hooks from matching
        /// backups, and preserve independent task controls. Tracked Codex
        /// hooks are left alone. Remove the retired Devin guard and Pi project
        /// extension, and retired Pixel retrieval blocks from AGENTS.md and
        /// Codex config. Adds no guard. Foreign instructions and native tools
        /// are preserved.
        /// Files: `.claude/settings.local.json`, `.claude/pixel-rtk-hooks.json`,
        /// `.codex/config.toml`, `.codex/hooks.json`,
        /// `.codex/pixel-composed-guard-backup.json`, and `AGENTS.md`.
        /// Machine-specific files naming this binary go into `info/exclude`.
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Remove everything `pixel install` wrote: managed blocks from
    /// agent-config files, hook entries from all settings files, hook
    /// scripts, the Pi impact package, the rule source file, and the
    /// pixel binary itself. Idempotent: safe to re-run.
    Uninstall {
        #[arg(long)]
        json: bool,
        /// Preview what would be removed without making any changes.
        #[arg(long)]
        dry_run: bool,
        /// Path to the pixel binary to remove (default: the running binary,
        /// left in place when Homebrew or mise installed it).
        #[arg(long)]
        binary_path: Option<PathBuf>,
        /// Shell whose wrapper block should be removed (default: the
        /// account's login shell, then $SHELL).
        #[arg(long)]
        shell: Option<String>,
        /// Remove only that shell's wrapper block and keep everything else
        /// installed: the fix for a block `pixel doctor` reports in a
        /// profile the login shell never loads.
        #[arg(long)]
        wrappers_only: bool,
        /// Remove only the project-local artifacts `pixel install --repo`
        /// wrote in this repository. Skips all global removal steps.
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Check that a release tag is consistent with the tree before anything
    /// is built or published: crates/pixel/Cargo.toml carries the version,
    /// Cargo.lock is fresh for every workspace member, CHANGELOG.md has the
    /// `## [x.y.z]` heading and an empty Unreleased section. Exit 1 on any
    /// failed check.
    #[command(alias = "release-check")]
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
    #[command(alias = "upgrade")]
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
        /// Shell whose wrapper block should be checked (default: $SHELL).
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
    /// Removed: the legacy `.gitpixel/` migration. Hidden and kept only so a
    /// script that still calls it exits 0 with a note instead of failing
    /// with "unrecognized subcommand".
    #[command(hide = true)]
    Migrate {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Host hook entry points (task-event) registered by `pixel install`.
    #[command(alias = "hook")]
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
    #[command(alias = "task")]
    TaskState {
        #[command(subcommand)]
        cmd: TaskCmd,
    },
    /// Self-assessment: pixel's own action log (what ran, what went wrong).
    /// Reads <path>/.pixel/actions.jsonl, written asynchronously by every
    /// pixel invocation.
    #[command(alias = "log")]
    ActionLog {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Most recent entries to show.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Only show entries that ended in an error.
        #[arg(long)]
        errors_only: bool,
        #[arg(long)]
        json: bool,
        /// Delete the action log for this root and exit.
        #[arg(long)]
        clear: bool,
    },
    /// Token-savings report: for retrieval-shaped commands (search/query/
    /// context/resolve) that recorded snippet-vs-pool volumes, aggregate the
    /// fraction of the candidate pool the agent did NOT have to read. A
    /// measured counter to semble's '99% fewer tokens' claim — own numbers,
    /// same format.
    #[command(alias = "savings")]
    TokenSavings {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
        /// Only consider events from the last N hours.
        #[arg(long)]
        since_hours: Option<u64>,
    },
    /// Squash every commit on the current branch since its base into ONE
    /// commit (crash-safe, backup-ref'd), optionally force-pushing with lease.
    #[command(alias = "rewrite")]
    SquashBranch {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Explicit base ref (squash <onto>..HEAD). Default: merge-base with
        /// the branch upstream, else with the remote default branch.
        #[arg(long)]
        onto: Option<String>,
        /// Squash commit message (default: auto-generated subject list).
        #[arg(short = 'm', long = "message")]
        message: Option<String>,
        /// Push the rewritten branch with --force-with-lease afterwards.
        #[arg(long)]
        push: bool,
        /// Remote name.
        #[arg(long, default_value = "origin")]
        remote: String,
        /// Reject if HEAD does not match this OID.
        #[arg(long)]
        expected_head: Option<String>,
        /// Allow rewriting the default branch and published mainline commits.
        /// Overrides both default-branch and published-mainline protection.
        #[arg(long)]
        allow_default_branch: bool,
        /// Idempotency / recovery key.
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Per-region blame attribution: who introduced/owns each region of a file.
    #[command(alias = "provenance")]
    WhoWrote {
        /// Repo-relative file to attribute.
        file: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Restrict to lines a,b (1-based inclusive), e.g. --lines 10,40.
        #[arg(long, value_parser = parse_line_range)]
        lines: Option<(u32, u32)>,
        /// Author query (case-insensitive substring on name or email) —
        /// adds a did-they-touch-this verdict.
        #[arg(long)]
        author: Option<String>,
        /// Max regions emitted (default 200); truncation sets lower_bound.
        #[arg(long, default_value_t = 200)]
        limit_regions: usize,
        #[arg(long)]
        json: bool,
    },
    /// One-call read-only branch inventory: ahead/behind, merged, stale,
    /// unpushed — the deterministic "did you push everything?" answer.
    #[command(alias = "branches")]
    ListBranches {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Run `git fetch --prune <remote>` first for a live view.
        #[arg(long)]
        fetch: bool,
        /// Remote name.
        #[arg(long, default_value = "origin")]
        remote: String,
        /// Days after which a branch counts as stale.
        #[arg(long, default_value_t = 30)]
        stale_days: u64,
        #[arg(long)]
        json: bool,
    },
    /// Additive-only, key-level .env mutations with snapshots and restore.
    /// Values are NEVER printed in any output.
    #[command(alias = "env")]
    EditEnv {
        #[command(subcommand)]
        cmd: EnvCmd,
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
    /// Drive a browser toward a goal with `pixel classify` one operation at
    /// a time, save what worked as a `pixel flow`, and replay that saved
    /// document later — its branch conditions decided by `pixel classify`
    /// instead of matched text.
    ///
    /// `discover` is the jev-style loop: one question per cycle whose
    /// options are the operation-target pairs the page currently offers.
    /// `replay` follows the composed document, and re-decides a step whose
    /// page moved on, recording the new branch with `--update`.
    Ultraflow(ultraflow_cmd::UltraflowOptions),
    /// Save, retrieve, list, revise, run, and replay proven agent-browser
    /// paths (auth flows, config flows) so the agent follows a deterministic
    /// shortcut instead of re-discovering the UI from scratch every time.
    /// For a classify-decided replay with repair, use `pixel ultraflow replay`.
    #[command(alias = "replay-flow")]
    Flow {
        #[command(subcommand)]
        cmd: FlowCmd,
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

#[derive(Subcommand)]
enum EnvCmd {
    /// List .env files under root — key NAMES only, never values.
    Inventory {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Set one key (snapshot-first; every other line byte-preserved).
    Set {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        key: String,
        #[arg(long)]
        value: String,
        /// Create the file if it does not exist.
        #[arg(long)]
        create_file: bool,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Restore from a snapshot (latest if --snapshot omitted; undoable).
    Restore {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        snapshot: Option<String>,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// List snapshots recorded for a file.
    Snapshots {
        #[arg(long)]
        file: PathBuf,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Verify required keys exist (names only).
    Check {
        #[arg(long)]
        file: PathBuf,
        /// Required key name; repeat the flag once per key.
        #[arg(long = "require")]
        require: Vec<String>,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum FlowCmd {
    /// Create a new flow. Refuses to overwrite — use `revise` to update.
    Save {
        /// Flow name (kebab-case recommended, e.g. "github-auth-device-flow").
        name: String,
        #[arg(long)]
        title: String,
        #[arg(long, default_value = "")]
        description: String,
        /// Tag; repeat the flag once per tag (`--tag auth --tag github`).
        #[arg(long = "tag")]
        tags: Vec<String>,
        #[arg(long)]
        url: Option<String>,
        /// Path to a JSON file containing the steps array.
        #[arg(long)]
        from_file: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Retrieve a flow by name (for the agent to follow deterministically).
    Get {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// List all saved flows, optionally filtered by tag.
    List {
        #[arg(long)]
        tag: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Update an existing flow's metadata and/or steps. Bumps revision.
    Revise {
        name: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        description: Option<String>,
        /// Path to a JSON file containing the new steps array.
        #[arg(long)]
        from_file: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Run a flow by driving agent-browser (the plain, deterministic
    /// executor: refs resolved from fresh snapshots, conditions matched as
    /// text). For a classify-decided replay with repair, use
    /// `pixel ultraflow replay`.
    Run {
        name: String,
        /// Variable substitution: `--var key=value`. Repeat per var.
        #[arg(long = "var")]
        vars: Vec<String>,
        /// Shortcut for `--var google_account=<value>` (or `openai_account`
        /// depending on the flow). Picks which account to use.
        /// Accepts a full email address (e.g. user@example.com).
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Emit ready-to-run agent-browser commands with variable substitution.
    /// Pixel does NOT run agent-browser — it outputs the deterministic
    /// command sequence for the agent to execute (or to hand to
    /// `pixel flow run`).
    Replay {
        name: String,
        /// Variable substitution: `--var key=value`. Repeat per var.
        #[arg(long = "var")]
        vars: Vec<String>,
        /// Shortcut for `--var google_account=<value>` (or `openai_account`
        /// depending on the flow). Picks which account to use.
        /// Accepts a full email address (e.g. user@example.com).
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Delete a flow by name.
    Delete {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Pretty-print the full flow document (human-readable).
    Show {
        name: String,
        #[arg(long)]
        json: bool,
    },
}

/// The variables a flow verb carries: `--var key=value` pairs, plus the
/// `--account` shortcut resolved to whichever account variable the flow
/// declares (`openai_account` for Codex, `google_account` for the rest).
fn flow_vars(
    name: &str,
    vars: &[String],
    account: &Option<String>,
) -> Result<std::collections::HashMap<String, String>, String> {
    let mut var_map = std::collections::HashMap::new();
    for v in vars {
        let (k, val) = v
            .split_once('=')
            .ok_or_else(|| format!("--var expects key=value, got '{v}'"))?;
        var_map.insert(k.to_string(), val.to_string());
    }
    if let Some(acct) = account {
        // Check which var the flow expects by loading it.
        let var_name = pixel_flow::load(name)
            .ok()
            .and_then(|f| {
                f.vars.iter().find_map(|v| {
                    (v.name == "openai_account" || v.name == "google_account")
                        .then(|| v.name.clone())
                })
            })
            .unwrap_or_else(|| "google_account".to_string());
        var_map.insert(var_name, acct.clone());
    }
    Ok(var_map)
}

/// Parse a 1-based inclusive line range "a,b" for `provenance --lines`.
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

fn parse_line_range(s: &str) -> Result<(u32, u32), String> {
    let (a, b) = s
        .split_once(',')
        .ok_or_else(|| format!("expected 'start,end', got '{s}'"))?;
    let a: u32 = a
        .trim()
        .parse()
        .map_err(|e| format!("bad start line: {e}"))?;
    let b: u32 = b.trim().parse().map_err(|e| format!("bad end line: {e}"))?;
    if a == 0 || b < a {
        return Err(format!("invalid range {a},{b}: need 1 <= start <= end"));
    }
    Ok((a, b))
}

#[derive(Subcommand)]
enum HookCmd {
    /// Durable task lifecycle and completion gate for supported agent hosts.
    TaskEvent {
        #[arg(long, value_enum)]
        provider: task_hook::TaskProvider,
        #[arg(long, value_enum)]
        event: task_hook::TaskHookEvent,
    },
    #[command(flatten)]
    Retired(RetiredHookCmd),
}

/// Hook verbs that older releases registered in agent settings. Each one is
/// accepted and does nothing; the per-prompt brief and the task gates replaced
/// them.
#[derive(Subcommand)]
enum RetiredHookCmd {
    /// Retired: the guard.
    #[command(hide = true)]
    Guard(RetiredHook),
    /// Retired: the sealed foreign-hook composition of the Codex guard.
    #[command(hide = true)]
    ComposedGuard(RetiredHook),
    /// Retired: the capability block, replaced by the installed agent prompt.
    #[command(hide = true)]
    SessionStart(RetiredHook),
    /// Retired: the prompt-time task context, replaced by the per-prompt brief.
    #[command(hide = true)]
    PromptSubmit(RetiredHook),
    /// Retired: the targets re-injection after context compaction.
    #[command(hide = true)]
    PostCompaction(RetiredHook),
    /// Retired: the blast-radius note after an edit.
    #[command(hide = true)]
    PostToolUse(RetiredHook),
    /// Retired: the hook relay of the metrics line.
    #[command(hide = true)]
    Metrics(RetiredHook),
}

/// The arguments of a retired hook verb: whatever an older release wrote into
/// a settings file (`--provider`, `--backup`, `--delegate-rtk`, a path) is
/// accepted and ignored. A verb that stopped parsing would exit 2, and a host
/// treats a `PreToolUse` hook that exits 2 as a block on the tool call.
#[derive(clap::Args)]
#[command(disable_help_flag = true)]
struct RetiredHook {
    #[arg(num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    _ignored: Vec<std::ffi::OsString>,
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Guided global setup in a terminal; also offered by pixel install.
    Setup,
    /// Enable or disable all classify calls, including explicit --engine flags.
    Classify {
        #[arg(value_parser = ["on", "off"])]
        value: String,
    },
    /// Open the global YAML configuration in $VISUAL or $EDITOR (default: vi).
    Edit {
        /// Edit repository overrides instead of global settings.
        #[arg(long)]
        repo: bool,
        #[arg(default_value = ".")]
        path: PathBuf,
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
    /// Retired retrieval policy: no hook reads it any more. `pixel config
    /// policy` reports the effective setting and the layer that set it
    /// (environment, repository, global, or the advisory default); `advisory`,
    /// `enforce` or `off` persists it to `<root>/.pixel/config.yaml` — or
    /// `~/.pixel/config.yaml` with `--global`.
    Policy {
        /// New value; omit to report the effective setting.
        #[arg(value_enum)]
        value: Option<config_cmd::PolicyMode>,
        /// Write to the machine-wide configuration.
        #[arg(long)]
        global: bool,
        /// Print the effective setting as JSON, for the Pi extension.
        #[arg(long)]
        json: bool,
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
        /// opencode-go, jev, local).
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
    /// remote engine on the `jev` preset), or `auto` (probe local, fall
    /// back to remote — the default). `pixel install` sets this when you
    /// choose an engine.
    ClassifyEngine {
        /// The engine preference to store.
        #[arg(value_parser = ["local", "remote", "jev", "auto"])]
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
        if !config_cmd::feature_enabled(Some(root), "daemon_auto_start", "PIXEL_DAEMON_AUTO_START")
        {
            return Err(InProcessReason::AutoStartDisabled);
        }
        let exe = std::env::current_exe().map_err(|_| InProcessReason::StartFailed)?;
        let abs = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let mut command = std::process::Command::new(exe);
        command
            .arg("daemon")
            .arg("start")
            .arg(&abs)
            .arg("--foreground")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0);
        command.spawn().map_err(|_| InProcessReason::StartFailed)?;
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
#[derive(Debug, PartialEq, Eq)]
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

/// Check if an env var is explicitly set to "0"/"false"/"off".
fn env_flag_off(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| matches!(v.as_str(), "0" | "false" | "off"))
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
    match send_to_daemon(&root, &Request::TargetsFacts { task, limit }) {
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
/// (`list-errors last --json`) answers for its own flag, and a command with no
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
/// compatible for two minor versions (`docs/design/evaluate.md`,
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

fn extractor_for_shard(shard: &Shard) -> Result<Box<dyn GramExtractor>, String> {
    let id = shard.extractor_id();
    if id == "trigram" {
        return Ok(Box::new(TrigramExtractor));
    }
    if let Some(rest) = id.strip_prefix("sparse-crc32-")
        && let Some((min, max)) = rest.split_once('-')
        && let (Ok(min), Ok(max)) = (min.parse::<usize>(), max.parse::<usize>())
    {
        return Ok(Box::new(SparseGramExtractor::with_lengths(
            Crc32Weigher,
            min,
            max,
        )));
    }
    Err(format!(
        "index built with unsupported extractor {id:?}; re-run `pixel build-index`"
    ))
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
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let abs = path
        .canonicalize()
        .map_err(|e| format!("bad path {}: {e}", path.display()))?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("daemon")
        .arg("start")
        .arg(&abs)
        .arg("--foreground")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Detach from the caller's process group so terminal/agent supervisors
    // do not tear down the daemon when the short-lived start command exits.
    command.process_group(0);
    command.spawn().map_err(|e| format!("spawn daemon: {e}"))?;
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
        .join(pixel_install::config::PIXEL_DEV_EXECUTABLE)
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
/// `pixel-dev` side build ([`pixel_install::config::is_side_build`]). Such a
/// build carries its branch's prompts under the release's version, so
/// whatever a release install owns (the deployed prompts, the upgrade
/// notice) is not its to judge.
pub(crate) fn is_developer_build(exe: &Path) -> bool {
    is_cargo_target_path(exe) || pixel_install::config::is_side_build(exe)
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

/// Drop the tracked-clean file list from a `repo-state` answer.
///
/// On a clean tree the list is the bulk of the answer (200 of 351 paths,
/// ~7 KB measured on this repository) and no consumer reads it: `repo-state`
/// answers "HEAD + branch + dirty" and `clean_count` is what says how clean
/// the rest of the tree is. `--include-clean` asks for the full, capped list.
fn compact_repo_state(data: &mut Value) {
    if let Some(obj) = data.as_object_mut() {
        obj.remove("clean");
        obj.remove("clean_list_cap");
        obj.remove("clean_list_truncated");
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

/// What `pixel migrate` prints now that the command does nothing.
const MIGRATE_REMOVED_NOTE: &str = "note: 'migrate' was removed and does nothing; pixel no longer \
     reads legacy .gitpixel/ state, so delete that directory by hand if it is still there";

/// The pre-rename subcommand this invocation was spelled with, and its
/// current name. Only the first word that is not an option names the
/// command (`pixel prepare-repo ready` runs `prepare-repo` on a path called
/// `ready`); `--metrics` is the one option taking a separate value before it.
fn renamed_invocation(argv: &[String]) -> Option<(&str, &'static str)> {
    let mut words = argv.iter().skip(1);
    while let Some(word) = words.next() {
        if word == "--metrics" {
            words.next();
        } else if !word.starts_with('-') {
            return pixel_proto::commands::renamed_to(word).map(|new| (word.as_str(), new));
        }
    }
    None
}

/// The arguments as the action log records them. An API key is a secret:
/// the log is plain text under `.pixel/`, so every positional after
/// `config remote-key <preset>` is masked.
///
/// The auth flow's URL variable is masked for the same reason: its
/// `code`/`state` query is a bearer token for one login, and
/// `ai-cli-readify --authenticate` is not the only writer — a `pixel flow
/// replay` typed by hand would land here too. The name stays readable, the
/// value never does.
fn logged_args(args: &[String]) -> String {
    let secret_from = args
        .windows(2)
        .position(|pair| pair[0] == "config" && pair[1] == "remote-key")
        .map(|at| at + 3);
    #[cfg(feature = "readify")]
    let url_name = ai_cli_readify::auth::AUTH_URL_VAR;
    #[cfg(not(feature = "readify"))]
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
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The one stderr line announcing that an old command name was used, or
/// `None` when the name is current or the stream is protected (hooks,
/// `search-like-rg`, the statusline), so the note never lands in a hook
/// response or a byte-compatible rg output. The note is not a metrics line:
/// `--metrics off`, `PIXEL_METRICS=0` and the persistent `metrics` opt-out
/// silence reporting, not the rename teaching signal.
fn rename_note(argv: &[String], unprotected: bool) -> Option<String> {
    if !unprotected {
        return None;
    }
    let (old, new) = renamed_invocation(argv)?;
    Some(format!(
        "note: '{old}' is now '{new}'; the old name stays accepted until {}\n",
        pixel_proto::commands::ALIAS_REMOVAL_VERSION
    ))
}

/// Whether this invocation checks the prompts `pixel install` deployed:
/// every command but the protected streams, the three that already deal
/// with them (`install` rewrites them, `doctor` reports them, `uninstall`
/// removes them), and any command of a developer build
/// ([`is_developer_build`]: `pixel-dev`, or a binary run from `target/`).
/// The deployed prompts are the managed `pixel`'s: a build from another
/// commit differs from them by construction while printing the same
/// version, and the fix the note names, `pixel install`, would hand the
/// whole machine's hooks and prompts to that build (#549).
fn checks_deployed_prompts(command_label: &str, protected: bool, developer_build: bool) -> bool {
    !protected && !developer_build && !matches!(command_label, "install" | "doctor" | "uninstall")
}

/// The command labels the update close may relaunch. The relaunch executes
/// the command again, so only the read-only surface — retrieval, graph
/// queries, git and pixel reporting — is ever offered; a mutating command
/// (`push`, `install`, `scope-task --clear`, ...) finishes and stays
/// finished, and its notice stays informative without a prompt. Labels are
/// clap's kebab-case subcommand names, the same strings
/// `matches.subcommand_name()` reports.
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
    "index-stats",
    "recall",
    "list-errors",
    "repo-state",
    "review-changes",
    "commit-history",
    "diff",
    "file-history",
    "search-history",
    "dig-history",
    "who-wrote",
    "list-branches",
    "token-savings",
];

/// Whether the parsed invocation may be executed again by the relaunch:
/// the top-level label and every nested mode must be read-only. A mutating
/// nested mode (`recall index` ingests, `list-errors gc` applies
/// retention) or a flag that reaches outward (`list-branches --fetch`
/// runs `git fetch --prune`) makes the invocation mutating.
fn read_only_invocation(matches: &ArgMatches) -> bool {
    let Some(label) = matches.subcommand_name() else {
        return false;
    };
    if !READ_ONLY_COMMANDS.contains(&label) {
        return false;
    }
    match matches.subcommand() {
        Some(("recall", nested)) => nested.subcommand_name() != Some("index"),
        Some(("list-errors", nested)) => nested.subcommand_name() != Some("gc"),
        Some(("list-branches", nested)) => !nested.get_flag("fetch"),
        _ => true,
    }
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

/// One stderr line naming the prompts an earlier `pixel install` deployed:
/// this release deploys none, and nothing outside `pixel doctor` said so.
fn stale_prompt_note(stale: &[&str]) -> Option<String> {
    if stale.is_empty() {
        return None;
    }
    let (verb, pronoun) = if stale.len() == 1 {
        ("is", "it")
    } else {
        ("are", "them")
    };
    Some(format!(
        "note: {} deployed by an earlier `pixel install` {verb} retired in this pixel ({}) — run `pixel install` to remove {pronoun}\n",
        stale.join(" and "),
        env!("CARGO_PKG_VERSION")
    ))
}

/// What a missing `--request-id` means, for an agent that copied a command
/// without it: clap's bare "required arguments were not provided" left one
/// retrying the same line, or moving on as if the branch existed.
const REQUEST_ID_TIP: &str = "mutation ops need --request-id <id>: any stable string naming this operation, passed unchanged on a retry so pixel replays its result instead of running it twice";

/// `error` with [`REQUEST_ID_TIP`] as its `tip:` line when the argument it
/// reports missing is `--request-id`; any other error is returned untouched.
fn with_request_id_tip(mut error: clap::Error) -> clap::Error {
    use clap::error::{ContextKind, ContextValue, ErrorKind};
    let lacks_request_id = error.kind() == ErrorKind::MissingRequiredArgument
        && matches!(
            error.get(ContextKind::InvalidArg),
            Some(ContextValue::Strings(missing))
                if missing.iter().any(|arg| arg.starts_with("--request-id"))
        );
    if lacks_request_id {
        error.insert(
            ContextKind::Suggested,
            ContextValue::StyledStrs(vec![REQUEST_ID_TIP.into()]),
        );
    }
    error
}

fn run() -> Result<(), String> {
    let started = std::time::Instant::now();
    let argv: Vec<String> = std::env::args().collect();
    let matches = Cli::command()
        .try_get_matches()
        .unwrap_or_else(|error| with_request_id_tip(error).exit());
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
            | Command::ListErrors {
                cmd: sniper_cmd::SniperCmd::Run { .. }
            }
    );
    let root = discover_root(&path).or_else(|_| discover_root(Path::new(".")));
    // `--metrics=off` and `PIXEL_METRICS=0` veto one invocation; the
    // `metrics` key in `.pixel/config.json` (repo, then `~/.pixel/` global)
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
    // After `begin`, which zeroes the byte counters: both notes are rendered
    // output the caller reads.
    if let Some(note) = rename_note(&argv, !protected) {
        eprint!("{note}");
    }
    let developer_build = std::env::current_exe().is_ok_and(|exe| is_developer_build(&exe));
    if checks_deployed_prompts(&command_label, protected, developer_build)
        && let Some(home) = std::env::var_os("HOME")
        && let Some(note) =
            stale_prompt_note(&pixel_install::install::retired_prompts(Path::new(&home)))
    {
        eprint!("{note}");
    }
    // Compatibility fallback must exec the original before any logging changes
    // its search corpus; its successful Pixel branch retains existing logging.
    let mut logger = match &root {
        // The compatibility search and the retired hook verbs exit through
        // their own path before any event is logged; spawning the writer
        // would only create `.pixel` in unindexed repos, which the search
        // then mistakes for an index.
        _ if matches!(
            &cli.command,
            Command::SearchLikeRg { .. }
                | Command::RunHook {
                    cmd: HookCmd::Retired(_)
                }
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
                .map(pixel_install::install::stable_exe_path),
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
                let manifest_path = root
                    .join(pixel_index::index::SHARD_DIR)
                    .join("targets.json");
                if manifest_path.exists() {
                    std::fs::remove_file(&manifest_path)
                        .map_err(|e| format!("remove {}: {e}", manifest_path.display()))?;
                    println!("targets manifest cleared");
                } else {
                    println!("no active targets manifest");
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
                    },
                    false,
                )?
            };
            let active_tasks = if no_manifest || read_only {
                None
            } else {
                Some(write_targets_manifest(&manifest_path, &task, &data)?)
            };
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
        Command::PlanRollback {
            problem,
            path,
            files,
            depth,
            apply,
            merge,
            stash_first,
            allow_dirty,
            json,
        } => {
            let root = discover_root(&path)?;
            if let Some(oid) = apply {
                let result = rescue_cmd::apply(
                    &root,
                    &oid,
                    &files,
                    &rescue_cmd::ApplyOptions {
                        merge,
                        stash_first,
                        allow_dirty,
                    },
                )?;
                if json {
                    return print_data(&result, true);
                }
                if let Some(applied) = result["files"].as_array() {
                    for f in applied {
                        println!(
                            "{}: {}{}",
                            f["path"].as_str().unwrap_or("?"),
                            f["action"].as_str().unwrap_or("?"),
                            f["conflicts"]
                                .as_i64()
                                .filter(|c| *c > 0)
                                .map(|c| format!(" ({c} conflict hunk(s) — resolve the markers)"))
                                .unwrap_or_default(),
                        );
                    }
                }
                println!("{}", result["note"].as_str().unwrap_or(""));
                return Ok(());
            }
            let problem = problem.ok_or_else(|| "missing problem description".to_string())?;
            // Locate targets: explicit --file hints win; otherwise the sniper
            // target engine points the problem at files (P0 slice).
            let (target_paths, keywords) = if files.is_empty() {
                let data = execute(
                    &path,
                    Request::Targets {
                        task: problem.clone(),
                        limit: Some(10),
                        max_tier: None,
                        precision: false,
                    },
                    false,
                )?;
                let all = data["targets"].as_array().cloned().unwrap_or_default();
                let mut paths: Vec<String> = all
                    .iter()
                    .filter(|t| t["tier"] == "P0")
                    .filter_map(|t| t["path"].as_str().map(str::to_string))
                    .take(5)
                    .collect();
                if paths.is_empty() {
                    paths = all
                        .iter()
                        .filter_map(|t| t["path"].as_str().map(str::to_string))
                        .take(5)
                        .collect();
                }
                let kws: Vec<String> = data["keywords"]
                    .as_array()
                    .map(|ks| {
                        ks.iter()
                            .filter_map(|k| k.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                (paths, kws)
            } else {
                let q = pixel_rank::tokenize_task(&problem).unwrap_or_default();
                (files.clone(), q.keywords)
            };
            if target_paths.is_empty() {
                return Err(
                    "could not locate target files for this problem — pass --file <path>"
                        .to_string(),
                );
            }
            let plan = rescue_cmd::plan(&root, &problem, &target_paths, &keywords, depth)?;
            if json {
                return print_data(&plan, true);
            }
            for t in plan["targets"].as_array().cloned().unwrap_or_default() {
                println!(
                    "{}{}",
                    t["path"].as_str().unwrap_or("?"),
                    if t["dirty"].as_bool().unwrap_or(false) {
                        "  [DIRTY — has uncommitted changes]"
                    } else {
                        ""
                    }
                );
                for v in t["versions"].as_array().cloned().unwrap_or_default() {
                    println!(
                        "  {}  {}{}",
                        v["short"].as_str().unwrap_or("?"),
                        v["subject"].as_str().unwrap_or(""),
                        if v["suspect"].as_bool().unwrap_or(false) {
                            "  [SUSPECT]"
                        } else {
                            ""
                        }
                    );
                }
                if let Some(rec) = t["recommended"].as_object() {
                    println!(
                        "  → recommended: {} ({})",
                        rec.get("oid").and_then(Value::as_str).unwrap_or("?"),
                        rec.get("reason").and_then(Value::as_str).unwrap_or(""),
                    );
                }
                println!();
            }
            for c in plan["decision"]["caveats"]
                .as_array()
                .cloned()
                .unwrap_or_default()
            {
                eprintln!("⚠ {}", c.as_str().unwrap_or(""));
            }
            if let Some(cmd) = plan["decision"]["options"][0]["command"].as_str() {
                println!("revert: {cmd}");
            }
            println!("fix forward: keep current code and fix the bug in place");
            Ok(())
        }
        #[cfg(feature = "readify")]
        Command::AiCliReadify {
            apply,
            timeout,
            agents,
            json,
            answer_prompts,
            approve,
            workspace,
            authenticate,
            account,
        } => {
            let opts = ai_cli_readify::Options {
                apply,
                timeout: Duration::from_secs(timeout),
                agents: agents
                    .into_iter()
                    .map(ai_cli_readify::Agent::from)
                    .collect(),
                answer_prompts,
                approve,
                workspace,
                authenticate,
                account,
            };
            let report = ai_cli_readify::run(&opts)?;
            if json {
                let value = serde_json::to_value(&report).map_err(|e| e.to_string())?;
                print_data(&value, true)
            } else {
                ai_cli_readify::print_report(&report);
                Ok(())
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
        Command::Note {
            action,
            file,
            target,
            note,
            path,
            json,
        } => {
            let data = execute(
                &path,
                Request::Note {
                    action: action.clone(),
                    file,
                    target,
                    note,
                },
                false,
            )?;
            finish_graph_cmd(data, json, |d| {
                if let Some(notes) = d.get("notes").and_then(Value::as_array) {
                    // `note list` output: grouped by file.
                    if notes.is_empty() {
                        return Some("no notes\n".to_string());
                    }
                    let mut output = String::new();
                    for n in notes {
                        output.push_str(&format!(
                            "{}  {}  {}\n",
                            n.get("file_path").and_then(Value::as_str).unwrap_or("?"),
                            n.get("target").and_then(Value::as_str).unwrap_or("?"),
                            n.get("note").and_then(Value::as_str).unwrap_or("?"),
                        ));
                    }
                    return Some(output);
                }
                if let Some(n) = d.get("note") {
                    // `note get` / `note set` echo.
                    return Some(match n.as_str() {
                        Some(text) => format!(
                            "{}  {}  {}\n",
                            d.get("file").and_then(Value::as_str).unwrap_or("?"),
                            d.get("target").and_then(Value::as_str).unwrap_or("?"),
                            text
                        ),
                        None => "no note\n".to_string(),
                    });
                }
                if d.get("removed").is_some() {
                    return Some(format!(
                        "removed {}\n",
                        d.get("removed").and_then(Value::as_bool).unwrap_or(false)
                    ));
                }
                None
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
        Command::Coverage { path, json } => {
            coverage_cmd::run(coverage_cmd::CoverageOptions { path, json })
        }
        Command::Audit { path, top, json } => {
            audit_cmd::run(audit_cmd::AuditOptions { path, top, json })
        }
        Command::Space {
            path,
            json,
            delete,
            yes,
        } => space_cmd::run(path, json, delete, yes),
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
        Command::IndexStats { path } => {
            let path = discover_root(&path)?;
            let shard = Shard::open(&shard_path(&path)).map_err(|e| e.to_string())?;
            let _ = extractor_for_shard(&shard); // validates extractor id
            write_stdout(&format!(
                "files={} grams={} extractor={} commit={}\n",
                shard.file_count(),
                shard.gram_count(),
                shard.extractor_id(),
                shard.commit_oid().unwrap_or("-")
            ))?;
            Ok(())
        }
        Command::Daemon { cmd } => match cmd {
            DaemonCmd::Start { path, foreground } => {
                daemon_start(discover_root(&path)?, foreground, false)
            }
            DaemonCmd::Stop { path } => daemon_stop(discover_root(&path)?),
            DaemonCmd::Status { path } => daemon_status(discover_root(&path)?),
        },
        Command::Recall { cmd } => recall_cmd::run_recall(cmd),
        Command::ListErrors { cmd } => sniper_cmd::run_sniper(cmd),
        Command::Classify(options) => classify::run(options),
        Command::WebSearch { query, limit, json } => {
            web_search::run(web_search::WebSearchOptions { query, limit, json })
        }
        // -------------------------------------------------------------
        // M2 — safe git mutation ops (pixel-ops)
        // -------------------------------------------------------------
        Command::RepoState {
            path,
            files,
            include_clean,
            json,
        } => {
            let root = discover_root(&path)?;
            let mut data = pixel_ops::inspect::inspect(&root)?;
            if !files.is_empty() {
                // Filter the dirty/clean lists to the requested paths.
                if let Some(dirty) = data.get_mut("dirty").and_then(Value::as_array_mut) {
                    dirty.retain(|d| {
                        d.get("path")
                            .and_then(Value::as_str)
                            .is_some_and(|p| files.iter().any(|f| f == p))
                    });
                }
                if let Some(clean) = data.get_mut("clean").and_then(Value::as_array_mut) {
                    clean.retain(|c| c.as_str().is_some_and(|p| files.iter().any(|f| f == p)));
                }
                data["dirty_count"] = json!(
                    data.get("dirty")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len)
                );
                data["clean_count"] = json!(
                    data.get("clean")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len)
                );
            }
            if !include_clean {
                compact_repo_state(&mut data);
            }
            print_data(&data, json)
        }
        Command::ReviewChanges {
            path,
            cursor,
            byte_cap,
            json,
        } => {
            let root = discover_root(&path)?;
            let data = pixel_ops::review::review(&root, cursor.as_deref(), byte_cap)?;
            print_data(&data, json)
        }
        Command::CommitHistory {
            path,
            ref_,
            limit,
            detail,
            cursor,
            byte_cap,
            json,
        } => {
            let root = discover_root(&path)?;
            let data = pixel_ops::history::history(
                &root,
                ref_.as_deref(),
                limit,
                &detail,
                cursor.as_deref(),
                byte_cap,
            )?;
            print_data(&data, json)
        }
        Command::Diff {
            from,
            to,
            path,
            paths,
            byte_cap,
            json,
        } => {
            let root = discover_root(&path)?;
            let paths_opt = if paths.is_empty() {
                None
            } else {
                Some(paths.as_slice())
            };
            let data = pixel_ops::diff::diff(&root, &from, to.as_deref(), paths_opt, byte_cap)?;
            print_data(&data, json)
        }
        Command::Commit {
            message,
            message_file,
            path,
            files,
            push,
            amend,
            expected_head,
            request_id,
            json,
        } => {
            let root = discover_root(&path)?;
            let message = commit_message(message, message_file.as_deref())?;
            let opts = pixel_ops::publish::PublishOptions {
                message,
                files,
                expected_head,
                expected_fingerprints: std::collections::BTreeMap::new(),
                push,
                amend,
                request_id,
            };
            let data = pixel_ops::publish::publish(&root, &opts, None)?;
            print_data(&data, json)
        }
        Command::Push {
            remote,
            refspec,
            path,
            force_with_lease,
            request_id,
            json,
        } => {
            let root = discover_root(&path)?;
            let opts = pixel_ops::push::PushOptions {
                remote,
                refspec,
                request_id,
                force_with_lease,
            };
            let data = pixel_ops::push::push(&root, &opts, None)?;
            print_data(&data, json)
        }
        Command::CommitAndPush {
            message,
            message_file,
            path,
            files,
            remote,
            refspec,
            force_with_lease,
            request_id,
            json,
        } => {
            let root = discover_root(&path)?;
            let message = commit_message(message, message_file.as_deref())?;
            let data = pixel_ops::ship::ship_with_lease(
                &root,
                &message,
                &files,
                &remote,
                &refspec,
                &request_id,
                force_with_lease,
            )?;
            print_data(&data, json)
        }
        Command::NewBranch {
            name,
            path,
            from,
            request_id,
            json,
        } => {
            let root = discover_root(&path)?;
            let opts = pixel_ops::branch::BranchOptions {
                name,
                from,
                request_id,
            };
            let data = pixel_ops::branch::branch(&root, &opts)?;
            print_data(&data, json)
        }
        Command::FastForward {
            path,
            expected_head,
            target_oid,
            request_id,
            json,
        } => {
            let root = discover_root(&path)?;
            let opts = pixel_ops::update::UpdateOptions {
                expected_head,
                target_oid,
                request_id,
            };
            let data = pixel_ops::update::update(&root, &opts)?;
            print_data(&data, json)
        }
        Command::Fetch {
            remote,
            path,
            refspec,
            json,
        } => {
            let root = discover_root(&path)?;
            let data = pixel_ops::sync::sync(&root, &remote, refspec.as_deref())?;
            print_data(&data, json)
        }
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
        Command::SearchHistory {
            query,
            path,
            facet,
            limit,
            json,
        } => {
            let data = execute(
                &path,
                Request::History {
                    query,
                    facet: Some(facet),
                    limit,
                },
                false,
            )?;
            print_data(&data, json)
        }
        Command::FileHistory {
            path,
            file,
            token,
            json,
        } => {
            let data = execute(&path, Request::Lifecycle { path: file, token }, false)?;
            print_data(&data, json)
        }
        Command::DigHistory {
            path,
            phrase,
            file,
            from,
            to,
            limit,
            show,
            parent,
            json,
        } => {
            if let Some(oid) = show {
                return excavate_show(&path, &oid, file.as_deref(), parent, json);
            }
            let data = execute(
                &path,
                Request::Excavate {
                    phrase,
                    path: file,
                    from,
                    to,
                    limit,
                },
                false,
            )?;
            print_data(&data, json)
        }
        Command::SyncBranch {
            path,
            strategy,
            push,
            into,
            request_id,
            json,
        } => {
            let data = execute(
                &path,
                Request::Reconcile {
                    strategy: Some(strategy),
                    push: Some(push),
                    into,
                    request_id,
                },
                false,
            )?;
            print_data(&data, json)
        }
        Command::RecordEvent {
            kind,
            path,
            file,
            detail,
            json,
        } => {
            let data = execute(
                &path,
                Request::Journal {
                    kind,
                    path: file,
                    detail,
                },
                false,
            )?;
            print_data(&data, json)
        }
        // -------------------------------------------------------------
        // M5/M6 — install / doctor / migrate / hook
        // -------------------------------------------------------------
        Command::Install { json, shell, repo } => {
            let is_global_install = repo.is_none();
            let config_root = repo.clone();
            let stdout_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
            let interactive_banner = should_render_install_banner(json, stdout_tty);
            let color = banner_color(std::env::var_os("NO_COLOR").as_deref());
            if interactive_banner {
                let stdin_tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
                if install_intro::should_play(json, stdout_tty, stdin_tty, |key| {
                    std::env::var_os(key)
                }) {
                    install_intro::play();
                }
                write_stdout(&pixel_install::banner::render_start(color))?;
            }
            let report = pixel_install::install::install(&pixel_install::install::InstallOptions {
                shell,
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
                write_stdout(&pixel_install::banner::render_result(&report, color))
            }
        }
        Command::Uninstall {
            json,
            dry_run,
            binary_path,
            shell,
            wrappers_only,
            repo,
        } => {
            let report =
                pixel_install::uninstall::uninstall(&pixel_install::uninstall::UninstallOptions {
                    binary_path,
                    running_binary: std::env::current_exe().ok(),
                    dry_run,
                    shell,
                    wrappers_only,
                    repo,
                    ..Default::default()
                })
                .map_err(|e| e.to_string())?;
            print_data(
                &serde_json::to_value(&report).map_err(|e| e.to_string())?,
                json,
            )
        }
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
            // atomic (same filesystem). Mirrors scripts/install.sh's `.pixel.tmp.$$`.
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
                let catalogue = serde_json::to_value(pixel_install::doctor::CHECKS)
                    .map_err(|e| e.to_string())?;
                return if json {
                    print_data(&catalogue, true)
                } else {
                    write_stdout(&pixel_install::doctor::render_catalogue(
                        pixel_install::doctor::CHECKS,
                    ))
                };
            }
            let options = discover_root(&path).map(|root| pixel_install::doctor::DoctorOptions {
                repo_root: Some(root),
                shell,
                only,
                skip,
                ..Default::default()
            });
            let run = |options: &pixel_install::doctor::DoctorOptions| {
                pixel_install::doctor::doctor(options).map_err(|e| e.to_string())
            };
            // Exit 2 keeps "the checks could not run" apart from exit 1,
            // "a check found a problem", for a script gating on doctor.
            let options = options.inspect_err(|_| owned_exit.set(Some(2)))?;
            let mut report = run(&options).inspect_err(|_| owned_exit.set(Some(2)))?;
            let plan = pixel_install::doctor::repair_plan(&report);
            let mut outcomes = None;
            if fix {
                let exe = std::env::current_exe().map_err(|e| e.to_string())?;
                let (plan, left) = pixel_install::doctor::split_home_repairs(
                    plan.clone(),
                    pixel_install::config::is_side_build(&exe),
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
                        pixel_install::doctor::run_repair(&exe, repair)
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
                            pixel_install::doctor::judge_repair(repair, ran, &report)
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
                    text.push_str(&pixel_install::doctor::render_repairs(outcomes));
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
        Command::Migrate { .. } => {
            eprintln!("{MIGRATE_REMOVED_NOTE}");
            Ok(())
        }
        Command::RunHook { cmd } => match cmd {
            HookCmd::TaskEvent { provider, event } => task_hook::run(provider, event),
            // Retired verbs: drain the payload the host wrote, answer nothing.
            HookCmd::Retired(_) => {
                let _ = hook_input::read_hook_payload();
                std::process::exit(0);
            }
        },
        Command::Config { cmd } => match cmd {
            None => config_cmd::overview(Path::new(".")),
            Some(ConfigCmd::Setup) => config_cmd::setup(),
            Some(ConfigCmd::Classify { value }) => config_cmd::set_classify_enabled(value == "on"),
            Some(ConfigCmd::Edit { repo, path }) => config_cmd::edit(&path, repo),
            Some(ConfigCmd::Metrics {
                value,
                global,
                path,
            }) => config_cmd::run_metrics(&path, global, value.as_deref().map(|v| v == "on")),
            Some(ConfigCmd::Policy {
                value,
                global,
                json,
                path,
            }) => config_cmd::run_policy(&path, global, value, json),
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
                if value == "jev" {
                    config_cmd::set_classify_remote_model(decide_remote::Preset::Jev, None, None)?;
                    println!("classify engine: remote (jev) stored");
                } else {
                    config_cmd::set_classify_engine(&value)?;
                    println!("classify engine: {value} stored");
                }
                Ok(())
            }
        },
        Command::TaskState { cmd } => task_commands::run(cmd),
        Command::ActionLog {
            path,
            limit,
            errors_only,
            json,
            clear,
        } => run_log(&path, limit, errors_only, json, clear),
        Command::TokenSavings {
            path,
            json,
            since_hours,
        } => run_savings(&path, json, since_hours),
        Command::SquashBranch {
            path,
            onto,
            message,
            push,
            remote,
            expected_head,
            allow_default_branch,
            request_id,
            json,
        } => {
            let root = discover_root(&path)?;
            let opts = pixel_ops::rewrite::RewriteOptions {
                onto,
                message,
                push,
                remote,
                request_id,
                expected_head,
                allow_default_branch,
            };
            let data = pixel_ops::rewrite::rewrite(&root, &opts)?;
            print_data(&data, json)
        }
        Command::WhoWrote {
            file,
            path,
            lines,
            author,
            limit_regions,
            json,
        } => {
            let root = discover_root(&path)?;
            let opts = pixel_ops::provenance::ProvenanceOptions {
                file,
                lines,
                author,
                limit_regions,
            };
            let data = pixel_ops::provenance::provenance(&root, &opts)?;
            print_data(&data, json)
        }
        Command::ListBranches {
            path,
            fetch,
            remote,
            stale_days,
            json,
        } => {
            let root = discover_root(&path)?;
            let opts = pixel_ops::branches::BranchesOptions {
                fetch,
                remote,
                stale_days,
            };
            let data = pixel_ops::branches::branches(&root, &opts)?;
            print_data(&data, json)
        }
        Command::EditEnv { cmd } => {
            use pixel_ops::envfile::EnvAction;
            let (path, json, action) = match cmd {
                EnvCmd::Inventory { path, json } => (path, json, EnvAction::Inventory),
                EnvCmd::Set {
                    file,
                    key,
                    value,
                    create_file,
                    path,
                    json,
                } => (
                    path,
                    json,
                    EnvAction::Set {
                        file,
                        key,
                        value,
                        create_file,
                    },
                ),
                EnvCmd::Restore {
                    file,
                    snapshot,
                    path,
                    json,
                } => (path, json, EnvAction::Restore { file, snapshot }),
                EnvCmd::Snapshots { file, path, json } => {
                    (path, json, EnvAction::Snapshots { file })
                }
                EnvCmd::Check {
                    file,
                    require,
                    path,
                    json,
                } => (path, json, EnvAction::Check { file, require }),
            };
            let root = discover_root(&path)?;
            let data = pixel_ops::envfile::envfile(&root, &action)?;
            print_data(&data, json)
        }
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
        Command::Ultraflow(options) => ultraflow_cmd::run(options),
        Command::Flow { cmd } => {
            use pixel_flow::FlowAction;
            let json = match &cmd {
                FlowCmd::Save { json, .. }
                | FlowCmd::Get { json, .. }
                | FlowCmd::List { json, .. }
                | FlowCmd::Revise { json, .. }
                | FlowCmd::Replay { json, .. }
                | FlowCmd::Run { json, .. }
                | FlowCmd::Delete { json, .. }
                | FlowCmd::Show { json, .. } => *json,
            };
            let action = match cmd {
                FlowCmd::Save {
                    name,
                    title,
                    description,
                    tags,
                    url,
                    from_file,
                    json: _,
                } => FlowAction::Save {
                    name,
                    title,
                    description,
                    tags,
                    url,
                    from_file: Some(from_file),
                },
                FlowCmd::Get { name, json: _ } => FlowAction::Get { name },
                FlowCmd::List { tag, json: _ } => FlowAction::List { tag },
                FlowCmd::Revise {
                    name,
                    title,
                    description,
                    from_file,
                    json: _,
                } => FlowAction::Revise {
                    name,
                    title,
                    description,
                    from_file,
                },
                FlowCmd::Replay {
                    name,
                    vars,
                    account,
                    json: _,
                } => {
                    let vars = flow_vars(&name, &vars, &account)?;
                    FlowAction::Run {
                        name,
                        vars,
                        dry_run: false,
                    }
                }
                FlowCmd::Run {
                    name,
                    vars,
                    account,
                    json: _,
                } => {
                    let vars = flow_vars(&name, &vars, &account)?;
                    FlowAction::Execute { name, vars }
                }
                FlowCmd::Delete { name, json: _ } => FlowAction::Delete { name },
                FlowCmd::Show { name, json: _ } => FlowAction::Show { name },
            };
            let data = pixel_flow::flow(&action)?;
            if matches!(action, FlowAction::Execute { .. })
                && data.get("success").and_then(serde_json::Value::as_bool) != Some(true)
            {
                return Err(format!(
                    "flow execution failed: {}",
                    data.get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("flow did not complete successfully")
                ));
            }
            if json {
                return print_data(&data, true);
            }
            // For replay and show, the output field contains human-readable
            // text — print it directly to stdout. For everything else, use
            // the standard print_data path (JSON or pretty).
            match &action {
                FlowAction::Run { .. } | FlowAction::Show { .. } => {
                    if let Some(output) = data.get("output").and_then(|v| v.as_str()) {
                        println!("{output}");
                        Ok(())
                    } else {
                        print_data(&data, true)
                    }
                }
                FlowAction::Execute { .. } => {
                    // Print the execution log to stderr, result summary to stdout.
                    if let Some(log) = data.get("log").and_then(|v| v.as_str()) {
                        eprintln!("{log}");
                    }
                    let success = data
                        .get("success")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    let steps = data
                        .get("steps_executed")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    let skipped = data
                        .get("steps_skipped")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    if success {
                        println!("✓ Flow executed: {} steps, {} skipped", steps, skipped);
                    } else if let Some(err) = data.get("error").and_then(|v| v.as_str()) {
                        println!("✗ Flow failed after {} steps: {}", steps, err);
                    } else {
                        println!(
                            "~ Flow completed with warnings: {} steps, {} skipped",
                            steps, skipped
                        );
                    }
                    Ok(())
                }
                _ => print_data(&data, true),
            }
        }
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
    let tokens = |v: &Value| recall_cmd::estimate_tokens(&v.to_string());
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

/// How many trailing log entries `pixel log` reads. Over-fetch when
/// filtering to errors so `limit` still means "the last N errors", not "the
/// last N entries, some of which happen to be errors".
fn log_fetch_count(limit: usize, errors_only: bool) -> usize {
    if errors_only {
        limit.max(1) * 20
    } else {
        limit.max(1)
    }
}

/// The last `limit` of the tailed entries, errors only when asked.
fn select_log_events(
    mut events: Vec<pixel_actionlog::ActionEvent>,
    limit: usize,
    errors_only: bool,
) -> Vec<pixel_actionlog::ActionEvent> {
    if errors_only {
        events.retain(|e| e.outcome == pixel_actionlog::Outcome::Error);
    }
    if events.len() > limit {
        let start = events.len() - limit;
        events.drain(0..start);
    }
    events
}

/// `pixel log` — the self-assessment surface over the async action log every
/// pixel invocation writes to `<root>/.pixel/actions.jsonl`.
fn run_log(
    path: &Path,
    limit: usize,
    errors_only: bool,
    json: bool,
    clear: bool,
) -> Result<(), String> {
    let root = discover_root(path)?;
    let log_path = pixel_actionlog::ActionLog::path_for_root(&root);
    if clear {
        return match std::fs::remove_file(&log_path) {
            Ok(()) => {
                println!("action log cleared: {}", log_path.display());
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                println!("no action log at {}", log_path.display());
                Ok(())
            }
            Err(e) => Err(format!("remove {}: {e}", log_path.display())),
        };
    }
    let tailed = pixel_actionlog::tail(&log_path, log_fetch_count(limit, errors_only))
        .map_err(|e| format!("read {}: {e}", log_path.display()))?;
    let events = select_log_events(tailed, limit, errors_only);
    if json {
        for e in &events {
            println!("{}", serde_json::to_string(e).map_err(|e| e.to_string())?);
        }
        return Ok(());
    }
    if events.is_empty() {
        println!("no recorded actions at {}", log_path.display());
        return Ok(());
    }
    let now = pixel_actionlog::now_ms();
    for e in &events {
        let when = relative_time(e.ts_ms, now);
        match e.outcome {
            pixel_actionlog::Outcome::Ok => {
                println!(
                    "{when:>8}  ok     {:<10} {} ({} ms)",
                    e.command, e.args, e.duration_ms
                );
            }
            pixel_actionlog::Outcome::Error => {
                println!(
                    "{when:>8}  ERROR  {:<10} {} ({} ms) — {}",
                    e.command,
                    e.args,
                    e.duration_ms,
                    e.error.as_deref().unwrap_or("?")
                );
            }
        }
        if let Some(line) = pixel_actionlog::format_metrics_line(e) {
            println!("  {line}");
        }
    }
    Ok(())
}

/// Per-command aggregate of `pixel savings`: invocations, pool chars,
/// snippet chars.
#[derive(Debug, Default, PartialEq, Eq)]
struct SavingsAgg {
    count: u64,
    pool: u64,
    snippet: u64,
}

/// The events at or after `cutoff_ms` (all of them without a cutoff).
fn savings_window(
    events: &[pixel_actionlog::ActionEvent],
    cutoff_ms: Option<i64>,
) -> Vec<pixel_actionlog::ActionEvent> {
    events
        .iter()
        .filter(|e| cutoff_ms.is_none_or(|c| e.ts_ms >= c))
        .cloned()
        .collect()
}

/// Pool and snippet chars per command, over the events that recorded both.
fn savings_by_command(
    events: &[pixel_actionlog::ActionEvent],
) -> std::collections::BTreeMap<String, SavingsAgg> {
    let mut by_cmd = std::collections::BTreeMap::<String, SavingsAgg>::new();
    for e in events {
        let (Some(snippet), Some(pool)) = (e.snippet_cap_chars, e.pool_chars) else {
            continue; // not retrieval-shaped (or volumes not recorded)
        };
        let agg = by_cmd.entry(e.command.clone()).or_default();
        agg.count += 1;
        agg.pool = agg.pool.saturating_add(pool);
        agg.snippet = agg.snippet.saturating_add(snippet);
    }
    by_cmd
}

/// The share of the pool the snippets did not return; 0 for an empty pool.
fn savings_ratio(snippet: u64, pool: u64) -> f64 {
    if pool > 0 {
        1.0 - (snippet as f64 / pool as f64)
    } else {
        0.0
    }
}

/// Preserve legacy snippet/pool reports, separately aggregate versioned
/// invocation metrics. Old measurements are never silently reclassified as a workflow version.
fn run_savings(path: &Path, json: bool, since_hours: Option<u64>) -> Result<(), String> {
    let root = discover_root(path)?;
    let log_path = pixel_actionlog::ActionLog::path_for_root(&root);
    // Over-fetch; savings is a lightweight aggregate read.
    let events = pixel_actionlog::tail(&log_path, 1_000_000)
        .map_err(|e| format!("read {}: {e}", log_path.display()))?;
    let cutoff_ms = since_hours.map(|h| {
        pixel_actionlog::now_ms().saturating_sub(
            i64::try_from(h)
                .unwrap_or(i64::MAX)
                .saturating_mul(3_600_000),
        )
    });
    let filtered = savings_window(&events, cutoff_ms);
    let workflow_metrics = pixel_actionlog::summarize_metrics(&filtered);
    let by_cmd = savings_by_command(&filtered);

    let tot_pool: u64 = by_cmd.values().map(|a| a.pool).sum();
    let tot_snippet: u64 = by_cmd.values().map(|a| a.snippet).sum();
    let overall = savings_ratio(tot_snippet, tot_pool);

    if json {
        let rows: Vec<serde_json::Value> = by_cmd
            .iter()
            .map(|(cmd, a)| {
                let ratio = savings_ratio(a.snippet, a.pool);
                serde_json::json!({
                    "command": cmd,
                    "calls": a.count,
                    "pool_chars": a.pool,
                    "snippet_chars": a.snippet,
                    "savings": ratio,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "overall_savings": overall,
                "total_pool_chars": tot_pool,
                "total_snippet_chars": tot_snippet,
                "by_command": rows,
                "legacy_basis": "legacy snippet/pool byte comparison; not a workflow estimate",
                "workflow_metrics": workflow_metrics,
            })
        );
        return Ok(());
    }

    println!("workflow metrics (measured bytes/duration; versioned byte-based token estimates)");
    println!(
        "{}",
        serde_json::to_string_pretty(&workflow_metrics).map_err(|e| e.to_string())?
    );
    println!("legacy savings (snippet vs candidate-pool chars; not a workflow estimate)");
    println!(
        "{:<14} {:>5}  {:>12}  {:>14}  {:>7}",
        "command", "calls", "pool_chars", "snippet_chars", "savings"
    );
    for (cmd, a) in &by_cmd {
        let ratio = savings_ratio(a.snippet, a.pool);
        println!(
            "{:<14} {:>5}  {:>12}  {:>14}  {:>6.1}%",
            cmd,
            a.count,
            a.pool,
            a.snippet,
            ratio * 100.0
        );
    }
    println!(
        "{:<14} {:>5}  {:>12}  {:>14}  {:>6.1}%",
        "TOTAL",
        by_cmd.values().map(|a| a.count).sum::<u64>(),
        tot_pool,
        tot_snippet,
        overall * 100.0
    );
    Ok(())
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

fn relative_time(ts_ms: i64, now_ms: i64) -> String {
    let delta_ms = (now_ms - ts_ms).max(0);
    let secs = delta_ms / 1000;
    if secs < 5 {
        "just now".to_string()
    } else if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

/// Dry-run parse one `pixel …` argv (including the leading "pixel") against
/// this binary's real clap definition — nothing is executed. Used by
/// `pixel doctor`'s rule-vs-binary parity check so the installed rule text
/// can never document syntax the parser would reject.
/// The commit message of `publish`/`ship`: `-m <text>`, or the content of
/// `--message-file <path>` (`-` reads stdin). Clap guarantees exactly one
/// of the two is present.
fn commit_message(inline: Option<String>, file: Option<&Path>) -> Result<String, String> {
    let raw = match (inline, file) {
        (Some(text), _) => text,
        (None, Some(path)) if path == Path::new("-") => {
            let mut text = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
                .map_err(|e| format!("cannot read commit message from stdin: {e}"))?;
            text
        }
        (None, Some(path)) => std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read commit message file {}: {e}", path.display()))?,
        (None, None) => return Err("a commit message is required (-m or --message-file)".into()),
    };
    normalize_commit_message(&raw)
}

/// Trailing whitespace goes (an editor's final newline would otherwise
/// become a blank trailer line); a blank message is refused before git
/// sees it, so no journal entry is written for a commit git would reject.
fn normalize_commit_message(raw: &str) -> Result<String, String> {
    let text = raw.trim_end();
    if text.trim().is_empty() {
        return Err("commit message is empty".into());
    }
    Ok(text.to_string())
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
    use pixel_install::doctor::VARIADIC_SENTINEL;
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

/// `pixel excavate --show <oid> --file <path>`: full historical file content
/// in ONE call — the follow-up to an excavate candidate list that previously
/// forced agents into raw `git show`/`git log` rounds. Reads `<oid>:<path>`
/// through the safe `pixel_git::GitRunner` (ref-validated, output-capped);
/// when the file does not exist at `<oid>` (e.g. `<oid>` is the deletion
/// commit itself) it falls back to `<oid>^:<path>` — the pre-deletion
/// content — and says so. `--parent` skips straight to the parent read.
/// Implemented CLI-side (no daemon/proto round-trip): the content lives in
/// the object store, not the facts db, so a direct git read is exact.
fn excavate_show(
    path: &Path,
    oid: &str,
    file: Option<&str>,
    parent: bool,
    json: bool,
) -> Result<(), String> {
    let Some(file) = file else {
        return Err("dig-history --show requires --file <repo-relative path>".to_string());
    };
    let root = discover_root(path)?;
    let runner = pixel_git::GitRunner::new(&root);
    let (content, source, parent_fallback) = if parent {
        let c = runner
            .show_blob_string_at_parent(oid, file)
            .map_err(|e| format!("cannot read {oid}^:{file}: {e}"))?;
        (c, format!("{oid}^:{file}"), false)
    } else {
        match runner.show_blob_string(oid, file) {
            Ok(c) => (c, format!("{oid}:{file}"), false),
            Err(at_oid_err) => match runner.show_blob_string_at_parent(oid, file) {
                Ok(c) => (c, format!("{oid}^:{file}"), true),
                Err(_) => {
                    return Err(format!(
                        "{file} exists neither at {oid} nor at {oid}^: {at_oid_err}"
                    ));
                }
            },
        }
    };
    if json {
        let data = serde_json::json!({
            "oid": oid,
            "file": file,
            "source": source,
            "parent_fallback": parent_fallback,
            "content": content,
        });
        print_data(&data, true)
    } else {
        if parent_fallback {
            eprintln!(
                "pixel: {file} does not exist at {oid}; showing the parent's \
                 pre-deletion content ({source})"
            );
        } else {
            eprintln!("pixel: {source}");
        }
        write_stdout(&content)
    }
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
    /// counts through a nested subcommand (`list-errors last --json`), whose
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
        assert!(parsed(&["pixel", "list-errors", "last", "--json"]));
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
            // NOTE: the historical rule text wrote `[--path <path>]` here —
            // the real flag is `--file`. That drift is exactly what the
            // runtime `rule.parity` doctor check flags.
            r#"pixel dig-history --phrase "<what you're looking for>" [--file <path>] [--json]"#,
            r#"pixel plan-rollback "<what broke, in the user's words>" /path/to/repo [--json]"#,
            r#"pixel plan-rollback --apply <oid> --file <path> /path/to/repo [--merge|--stash-first|--allow-dirty]"#,
            r#"pixel find-code "<phrase>" /path/to/repo [--json] [--limit N]"#,
            r#"pixel search-content "<pattern>" /path/to/repo --context 5 [--json] [--limit N]"#,
            r#"pixel sync-branch /path/to/repo [--strategy report|rebase-if-clean] [--push auto|never]"#,
            r#"pixel scope-task "<one-line task description>" /path/to/repo [--json] [--limit N]"#,
            r#"pixel scope-task --clear /path/to/repo"#,
            r#"pixel impact <symbol_name_or_uid> /path/to/repo [--direction upstream|downstream] [--depth N] [--json]"#,
            r#"pixel what-changed /path/to/repo [--base <ref>] [--json]"#,
            r#"pixel repo-state /path/to/repo [--json]"#,
            r#"pixel review-changes /path/to/repo [--json]"#,
            r#"pixel commit-history /path/to/repo [--ref <ref>] [--limit N] [--json]"#,
            r#"pixel diff <from> /path/to/repo [--paths <p1> --paths <p2>] [--json]"#,
            r#"pixel commit --files <f1> --files <f2> --message "<msg>" --request-id <id> /path/to/repo"#,
            r#"pixel push <remote> <refspec> /path/to/repo --request-id <id>"#,
            r#"pixel commit-and-push --files <f1> --files <f2> --message "<msg>" <remote> <refspec> /path/to/repo --request-id <id>"#,
            r#"pixel new-branch <name> /path/to/repo --request-id <id>"#,
            r#"pixel fetch <remote> /path/to/repo [--json]"#,
            r#"pixel fast-forward /path/to/repo --expected-head <oid> --target-oid <oid> --request-id <id>"#,
            r#"pixel status /path/to/repo"#,
            r#"pixel build-index --history ."#,
            r#"pixel install"#,
            r#"pixel doctor"#,
        ];
        let mut failures = Vec::new();
        for line in canonical {
            match pixel_install::doctor::normalize_rule_command(line) {
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

    /// `--files` and `--paths` take one value per occurrence, so a rule line
    /// that shows several values after one flag teaches an agent a command
    /// the CLI rejects: the parity check must go red on it, not validate the
    /// single-value reading, even where the extra value parses as a PATH.
    #[test]
    fn variadic_single_value_flags_are_rejected_as_documented() {
        for line in [
            r#"pixel commit --files <f>... --message "<msg>" --request-id <id> /path/to/repo"#,
            r#"pixel commit-and-push --files <f>... --message "<msg>" <remote> <refspec> /path/to/repo --request-id <id>"#,
            // No explicit PATH: the second value would bind to the defaulted
            // positional and parse, so only the sentinel's trace catches it.
            r#"pixel commit --files <f>... --message "<msg>" --request-id <id>"#,
            r#"pixel diff <from> [--paths <p>...] [--json]"#,
            // The second value lands in a multi-value positional (`paths`),
            // which takes several values but not this flag's.
            r#"pixel search-content "<re>" --glob <g>..."#,
        ] {
            let argv = pixel_install::doctor::normalize_rule_command(line)
                .unwrap_or_else(|| panic!("`{line}` did not normalize"));
            assert!(
                validate_cli_syntax(&argv).is_err(),
                "`{line}` → argv {argv:?} should be rejected by the CLI parser"
            );
        }
    }

    /// An argument that does take several values per occurrence keeps its
    /// `...` placeholder: the parity check must not go red on a true shape.
    #[test]
    fn variadic_placeholder_on_a_multi_value_argument_parses() {
        // The sentinel is the last value, so it is held only if its first
        // value's argument is found at the index just before it.
        let line = "pixel search-content \"<re>\" <path>...";
        let argv = pixel_install::doctor::normalize_rule_command(line)
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
            vec![
                "pixel".to_string(),
                "plan-rollback".into(),
                "--limit".into(),
                "3".into(),
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
            vec![
                "pixel".to_string(),
                "task-state".into(),
                "reset".into(),
                "--session".into(),
                "session-123".into(),
                "/repo".into(),
            ],
            vec![
                "pixel".to_string(),
                "run-hook".into(),
                "prompt-submit".into(),
                "--provider".into(),
                "claude".into(),
            ],
            vec![
                "pixel".to_string(),
                "run-hook".into(),
                "post-compaction".into(),
                "--provider".into(),
                "claude".into(),
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
    use pixel_install::doctor::{extract_rule_commands, normalize_rule_command};

    const ASSETS: [(&str, &str); 2] = [
        (
            "pixel-agent-prompt.md",
            include_str!("../../pixel-install/assets/pixel-agent-prompt.md"),
        ),
        (
            "pixel-subagent-prompt.md",
            include_str!("../../pixel-install/assets/pixel-subagent-prompt.md"),
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

    /// The REPLACEMENT MAP rows that shipped in 0.6.0 must go red, each
    /// named with clap's reason, or the test above cannot catch their like.
    #[test]
    fn the_shipped_replacement_map_rows_are_rejected_by_name() {
        let text = "\
| Instead of | Run |
| --- | --- |
| `git checkout -b` / `git fetch` / `git merge --ff-only` | `pixel new-branch name` / `pixel fetch` / `pixel fast-forward …` |
";
        let (_, failures) = asset_failures("sample.md", text);
        assert_eq!(failures.len(), 3, "{failures:#?}");
        assert!(
            failures[0].starts_with("sample.md: `pixel new-branch name` → ")
                && failures[0].contains("--request-id"),
            "{}",
            failures[0]
        );
        assert!(
            failures[1].starts_with("sample.md: `pixel fetch` → ")
                && failures[1].contains("<REMOTE>"),
            "{}",
            failures[1]
        );
        assert_eq!(
            failures[2],
            "sample.md: `pixel fast-forward …` elides its arguments, but no full `pixel fast-forward …` line in this asset parses"
        );
    }

    /// A placeholder for an integer argument is a correct line: the agent
    /// substitutes a real id, so the dummy must not be what fails it.
    #[test]
    fn a_placeholder_for_an_integer_argument_parses() {
        let text = "| a | `pixel list-errors show <id>` / `pixel list-errors since <cursor>` |\n";
        assert_eq!(asset_failures("sample.md", text), (2, vec![]));
    }

    /// An elision is accepted once the asset spells the command out in a
    /// line that parses, and a line with an unreadable placeholder is
    /// reported, not skipped.
    #[test]
    fn an_elision_needs_a_full_form_and_an_unreadable_line_is_reported() {
        let text = "\
| a | `pixel recall …` |
| b | `pixel recall search \"token\" --since 30d` |
| c | `pixel impact a…b` |
";
        let (checked, failures) = asset_failures("sample.md", text);
        assert_eq!(checked, 1);
        assert_eq!(
            failures,
            vec!["sample.md: `pixel impact a…b` has placeholders the normalizer cannot read"]
        );
    }

    /// The kill switches (`PIXEL_DAEMON_AUTO_START=0`, `PIXEL_GUARD_*=off`)
    /// fire only on an explicit off value: unset and any other value keep
    /// the feature on.
    #[test]
    fn env_flag_off_fires_only_on_an_explicit_off_value() {
        let name = format!("PIXEL_TEST_FLAG_{}_{}", std::process::id(), line!());
        assert!(!crate::env_flag_off(&name), "unset");
        for (value, expected) in [
            ("0", true),
            ("false", true),
            ("off", true),
            ("1", false),
            ("", false),
            ("no", false),
        ] {
            // SAFETY: the variable name is unique to this test (pid + line),
            // so no other thread in the process reads or writes it.
            unsafe {
                std::env::set_var(&name, value);
            }
            assert_eq!(crate::env_flag_off(&name), expected, "{value:?}");
        }
        // SAFETY: as above.
        unsafe {
            std::env::remove_var(&name);
        }
    }
}

#[cfg(test)]
mod renamed_command_tests {
    use super::{
        Cli, bounded_result_note, checks_deployed_prompts, is_developer_build, logged_args,
        rename_note, renamed_invocation, stale_prompt_note,
    };
    use clap::CommandFactory;
    use std::collections::BTreeSet;

    /// The full `Cli` definition overflows a 2 MiB test thread in debug
    /// builds (the reason `validate_cli_syntax` runs on 4 MiB); build and
    /// parse it on a thread sized the same way.
    fn on_big_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(f)
            .unwrap()
            .join()
            .unwrap()
    }

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn clap_aliases_are_exactly_the_rename_table() {
        // Every hidden alias the parser accepts must be a documented rename,
        // and every documented rename must parse: a variant renamed again
        // without updating the table fails here, not in a user's script.
        let registered = on_big_stack(|| {
            let cli = Cli::command();
            let mut registered = BTreeSet::new();
            for sub in cli.get_subcommands() {
                for alias in sub.get_all_aliases() {
                    registered.insert((alias.to_string(), sub.get_name().to_string()));
                }
            }
            registered
        });
        let table: BTreeSet<(String, String)> = pixel_proto::commands::RENAMED_COMMANDS
            .iter()
            .map(|(old, new)| ((*old).to_string(), (*new).to_string()))
            .collect();
        assert_eq!(registered, table);
    }

    #[test]
    fn an_old_name_parses_to_the_current_subcommand() {
        let name = |words: &'static [&'static str]| {
            on_big_stack(move || {
                Cli::command()
                    .try_get_matches_from(words)
                    .unwrap()
                    .subcommand_name()
                    .map(str::to_string)
            })
        };
        // The action log and the metrics evidence key on this name, so an
        // alias invocation is recorded exactly like the current spelling.
        assert_eq!(
            name(&["pixel", "ready", "--no-daemon", "--json"]).as_deref(),
            Some("prepare-repo")
        );
        assert_eq!(
            name(&["pixel", "hook", "session-start"]).as_deref(),
            Some("run-hook")
        );
    }

    /// A mutation op run without `--request-id` names the flag and says
    /// what value to give it; an error about another argument, even one of
    /// a mutation op, carries no such tip.
    #[test]
    fn a_missing_request_id_carries_the_tip_and_nothing_else_does() {
        let rendered = |words: &'static [&'static str]| {
            on_big_stack(move || {
                let error = Cli::command().try_get_matches_from(words).unwrap_err();
                super::with_request_id_tip(error).render().to_string()
            })
        };
        let tip = format!("tip: {}", super::REQUEST_ID_TIP);
        for words in [
            &["pixel", "new-branch", "feat/x"][..],
            &["pixel", "commit", "--files", "a", "-m", "msg"][..],
            &[
                "pixel",
                "fast-forward",
                "--expected-head",
                "a",
                "--target-oid",
                "b",
            ][..],
        ] {
            let text = rendered(words);
            assert!(text.contains("--request-id <REQUEST_ID>"), "{text}");
            assert!(text.contains(&tip), "{words:?}: {text}");
        }
        let fetch = rendered(&["pixel", "fetch"]);
        assert!(fetch.contains("<REMOTE>"), "{fetch}");
        assert!(!fetch.contains("tip:"), "{fetch}");
        let unknown = rendered(&["pixel", "new-branch", "x", "--request-id", "r", "--nope"]);
        assert!(!unknown.contains(super::REQUEST_ID_TIP), "{unknown}");
    }

    #[test]
    fn renamed_invocation_reads_the_command_word_only() {
        assert_eq!(
            renamed_invocation(&argv(&["pixel", "ready"])),
            Some(("ready", "prepare-repo"))
        );
        assert_eq!(
            renamed_invocation(&argv(&["pixel", "--metrics", "off", "changes", "."])),
            Some(("changes", "what-changed")),
            "the value of --metrics is not the command"
        );
        assert_eq!(
            renamed_invocation(&argv(&["pixel", "--metrics=off", "symbol", "x"])),
            Some(("symbol", "find-symbol"))
        );
        assert_eq!(
            renamed_invocation(&argv(&["pixel", "prepare-repo", "ready"])),
            None,
            "a path named like an old command is not a renamed invocation"
        );
        assert_eq!(renamed_invocation(&argv(&["pixel", "impact", "x"])), None);
        assert_eq!(renamed_invocation(&argv(&["pixel", "--help"])), None);
        assert_eq!(renamed_invocation(&argv(&["pixel"])), None);
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

    #[test]
    fn stale_prompt_note_names_each_stale_file_in_one_line() {
        assert_eq!(stale_prompt_note(&[]), None);
        let one = stale_prompt_note(&["agent-prompt.md"]).unwrap();
        assert_eq!(
            one,
            format!(
                "note: agent-prompt.md deployed by an earlier `pixel install` is retired in this pixel ({}) — run `pixel install` to remove it\n",
                env!("CARGO_PKG_VERSION")
            )
        );
        let both = stale_prompt_note(&["agent-prompt.md", "subagent-prompt.md"]).unwrap();
        assert!(
            both.starts_with(
                "note: agent-prompt.md and subagent-prompt.md deployed by an earlier `pixel install` are retired"
            ) && both.ends_with("remove them\n"),
            "{both}"
        );
        assert_eq!(both.lines().count(), 1, "{both}");
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

    #[test]
    fn deployed_prompts_are_checked_except_by_the_commands_that_own_them() {
        assert!(checks_deployed_prompts("search-content", false, false));
        assert!(checks_deployed_prompts("self-update", false, false));
        assert!(!checks_deployed_prompts("search-content", true, false));
        for owner in ["install", "doctor", "uninstall"] {
            assert!(!checks_deployed_prompts(owner, false, false), "{owner}");
        }
    }

    /// A `pixel-dev` built from a branch differs from the release's deployed
    /// prompts by construction: the note would be noise on every command,
    /// and the fix it names would move every repository's hooks and prompts
    /// onto that branch build.
    #[test]
    fn deployed_prompts_are_never_checked_by_a_side_build() {
        assert!(!checks_deployed_prompts("search-content", false, true));
        assert!(!checks_deployed_prompts("self-update", false, true));
    }

    /// #549: a binary run from a checkout's `target/` prints the release's
    /// version over its branch's prompts, so it is a developer build like
    /// `pixel-dev`; a `pixel` anywhere else is an install and keeps the
    /// stale-prompt note.
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

    #[test]
    fn rename_note_is_one_line_and_skips_only_protected_streams() {
        assert_eq!(
            rename_note(&argv(&["pixel", "ready", "--json"]), true).as_deref(),
            Some("note: 'ready' is now 'prepare-repo'; the old name stays accepted until 1.0\n")
        );
        assert_eq!(rename_note(&argv(&["pixel", "ready"]), false), None);
        assert_eq!(rename_note(&argv(&["pixel", "prepare-repo"]), true), None);
    }
}

#[cfg(test)]
#[path = "main_tests/flow_vars_tests.rs"]
mod flow_vars_tests;

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
#[path = "main_tests/commit_message_tests.rs"]
mod commit_message_tests;

#[cfg(test)]
#[path = "main_tests/review_gate_pretty_tests.rs"]
mod review_gate_pretty_tests;

#[cfg(test)]
#[path = "main_tests/search_fallback_gate_tests.rs"]
mod search_fallback_gate_tests;
