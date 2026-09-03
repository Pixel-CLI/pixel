//! gitpixel CLI — index/search plus the graph command surface, speaking to a
//! per-root daemon over its Unix socket when one is up, else in-process.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};

mod guard;
mod prompt_submit;
mod recall_cmd;
mod rescue_cmd;
mod sniper_cmd;
use pixel_index::index::{build, shard_path};
use pixel_index::shard::Shard;
use pixel_index::{Crc32Weigher, GramExtractor, SparseGramExtractor, TrigramExtractor};
use pixel_daemon::api::{PROTOCOL_VERSION, Request, Response, Service};
use pixel_daemon::daemon;
use pixel_proto::{compile_query, QueryKind, QueryStatus};
use serde_json::{json, Value};

#[derive(Parser)]
#[command(
    name = "pixel",
    version,
    about = "Fast, fresh code retrieval for agents"
)]
struct Cli {
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

#[derive(Subcommand)]
enum Command {
    /// Build (or rebuild) the text index for a directory tree.
    Index {
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
    Search {
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
        /// Ranking scope. Only `code` is supported: it reranks matches by
        /// file-level signals (filename match, symbol match, content density)
        /// via pixel-rank's RRF without changing the hit set. Any other value
        /// is an error. Omit `scope` for unranked (path/line) order.
        #[arg(long)]
        scope: Option<String>,
        /// Lines of context to include around each match (reads the file
        /// on-demand). Eliminates the need for a follow-up Read call.
        #[arg(long, default_value_t = 0)]
        context: usize,
    },
    /// Compile and execute one bounded deterministic retrieval recipe.
    Query {
        intent: String,
        #[arg(default_value = ".")]
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
    /// Sniper target list: task description in, closed prioritized file list
    /// out (P0 = start here, P1 = likely, P2 = droppable). Writes the
    /// enforcement manifest .pixel/targets.json unless --no-manifest.
    Targets {
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
        /// Deactivate scoping: delete .pixel/targets.json and exit.
        #[arg(long)]
        clear: bool,
    },
    /// Surgical revert planner: locate the files a problem points at, list
    /// recent versions with the likely-breaking commit flagged, recommend a
    /// last-known-good candidate. Plan only — nothing is written without
    /// --apply. Never resets; never touches the index or HEAD.
    Rescue {
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
    /// Look up symbols by name in the code graph.
    Symbol {
        name: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Budget-fitted context for a symbol uid.
    Context {
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
        #[arg(long)]
        json: bool,
    },
    /// Direct callers or callees of a symbol.
    Uses {
        uid_or_name: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, value_enum, default_value = "callers")]
        role: RoleArg,
        /// Skip this many relationships for page-wise retrieval.
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long)]
        json: bool,
    },
    /// Call path between two symbols.
    Trace {
        from: String,
        to: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Discovered execution flows.
    Processes {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long)]
        json: bool,
    },
    /// Functional-area clusters.
    Clusters {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long)]
        json: bool,
    },
    /// Symbols/flows affected by working-tree changes.
    Changes {
        #[arg(default_value = ".")]
        path: PathBuf,
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
    Graph {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Index + graph freshness status.
    Status {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Make a repository ready for agent work: index, graph, and warm daemon.
    Ready {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Build indexes only; do not start or use the background daemon.
        #[arg(long)]
        no_daemon: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show raw shard metadata (legacy).
    Stats {
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
    Sniper {
        #[command(subcommand)]
        cmd: sniper_cmd::SniperCmd,
    },
    // -----------------------------------------------------------------
    // M2 — safe git mutation ops (pixel-ops)
    // -----------------------------------------------------------------
    /// Show repo state: HEAD, branch, dirty files, fingerprints.
    Inspect {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Restrict the snapshot to these repo-relative paths.
        #[arg(long = "files")]
        files: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Review working-tree changes (staged, unstaged, untracked, conflicted).
    Review {
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
    /// Commit history with detail levels and byte caps.
    History {
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
    Publish {
        /// Commit message.
        #[arg(short = 'm', long = "message")]
        message: String,
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
    Ship {
        /// Commit message.
        #[arg(short = 'm', long = "message")]
        message: String,
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
    Branch {
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
    Update {
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
    Sync {
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
    Resolve {
        phrase: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// M3: history-wide fact + diff search.
    HistorySearch {
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
    Lifecycle {
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
    Excavate {
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
    Reconcile {
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
    Journal {
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
    /// Idempotent install: scrub deprecated MCP entries, wire hooks + agent-config.
    Install {
        #[arg(long)]
        json: bool,
    },
    /// Health check: install state, daemon, index/graph/facts freshness.
    Doctor {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Clean-cut state migration: drop .gitpixel/, rebuild .pixel/ fresh.
    Migrate {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Hook entrypoints (guard, session-start) invoked by Claude hooks.
    Hook {
        #[command(subcommand)]
        cmd: HookCmd,
    },
    /// Self-assessment: pixel's own action log (what ran, what went wrong).
    /// Reads <path>/.pixel/actions.jsonl, written asynchronously by every
    /// pixel invocation.
    Log {
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
    /// Squash every commit on the current branch since its base into ONE
    /// commit (crash-safe, backup-ref'd), optionally force-pushing with lease.
    Rewrite {
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
        /// Idempotency / recovery key.
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Per-region blame attribution: who introduced/owns each region of a file.
    Provenance {
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
    Branches {
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
    Env {
        #[command(subcommand)]
        cmd: EnvCmd,
    },
    /// Save, retrieve, list, revise, and replay proven agent-browser paths
    /// (auth flows, config flows) so the agent follows a deterministic
    /// shortcut instead of re-discovering the UI from scratch every time.
    Flow {
        #[command(subcommand)]
        cmd: FlowCmd,
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
    /// Emit ready-to-run agent-browser commands with variable substitution.
    /// Pixel does NOT run agent-browser — it outputs the deterministic
    /// command sequence for the agent to execute.
    ///
    /// Use `--execute` to actually run the commands via agent-browser.
    Replay {
        name: String,
        /// Variable substitution: `--var key=value`. Repeat per var.
        #[arg(long = "var")]
        vars: Vec<String>,
        /// Shortcut for `--var google_account=<value>` (or `openai_account`
        /// depending on the flow). Picks which account to use.
        /// Accepts a full email or a short alias: loic, mfleguier, livio.
        #[arg(long)]
        account: Option<String>,
        /// Actually execute the flow by running agent-browser commands.
        /// Without this flag, replay only prints the command sequence.
        #[arg(long)]
        execute: bool,
        /// Print commands without marking as executed (default is still
        /// print-only — pixel never runs agent-browser).
        #[arg(long)]
        dry_run: bool,
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

/// Parse a 1-based inclusive line range "a,b" for `provenance --lines`.
fn parse_line_range(s: &str) -> Result<(u32, u32), String> {
    let (a, b) = s
        .split_once(',')
        .ok_or_else(|| format!("expected 'start,end', got '{s}'"))?;
    let a: u32 = a.trim().parse().map_err(|e| format!("bad start line: {e}"))?;
    let b: u32 = b.trim().parse().map_err(|e| format!("bad end line: {e}"))?;
    if a == 0 || b < a {
        return Err(format!("invalid range {a},{b}: need 1 <= start <= end"));
    }
    Ok((a, b))
}

#[derive(Subcommand)]
enum HookCmd {
    /// `pixel hook guard "$@"` — targets enforcement guard.
    Guard {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// `pixel hook session-start` — emit capability block from op registry.
    SessionStart {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// `pixel hook prompt-submit "$@"` — task boundary detector.
    /// Reads the UserPromptSubmit payload from stdin, embeds the prompt
    /// and recent context, and emits a `[PIXEL:TASK_BOUNDARY]` advisory
    /// when a task boundary is detected.
    PromptSubmit,
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
    let sock = daemon::socket_path(root);
    let mut stream = UnixStream::connect(&sock).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_millis(100)))
        .ok()?;
    let ping = roundtrip(&mut stream, &Request::Ping)?;
    if !ping.ok
        || ping.data().get("protocol_version").and_then(Value::as_u64) != Some(PROTOCOL_VERSION)
    {
        // Old daemons must not serve stale schemas to a newer CLI. They all
        // understand Shutdown; close them and use the current in-process
        // service for this command. A later explicit start launches current.
        let _ = roundtrip(&mut stream, &Request::Shutdown);
        return None;
    }
    // Real request may legitimately take a while (lazy graph build).
    stream
        .set_read_timeout(Some(Duration::from_secs(600)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .ok()?;
    roundtrip(&mut stream, req)
}

/// Prefer the daemon; fall back to an in-process Service. The given path may
/// be anywhere inside a repo — the root is discovered automatically, so
/// pointing any command at a subdirectory or file just works.
fn execute(path: &Path, req: Request, no_daemon: bool) -> Result<Value, String> {
    let root = discover_root(path)?;
    if !no_daemon && let Some(resp) = try_daemon(&root, &req) {
        return unwrap_response(resp);
    }
    let mut svc = Service::open(&root).map_err(|e| e.to_string())?;
    unwrap_response(svc.handle(req))
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
            obj.insert("epistemics".into(), serde_json::to_value(e).unwrap_or(Value::Null));
        }
        if let Some(s) = snapshot
            && !obj.contains_key("snapshot")
        {
            obj.insert("snapshot".into(), serde_json::to_value(s).unwrap_or(Value::Null));
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
        let ms = info.get("build_ms").and_then(Value::as_u64).unwrap_or(0);
        eprintln!("pixel: built graph.db on first use ({ms} ms)");
    }
}

fn write_stdout(text: &str) -> Result<(), String> {
    match std::io::stdout().write_all(text.as_bytes()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(format!("write stdout: {error}")),
    }
}

fn print_data(data: &Value, raw_json: bool) -> Result<(), String> {
    let mut output = if raw_json {
        serde_json::to_string(data).unwrap_or_default()
    } else {
        serde_json::to_string_pretty(data).unwrap_or_default()
    };
    output.push('\n');
    write_stdout(&output)
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
    let new_id = new_task.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let mut tasks: Vec<Value> = Vec::new();
    if let Some(text) = existing {
        if let Ok(v) = serde_json::from_str::<Value>(text) {
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
    }
    tasks.retain(|t| {
        let created = t.get("created_unix").and_then(Value::as_u64).unwrap_or(0);
        let id = t.get("id").and_then(Value::as_str).unwrap_or("");
        now.saturating_sub(created) <= TARGETS_TTL_SECS && id != new_id
    });
    tasks.push(new_task);
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
        .map(|d| d.as_secs())
        .unwrap_or(0);
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
        .map(Vec::len)
        .unwrap_or(1);
    if let Some(parent) = manifest_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let tmp = manifest_path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(&manifest).unwrap_or_default(),
    )
    .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, manifest_path)
        .map_err(|e| format!("publish {}: {e}", manifest_path.display()))?;
    Ok(active)
}

#[cfg(test)]
mod targets_manifest_tests {
    use super::*;

    fn task_entry(id_src: &str, created: u64, path: &str) -> Value {
        serde_json::json!({
            "id": targets_task_id(id_src),
            "task": id_src,
            "created_unix": created,
            "targets": [{"path": path, "tier": "P0"}],
        })
    }

    #[test]
    fn merge_two_tasks_coexist() {
        let now = 1_000_000;
        let v = merge_targets_manifest(None, task_entry("task A", now, "src/a.rs"), now);
        let text = v.to_string();
        let v2 = merge_targets_manifest(Some(&text), task_entry("task B", now, "src/b.rs"), now);
        let tasks = v2["tasks"].as_array().unwrap();
        assert_eq!(v2["version"], 2);
        assert_eq!(tasks.len(), 2, "concurrent tasks must both survive");
        let names: Vec<&str> = tasks.iter().map(|t| t["task"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["task A", "task B"]);
    }

    #[test]
    fn merge_replaces_same_task_id() {
        let now = 1_000_000;
        let v = merge_targets_manifest(None, task_entry("task A", now - 100, "src/old.rs"), now);
        let text = v.to_string();
        let v2 = merge_targets_manifest(Some(&text), task_entry("task A", now, "src/new.rs"), now);
        let tasks = v2["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 1, "same task id must replace, not append");
        assert_eq!(tasks[0]["targets"][0]["path"], "src/new.rs");
    }

    #[test]
    fn merge_drops_expired_tasks() {
        let now = 1_000_000_000;
        let old = merge_targets_manifest(
            None,
            task_entry("stale task", now - TARGETS_TTL_SECS - 1, "src/stale.rs"),
            now - TARGETS_TTL_SECS - 1,
        );
        let text = old.to_string();
        let v2 = merge_targets_manifest(Some(&text), task_entry("fresh task", now, "src/fresh.rs"), now);
        let tasks = v2["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 1, "expired task must be dropped on merge");
        assert_eq!(tasks[0]["task"], "fresh task");
    }

    #[test]
    fn merge_wraps_legacy_singleton() {
        let now = 1_000_000;
        let legacy = serde_json::json!({
            "version": 1,
            "task": "legacy task",
            "created_unix": now - 50,
            "head_oid": "abc",
            "limit": 20,
            "files": [{"path": "src/legacy.rs", "tier": "P0"}],
        })
        .to_string();
        let v2 = merge_targets_manifest(Some(&legacy), task_entry("new task", now, "src/new.rs"), now);
        let tasks = v2["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 2, "legacy singleton must be preserved as a v2 task");
        assert_eq!(tasks[0]["task"], "legacy task");
        assert_eq!(tasks[0]["targets"][0]["path"], "src/legacy.rs");
        assert_eq!(tasks[1]["task"], "new task");
    }

    #[test]
    fn merge_survives_corrupt_existing() {
        let now = 1_000_000;
        let v2 = merge_targets_manifest(Some("{not json"), task_entry("task A", now, "src/a.rs"), now);
        assert_eq!(v2["tasks"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn task_id_stable_and_short() {
        assert_eq!(targets_task_id("x"), targets_task_id("x"));
        assert_ne!(targets_task_id("x"), targets_task_id("y"));
        assert_eq!(targets_task_id("anything").len(), 12);
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
/// Hard deadline for the SessionStart per-repo freshness probe. The
/// capability block must reach the agent even when the probe cannot
/// complete, so the probe is bounded rather than trusted.
const SESSION_STATUS_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Resolve a short account alias to a full email address for the
/// `--account` flag on `pixel flow replay`. Accepts full emails as-is.
fn resolve_account_alias(input: &str) -> String {
    let lower = input.to_lowercase();
    match lower.as_str() {
        "loic" | "loicmancino" => "loicmancino.work@gmail.com".to_string(),
        "mfleguier" | "marie" | "marie-france" => "mfleguier@gmail.com".to_string(),
        "livio" | "livio.gamassia" | "livio.g" => "livio.gamassia@gmail.com".to_string(),
        // Pass through if it already looks like an email or is unknown.
        _ => input.to_string(),
    }
}

fn discover_root(path: &Path) -> Result<PathBuf, String> {
    let abs = path
        .canonicalize()
        .map_err(|e| format!("bad path {}: {e}", path.display()))?;
    let start = if abs.is_file() {
        abs.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| abs.clone())
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
        "index built with unsupported extractor {id:?}; re-run `pixel index`"
    ))
}

fn print_search_matches(matches: &[Value], json: bool) -> Result<(), String> {
    let mut output = String::with_capacity(matches.len() * 80);
    for m in matches {
        let path = m.get("path").and_then(Value::as_str).unwrap_or("");
        let line = m.get("line").and_then(Value::as_u64).unwrap_or(0);
        let text = m.get("text").and_then(Value::as_str).unwrap_or("");
        let context = m.get("context").and_then(Value::as_str);
        if json {
            let mut entry = serde_json::json!({"path": path, "line": line, "text": text});
            if let Some(ctx) = context {
                entry["context"] = Value::String(ctx.to_string());
            }
            output.push_str(&entry.to_string());
            output.push('\n');
        } else if let Some(ctx) = context {
            output.push_str(&format!("--- {path}:{line} ---\n{ctx}\n"));
        } else {
            output.push_str(&format!("{path}:{line}:{text}\n"));
        }
    }
    match std::io::stdout().write_all(output.as_bytes()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(format!("write search results: {error}")),
    }
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
        let rel = m.get("path").and_then(Value::as_str).unwrap_or("").to_string();
        if rel.is_empty() {
            continue;
        }
        let start_line = m.get("start_line").and_then(Value::as_u64).unwrap_or(0) as usize;
        let end_line = m
            .get("end_line")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(start_line)
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
            let marker = if ln >= start_line && ln <= end_line { ">>" } else { "  " };
            ctx_lines.push(format!("{marker} {ln:>5}: {l}"));
        }
        m["context"] = Value::String(ctx_lines.join("\n"));
    }
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
) -> Result<(), String> {
    let groups = group_by_root(&paths)?;
    let multi_root = groups.len() > 1;
    for (root, rels) in groups {
        let whole_repo = rels.iter().any(String::is_empty);
        let req_paths = if whole_repo { None } else { Some(rels) };
        run_search_one(
            &pattern, &root, req_paths, multi_root, json, stats, limit, offset, no_daemon, scope.clone(), context,
        )?;
    }
    Ok(())
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
) -> Result<(), String> {
    // Fast path via daemon/service (index auto-built if missing).
    let data = execute(
        root,
        Request::Search {
            pattern: pattern.to_string(),
            json,
            limit,
            offset: Some(offset),
            paths: req_paths,
            scope,
        },
        no_daemon,
    )?;
    let empty = Vec::new();
    let matches = data
        .get("matches")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
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
    print_search_matches(&enriched, json)?;
    // Warn the user when results were truncated so the default row cap
    // is never a surprise.
    let truncated = data
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let match_count = data.get("match_count").and_then(Value::as_u64).unwrap_or(0);
    let limit = data.get("limit").and_then(Value::as_u64).unwrap_or(0);
    if truncated {
        eprintln!(
            "⚠ results truncated: returned {}; more matches exist (row limit {}, byte cap {} bytes). \
             Continue with --offset {} or pass --limit to raise the row cap (maximum 10000).",
            match_count,
            limit,
            data.get("byte_cap").and_then(Value::as_u64).unwrap_or(0),
            data.get("next_offset").and_then(Value::as_u64).unwrap_or(0),
        );
    }
    // Epistemics surfacing for search's line-oriented output (which prints
    // matches, not the whole response object): when the answer is a bounded
    // partial, say so on stderr with the named caps.
    if let Some(e) = data.get("epistemics")
        && e.get("lower_bound").and_then(Value::as_bool) == Some(true)
        && let Some(basis) = e.get("basis").and_then(Value::as_str)
    {
        eprintln!("note: bounded result — {basis}");
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
    Ok(())
}

// ---------------------------------------------------------------------------
// daemon management
// ---------------------------------------------------------------------------

fn daemon_ping(root: &Path) -> bool {
    try_daemon(root, &Request::Ping)
        .map(|r| r.ok)
        .unwrap_or(false)
}

fn daemon_start(path: PathBuf, foreground: bool) -> Result<(), String> {
    if foreground {
        return daemon::run(&path).map_err(|e| e.to_string());
    }
    if daemon_ping(&path) {
        write_stdout(&format!(
            "daemon already running ({})\n",
            daemon::socket_path(&path).display()
        ))?;
        return Ok(());
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
            write_stdout(&format!(
                "daemon started ({})\n",
                daemon::socket_path(&abs).display()
            ))?;
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    write_stdout(&format!(
        "daemon spawned; socket not answering yet ({})\n",
        daemon::socket_path(&abs).display()
    ))?;
    Ok(())
}

fn daemon_stop(path: PathBuf) -> Result<(), String> {
    match try_daemon(&path, &Request::Shutdown) {
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

/// Prepare every local GitPixel prerequisite in one deterministic operation.
fn ready(path: PathBuf, no_daemon: bool, json: bool) -> Result<(), String> {
    let root = discover_root(&path)?;
    let index = execute(&root, Request::Status {}, no_daemon)?;
    let graph = execute(&root, Request::Graph {}, no_daemon)?;
    if !no_daemon {
        daemon_start(root.clone(), false)?;
    }
    let status = execute(&root, Request::Status {}, no_daemon)?;
    let data = serde_json::json!({
        "root": root,
        "index": index.get("index").cloned().unwrap_or(Value::Null),
        "graph": graph,
        "daemon": if no_daemon { "skipped" } else { "running" },
        "status": status,
    });
    if json {
        print_data(&data, true)
    } else {
        write_stdout(&format!(
            "ready: {}\nindex: ready\ngraph: ready\ndaemon: {}\n",
            data.get("root").and_then(Value::as_str).unwrap_or("?"),
            data.get("daemon").and_then(Value::as_str).unwrap_or("?")
        ))
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

/// Count all commits reachable from any ref (`git rev-list --count --all`).
fn rev_list_count(root: &Path) -> Option<u64> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-list", "--count", "--all"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Facts/history visibility block for `pixel status`: phase, commits indexed
/// vs the git rev-list count, diff coverage, freshness, and schema version.
fn facts_status(root: &Path) -> Option<Value> {
    let store = pixel_facts::FactsStore::open(root).ok()?;
    let state = store.index_state();
    Some(json!({
        "phase": state.phase,
        "commits_indexed": state.commits_indexed,
        "total_commits": rev_list_count(root).unwrap_or(state.total_commits),
        "diff_indexed_pct": state.diff_indexed_pct,
        "fresh": state.fresh,
        "schema_version": state.schema_version,
    }))
}

/// Parse argv, dispatch, and record the invocation to the per-repo action
/// log (`<root>/.pixel/actions.jsonl`) so a session can be self-assessed
/// later. Logging is best-effort and asynchronous — it can never fail or
/// slow down the command it observes: `discover_root` failures fall back to
/// a no-op logger, and `ActionLog::finish` bounds the writer's shutdown
/// window instead of blocking on a slow disk.
fn run() -> Result<(), String> {
    let started = std::time::Instant::now();
    let argv: Vec<String> = std::env::args().collect();
    let command_label = argv.get(1).cloned().unwrap_or_else(|| "unknown".to_string());
    let args_summary = argv[1..].join(" ");

    let cli = Cli::parse();
    let mut logger = match discover_root(Path::new(".")) {
        Ok(root) => pixel_actionlog::ActionLog::spawn_for_root(&root),
        Err(_) => pixel_actionlog::ActionLog::noop(),
    };

    let result = run_command(cli.command);

    logger.log(
        pixel_actionlog::ActionEvent::new(command_label, args_summary)
            .with_result(&result, started.elapsed()),
    );
    logger.finish();

    result
}

fn run_command(command: Command) -> Result<(), String> {
    match command {
        Command::Index {
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
                    v.get("index.base_files")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0),
                    v.get("index.delta_files")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0),
                    v.get("index.overlay_files")
                        .and_then(|x| x.as_u64())
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
            let _lock = pixel_index::BuildLock::acquire(&path)
                .map_err(|e| format!("build lock: {e}"))?;
            let ex = make_extractor(extractor, max_gram);
            let stats = build(&path, ex.as_ref()).map_err(|e| e.to_string())?;
            eprintln!(
                "indexed {} files ({} bytes) -> {} grams, shard {} bytes, {} ms",
                stats.files, stats.bytes, stats.grams, stats.shard_bytes, stats.elapsed_ms
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
            Ok(())
        }
        Command::Search {
            pattern,
            paths,
            json,
            stats,
            limit,
            offset,
            no_daemon,
            scope,
            context,
        } => run_search(pattern, paths, json, stats, limit, offset, no_daemon, scope, context),
        Command::Query { intent, path, kind, budget, json, no_daemon } => {
            run_query(intent, path, &kind, budget, json, no_daemon)
        }
        Command::Targets {
            task,
            path,
            json,
            limit,
            no_manifest,
            clear,
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
            let manifest_path = root
                .join(pixel_index::index::SHARD_DIR)
                .join("targets.json");
            let data = execute(
                &path,
                Request::Targets {
                    task: task.clone(),
                    limit,
                },
                false,
            )?;
            let active_tasks = if no_manifest {
                None
            } else {
                Some(write_targets_manifest(&manifest_path, &task, &data)?)
            };
            finish_graph_cmd(data, json, pretty_targets)?;
            if let Some(active) = active_tasks {
                eprintln!(
                    "targets manifest active: {} ({active} task(s)) — scoping enforced; run `pixel targets --clear` when the task ends",
                    manifest_path.display()
                );
            }
            Ok(())
        }
        Command::Rescue {
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
        Command::Symbol { name, path, json } => {
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
        Command::Context {
            uid,
            path,
            budget,
            json,
        } => {
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
            json,
        } => {
            let dir = match direction {
                DirectionArg::Upstream => "upstream",
                DirectionArg::Downstream => "downstream",
            };
            let data = execute(
                &path,
                Request::Impact {
                    uid_or_name,
                    direction: dir.to_string(),
                    depth,
                },
                false,
            )?;
            finish_graph_cmd(data, json, |_| None)?;
            Ok(())
        }
        Command::Uses {
            uid_or_name,
            path,
            role,
            offset,
            json,
        } => {
            let role_s = match role {
                RoleArg::Callers => "callers",
                RoleArg::Callees => "callees",
            };
            let data = execute(
                &path,
                Request::Uses {
                    uid_or_name,
                    role: role_s.to_string(),
                    offset: Some(offset),
                },
                false,
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
        Command::Trace {
            from,
            to,
            path,
            json,
        } => {
            let data = execute(&path, Request::Trace { from, to }, false)?;
            finish_graph_cmd(data, json, |_| None)?;
            Ok(())
        }
        Command::Processes { path, offset, json } => {
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
        Command::Clusters { path, offset, json } => {
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
        Command::Changes {
            path,
            base,
            offset,
            tests,
            json,
        } => {
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
        Command::Graph { path, json } => {
            let v = execute(&path, Request::Graph {}, false)?;
            eprintln!(
                "graph built in {} ms -> {}",
                v.get("elapsed_ms").and_then(Value::as_u64).unwrap_or(0),
                path.join(pixel_index::index::SHARD_DIR)
                    .join("graph.db")
                    .display()
            );
            print_data(&v, json)?;
            Ok(())
        }
        Command::Status { path, json } => {
            let mut data = execute(&path, Request::Status {}, false)?;
            // The daemon/service now attaches a rich `facts` block itself
            // (schema version, phase-A state, hunk/gram counts). Only fill in
            // the client-side fallback when talking to an older daemon that
            // doesn't send one.
            if data.get("facts").map(|f| f.is_null()).unwrap_or(true) {
                if let Some(facts) = facts_status(&path) {
                    data["facts"] = facts;
                }
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
                if let Some(f) = data.get("facts") {
                    output.push_str(&format!(
                        "facts: phase={} commits={}/{} diff_coverage={:.0}% fresh={} schema_version={}\n",
                        f.get("phase").and_then(Value::as_str).unwrap_or("?"),
                        f.get("commits_indexed").and_then(Value::as_u64).unwrap_or(0),
                        f.get("total_commits").and_then(Value::as_u64).unwrap_or(0),
                        f.get("diff_indexed_pct").and_then(Value::as_f64).unwrap_or(0.0) * 100.0,
                        f.get("fresh").and_then(Value::as_bool).unwrap_or(false),
                        f.get("schema_version").and_then(Value::as_i64).unwrap_or(0),
                    ));
                    // Only the daemon-side block carries the poisoning
                    // counters; print them when present.
                    if let (Some(h), Some(g)) = (
                        f.get("hunks_with_text").and_then(Value::as_u64),
                        f.get("diff_grams").and_then(Value::as_u64),
                    ) {
                        output.push_str(&format!(
                            "facts-text: hunks_with_text={h} diff_grams={g}\n"
                        ));
                    }
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
        Command::Ready {
            path,
            no_daemon,
            json,
        } => ready(path, no_daemon, json),
        Command::Stats { path } => {
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
                daemon_start(discover_root(&path)?, foreground)
            }
            DaemonCmd::Stop { path } => daemon_stop(discover_root(&path)?),
            DaemonCmd::Status { path } => daemon_status(discover_root(&path)?),
        },
        Command::Recall { cmd } => recall_cmd::run_recall(cmd),
        Command::Sniper { cmd } => sniper_cmd::run_sniper(cmd),
        // -------------------------------------------------------------
        // M2 — safe git mutation ops (pixel-ops)
        // -------------------------------------------------------------
        Command::Inspect { path, files, json } => {
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
                    clean.retain(|c| {
                        c.as_str().is_some_and(|p| files.iter().any(|f| f == p))
                    });
                }
                data["dirty_count"] = json!(
                    data.get("dirty").and_then(Value::as_array).map(Vec::len).unwrap_or(0)
                );
                data["clean_count"] = json!(
                    data.get("clean").and_then(Value::as_array).map(Vec::len).unwrap_or(0)
                );
            }
            print_data(&data, json)
        }
        Command::Review {
            path,
            cursor,
            byte_cap,
            json,
        } => {
            let root = discover_root(&path)?;
            let data = pixel_ops::review::review(
                &root,
                cursor.as_deref(),
                byte_cap,
            )?;
            print_data(&data, json)
        }
        Command::History {
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
            let data = pixel_ops::diff::diff(
                &root,
                &from,
                to.as_deref(),
                paths_opt,
                byte_cap,
            )?;
            print_data(&data, json)
        }
        Command::Publish {
            message,
            path,
            files,
            push,
            amend,
            expected_head,
            request_id,
            json,
        } => {
            let root = discover_root(&path)?;
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
        Command::Ship {
            message,
            path,
            files,
            remote,
            refspec,
            force_with_lease,
            request_id,
            json,
        } => {
            let root = discover_root(&path)?;
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
        Command::Branch {
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
        Command::Update {
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
        Command::Sync {
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
        Command::Resolve {
            phrase,
            path,
            limit,
            json,
        } => {
            let mut data = execute(
                &path,
                Request::Resolve {
                    phrase,
                    limit,
                },
                false,
            )?;
            if let Ok(root) = discover_root(&path) {
                enrich_resolve_matches_with_context(&mut data, &root);
            }
            print_data(&data, json)
        }
        Command::HistorySearch {
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
        Command::Lifecycle {
            path,
            file,
            token,
            json,
        } => {
            let data = execute(
                &path,
                Request::Lifecycle {
                    path: file,
                    token,
                },
                false,
            )?;
            print_data(&data, json)
        }
        Command::Excavate {
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
        Command::Reconcile {
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
        Command::Journal {
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
        Command::Install { json } => {
            let report = pixel_install::install::install(
                &pixel_install::install::InstallOptions::default(),
            )
            .map_err(|e| e.to_string())?;
            print_data(&serde_json::to_value(&report).map_err(|e| e.to_string())?, json)
        }
        Command::Doctor { path, json } => {
            let root = discover_root(&path)?;
            let report = pixel_install::doctor::doctor(&pixel_install::doctor::DoctorOptions {
                repo_root: Some(root),
                // Hand the doctor this binary's REAL clap parser so the
                // rule-vs-binary parity check dry-runs every `pixel …` line
                // documented in the installed rule text against the actual
                // CLI definition — documented-but-rejected syntax goes red.
                syntax_validator: Some(validate_cli_syntax),
                ..Default::default()
            })
            .map_err(|e| e.to_string())?;
            print_data(&serde_json::to_value(&report).map_err(|e| e.to_string())?, json)
        }
        Command::Migrate { path, json } => {
            let root = discover_root(&path)?;
            let report = pixel_install::install::migrate(&root).map_err(|e| e.to_string())?;
            print_data(&serde_json::to_value(&report).map_err(|e| e.to_string())?, json)
        }
        Command::Hook { cmd } => match cmd {
            HookCmd::Guard { path: _ } => {
                // Real PreToolUse enforcement — reads the hook JSON payload
                // from stdin itself (matching the original working
                // gitpixel-targets-guard's design) and exits 2 to block or
                // 0 to allow. Never returns.
                guard::run();
            }
            HookCmd::SessionStart { path } => {
                let root = discover_root(&path)?;
                // Emit the capability block from the live op registry —
                // `SESSION_CAPABILITIES` lives next to `Op` itself and is
                // tested for exhaustiveness against every real variant, so
                // this can never advertise a capability that doesn't exist.
                let ops: Vec<&str> = pixel_proto::op::SESSION_CAPABILITIES.to_vec();
                // The usage doctrine is a shared constant beside the op
                // registry (pixel-proto), so the injected text, the doctor's
                // scenario-consistency check, and the rule file can never
                // silently disagree on the five mandatory scenarios.
                let mut pixel = serde_json::json!({
                    "capabilities": ops,
                    "protocol_version": PROTOCOL_VERSION,
                    "usage": pixel_proto::op::SESSION_USAGE,
                });
                // Per-repo freshness: index commit, graph presence, facts
                // phase/fresh. Best-effort — if status can't be read (not a
                // git repo, index not built), the capability block still
                // stands and the repo field is simply omitted.
                //
                // The probe is hard-bounded by a deadline. "Best-effort"
                // has to mean it, because `Status` on a root that is not a
                // git repo and has no shards walks the entire tree: a
                // session started in a plain directory (a home directory,
                // `/tmp`) would otherwise hang the hook forever and the
                // agent would receive no capability block at all — the exact
                // failure this hook exists to prevent. A presence check on
                // `.git`/`.pixel` is not enough of a guard: a bare
                // `.pixel/history.db` left in a home directory by any
                // history op makes that directory look indexed.
                //
                // On timeout the block is emitted without `repo` and the
                // still-running probe dies with the process.
                let probe = {
                    let root = root.clone();
                    let (tx, rx) = std::sync::mpsc::channel();
                    std::thread::spawn(move || {
                        let _ = tx.send(execute(&root, Request::Status {}, true));
                    });
                    rx.recv_timeout(SESSION_STATUS_PROBE_TIMEOUT).ok()
                };
                if let Some(Ok(data)) = probe {
                    let mut repo = serde_json::Map::new();
                    if let Some(i) = data.get("index") {
                        repo.insert(
                            "index_commit".into(),
                            i.get("commit_oid").cloned().unwrap_or(Value::Null),
                        );
                    }
                    let graph_present = data
                        .get("graph")
                        .and_then(|g| g.get("present"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    repo.insert("graph_present".into(), Value::Bool(graph_present));
                    if let Some(f) = facts_status(&root) {
                        repo.insert(
                            "facts_phase".into(),
                            f.get("phase").cloned().unwrap_or(Value::Null),
                        );
                        repo.insert(
                            "facts_fresh".into(),
                            f.get("fresh").cloned().unwrap_or(Value::Bool(false)),
                        );
                    }
                    pixel["repo"] = Value::Object(repo);
                }
                let block = serde_json::json!({ "pixel": pixel });
                write_stdout(&serde_json::to_string_pretty(&block).map_err(|e| e.to_string())?)?;
                Ok(())
            }
            HookCmd::PromptSubmit => {
                // Task boundary detector — reads UserPromptSubmit payload
                // from stdin, embeds prompt + context, emits advisory if a
                // boundary is detected. Never returns (exits 0 or via the
                // emit function).
                prompt_submit::run();
            }
        },
        Command::Log {
            path,
            limit,
            errors_only,
            json,
            clear,
        } => run_log(&path, limit, errors_only, json, clear),
        Command::Rewrite {
            path,
            onto,
            message,
            push,
            remote,
            expected_head,
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
            };
            let data = pixel_ops::rewrite::rewrite(&root, &opts)?;
            print_data(&data, json)
        }
        Command::Provenance {
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
        Command::Branches {
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
        Command::Env { cmd } => {
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
                } => (path, json, EnvAction::Set { file, key, value, create_file }),
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
        Command::Flow { cmd } => {
            use pixel_flow::FlowAction;
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
                    execute,
                    dry_run,
                    json: _,
                } => {
                    let mut var_map = std::collections::HashMap::new();
                    for v in &vars {
                        let (k, val) = v
                            .split_once('=')
                            .ok_or_else(|| format!("--var expects key=value, got '{v}'"))?;
                        var_map.insert(k.to_string(), val.to_string());
                    }
                    // --account shortcut: resolve alias to full email and
                    // inject into the flow's account var. Try openai_account
                    // first (Codex), then google_account (Claude/others).
                    if let Some(acct) = account {
                        let resolved = resolve_account_alias(&acct);
                        // Check which var the flow expects by loading it.
                        let var_name = pixel_flow::load(&name)
                            .ok()
                            .and_then(|f| {
                                f.vars.iter().find_map(|v| {
                                    if v.name == "openai_account" {
                                        Some("openai_account")
                                    } else if v.name == "google_account" {
                                        Some("google_account")
                                    } else {
                                        None
                                    }
                                })
                            })
                            .unwrap_or("google_account");
                        var_map.insert(var_name.to_string(), resolved);
                    }
                    if execute {
                        FlowAction::Execute {
                            name,
                            vars: var_map,
                        }
                    } else {
                        FlowAction::Replay {
                            name,
                            vars: var_map,
                            dry_run,
                        }
                    }
                }
                FlowCmd::Delete { name, json: _ } => FlowAction::Delete { name },
                FlowCmd::Show { name, json: _ } => FlowAction::Show { name },
            };
            let data = pixel_flow::flow(&action)?;
            // For replay and show, the output field contains human-readable
            // text — print it directly to stdout. For everything else, use
            // the standard print_data path (JSON or pretty).
            match &action {
                FlowAction::Replay { .. } | FlowAction::Show { .. } => {
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
                    let success = data.get("success").and_then(|v| v.as_bool()).unwrap_or(false);
                    let steps = data.get("steps_executed").and_then(|v| v.as_u64()).unwrap_or(0);
                    let skipped = data.get("steps_skipped").and_then(|v| v.as_u64()).unwrap_or(0);
                    if success {
                        println!("✓ Flow executed: {} steps, {} skipped", steps, skipped);
                    } else if let Some(err) = data.get("error").and_then(|v| v.as_str()) {
                        println!("✗ Flow failed after {} steps: {}", steps, err);
                    } else {
                        println!("~ Flow completed with warnings: {} steps, {} skipped", steps, skipped);
                    }
                    Ok(())
                }
                _ => print_data(&data, true),
            }
        }
    }
}

fn run_query(
    intent: String,
    path: PathBuf,
    kind: &str,
    budget: usize,
    json_output: bool,
    no_daemon: bool,
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
    let operation = result.plan[0].operations[0].as_str();
    let evidence = match operation {
        "resolve" => {
            let phrase = intent.trim().trim_start_matches("where is `").trim_end_matches('`');
            execute(&path, Request::Resolve { phrase: phrase.into(), limit: None }, no_daemon)?
        }
        "targets" => execute(&path, Request::Targets { task: intent.clone(), limit: None }, no_daemon)?,
        "impact" => {
            let target = intent.trim().trim_start_matches("show impact of ");
            execute(&path, Request::Impact { uid_or_name: target.into(), direction: "upstream".into(), depth: Some(3) }, no_daemon)?
        }
        "excavate" => execute(&path, Request::Excavate { phrase: Some(intent.clone()), path: None, from: None, to: None, limit: None }, no_daemon)?,
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
    // Over-fetch when filtering to errors so `limit` still means "the last
    // N errors", not "the last N entries, some of which happen to be errors".
    let fetch = if errors_only { limit.max(1) * 20 } else { limit.max(1) };
    let mut events = pixel_actionlog::tail(&log_path, fetch)
        .map_err(|e| format!("read {}: {e}", log_path.display()))?;
    if errors_only {
        events.retain(|e| e.outcome == pixel_actionlog::Outcome::Error);
    }
    if events.len() > limit {
        let start = events.len() - limit;
        events.drain(0..start);
    }
    if json {
        for e in &events {
            println!(
                "{}",
                serde_json::to_string(e).map_err(|e| e.to_string())?
            );
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
                println!("{when:>8}  ok     {:<10} {} ({} ms)", e.command, e.args, e.duration_ms);
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
        Err(e) => {
            eprintln!("pixel: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Dry-run parse one `pixel …` argv (including the leading "pixel") against
/// this binary's real clap definition — nothing is executed. Used by
/// `pixel doctor`'s rule-vs-binary parity check so the installed rule text
/// can never document syntax the parser would reject.
fn validate_cli_syntax(args: &[String]) -> Result<(), String> {
    match Cli::try_parse_from(args) {
        Ok(_) => Ok(()),
        Err(e) => Err(e
            .to_string()
            .lines()
            .next()
            .unwrap_or("parse error")
            .to_string()),
    }
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
        return Err("excavate --show requires --file <repo-relative path>".to_string());
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
            base.join("home/.pixel").join(pixel_index::index::SHARD_FILE),
            b"shard",
        )
        .unwrap();
        assert_eq!(discover_root(&work).unwrap(), std::fs::canonicalize(base.join("home")).unwrap());
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
            r#"pixel excavate --phrase "<what you're looking for>" [--file <path>] [--json]"#,
            r#"pixel rescue "<what broke, in the user's words>" /path/to/repo [--json]"#,
            r#"pixel rescue --apply <oid> --file <path> /path/to/repo [--merge|--stash-first|--allow-dirty]"#,
            r#"pixel resolve "<phrase>" /path/to/repo [--json] [--limit N]"#,
            r#"pixel search "<pattern>" /path/to/repo --context 5 [--json] [--limit N]"#,
            r#"pixel reconcile /path/to/repo [--strategy report|rebase-if-clean] [--push auto|never]"#,
            r#"pixel targets "<one-line task description>" /path/to/repo [--json] [--limit N]"#,
            r#"pixel targets --clear /path/to/repo"#,
            r#"pixel impact <symbol_name_or_uid> /path/to/repo [--direction upstream|downstream] [--depth N] [--json]"#,
            r#"pixel changes /path/to/repo [--base <ref>] [--json]"#,
            r#"pixel inspect /path/to/repo [--json]"#,
            r#"pixel review /path/to/repo [--json]"#,
            r#"pixel history /path/to/repo [--ref <ref>] [--limit N] [--json]"#,
            r#"pixel diff <from> /path/to/repo [--paths <p>...] [--json]"#,
            r#"pixel publish --files <f>... --message "<msg>" --request-id <id> /path/to/repo"#,
            r#"pixel push <remote> <refspec> /path/to/repo --request-id <id>"#,
            r#"pixel ship --files <f>... --message "<msg>" <remote> <refspec> /path/to/repo --request-id <id>"#,
            r#"pixel branch <name> /path/to/repo --request-id <id>"#,
            r#"pixel sync <remote> /path/to/repo [--json]"#,
            r#"pixel update /path/to/repo --expected-head <oid> --target-oid <oid> --request-id <id>"#,
            r#"pixel status /path/to/repo"#,
            r#"pixel index --history ."#,
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

    /// A knowingly-wrong documented command must be REJECTED — this is what
    /// makes the parity check able to go red at all.
    #[test]
    fn known_bad_rule_command_lines_are_rejected() {
        for bad in [
            vec!["pixel".to_string(), "search".into(), "--no-such-flag".into()],
            vec!["pixel".to_string(), "frobnicate".into()],
            vec!["pixel".to_string(), "rescue".into(), "--limit".into(), "3".into()],
        ] {
            assert!(
                validate_cli_syntax(&bad).is_err(),
                "argv {bad:?} should be rejected by the CLI parser"
            );
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
        let ctx = enriched.get("context").and_then(Value::as_str).unwrap_or("");

        // Should contain lines 2-6 (context=2 around line 4)
        assert!(ctx.contains(">>     4: pub const FOO"), "match line should be marked with >>");
        assert!(ctx.contains("      2: line 2"), "should include 2 lines before");
        assert!(ctx.contains("      6: line 6"), "should include 2 lines after");
        assert!(!ctx.contains("line 1"), "should not include lines outside context window");
        assert!(!ctx.contains("line 7"), "should not include lines outside context window");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn enrich_with_context_zero_context_returns_original() {
        let match_val = serde_json::json!({"path": "nonexistent.rs", "line": 1, "text": "foo"});
        let enriched = enrich_with_context(&match_val, Path::new("/tmp"), 0, false, &mut HashMap::new());
        // context=0 means no enrichment — original returned
        assert!(enriched.get("context").is_none(), "context=0 should not add context field");
    }

    #[test]
    fn enrich_with_context_missing_file_returns_original() {
        let match_val = serde_json::json!({"path": "does_not_exist_xyz.rs", "line": 1, "text": "foo"});
        let enriched = enrich_with_context(&match_val, Path::new("/tmp"), 5, false, &mut HashMap::new());
        // File doesn't exist — should return original without context
        assert!(enriched.get("context").is_none(), "missing file should not add context");
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
        let ctx = enriched.get("context").and_then(Value::as_str).unwrap_or("");
        assert!(ctx.contains(">>     1: only line"), "should contain the match line");
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
}
