//! gitpixel CLI — index/search plus the graph command surface, speaking to a
//! per-root daemon over its Unix socket when one is up, else in-process.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};

mod recall_cmd;
mod rescue_cmd;
mod sniper_cmd;
use pixel_index::index::{build, shard_path};
use pixel_index::shard::Shard;
use pixel_index::{Crc32Weigher, GramExtractor, SparseGramExtractor, TrigramExtractor};
use pixel_daemon::api::{PROTOCOL_VERSION, Request, Response, Service};
use pixel_daemon::daemon;
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
        /// Ranking scope: `code` reranks matches by file-level signals
        /// (filename match, symbol match, content density) via pixel-rank's
        /// RRF without changing the hit set. Default: unranked (path/line
        /// order).
        #[arg(long)]
        scope: Option<String>,
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
    /// One-look error capture: query the sniper error sink (CLI + MCP).
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
        /// Files to stage (repo-relative).
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
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Files to stage (repo-relative).
        #[arg(long = "files")]
        files: Vec<String>,
        remote: String,
        refspec: String,
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
        /// Restrict to a repo-relative path.
        #[arg(long)]
        file: Option<String>,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
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
    /// Idempotent install: register the pixel MCP server, hooks, agent-config.
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
    if resp.ok {
        Ok(resp.into_data())
    } else {
        Err(resp.error_message())
    }
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

/// Write the enforcement manifest atomically (tmp + rename).
fn write_targets_manifest(manifest_path: &Path, task: &str, data: &Value) -> Result<(), String> {
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
    let manifest = serde_json::json!({
        "version": 1,
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
        "files": files,
    });
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
    Ok(())
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
    // shadow the real repo root. `.pixel` alone only anchors non-git trees.
    let mut nearest_index: Option<PathBuf> = None;
    let mut cur = start.clone();
    loop {
        if cur.join(".git").exists() {
            return Ok(cur);
        }
        if nearest_index.is_none() && cur.join(pixel_index::index::SHARD_DIR).is_dir() {
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
        if json {
            output.push_str(
                &serde_json::json!({"path": path, "line": line, "text": text}).to_string(),
            );
            output.push('\n');
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
) -> Result<(), String> {
    let groups = group_by_root(&paths)?;
    let multi_root = groups.len() > 1;
    for (root, rels) in groups {
        let whole_repo = rels.iter().any(String::is_empty);
        let req_paths = if whole_repo { None } else { Some(rels) };
        run_search_one(
            &pattern, &root, req_paths, multi_root, json, stats, limit, offset, no_daemon, scope.clone(),
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
    if qualify {
        // Multiple repos in one invocation: qualify paths with the root so
        // output lines stay unambiguous.
        let qualified: Vec<Value> = matches
            .iter()
            .map(|m| {
                let mut m = m.clone();
                if let Some(rel) = m.get("path").and_then(Value::as_str) {
                    let full = root.join(rel).display().to_string();
                    m["path"] = Value::String(full);
                }
                m
            })
            .collect();
        print_search_matches(&qualified, json)?;
    } else {
        print_search_matches(matches, json)?;
    }
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

fn run() -> Result<(), String> {
    match Cli::parse().command {
        Command::Index {
            path,
            extractor,
            max_gram,
        } => {
            let path = discover_root(&path)?;
            let ex = make_extractor(extractor, max_gram);
            let stats = build(&path, ex.as_ref()).map_err(|e| e.to_string())?;
            eprintln!(
                "indexed {} files ({} bytes) -> {} grams, shard {} bytes, {} ms",
                stats.files, stats.bytes, stats.grams, stats.shard_bytes, stats.elapsed_ms
            );
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
        } => run_search(pattern, paths, json, stats, limit, offset, no_daemon, scope),
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
            if !no_manifest {
                write_targets_manifest(&manifest_path, &task, &data)?;
            }
            finish_graph_cmd(data, json, pretty_targets)?;
            if !no_manifest {
                eprintln!(
                    "targets manifest active: {} — scoping enforced; run `pixel targets --clear` when the task ends",
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
            json,
        } => {
            let data = execute(
                &path,
                Request::Changes {
                    base,
                    offset: Some(offset),
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
            let data = execute(&path, Request::Status {}, false)?;
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
            request_id,
            json,
        } => {
            let root = discover_root(&path)?;
            let data = pixel_ops::ship::ship(
                &root,
                &message,
                &files,
                &remote,
                &refspec,
                &request_id,
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
            let data = execute(
                &path,
                Request::Resolve {
                    phrase,
                    limit,
                },
                false,
            )?;
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
            json,
        } => {
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
            json,
        } => {
            let data = execute(
                &path,
                Request::Reconcile {
                    strategy: Some(strategy),
                    push: Some(push),
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
            HookCmd::Guard { path } => {
                let root = discover_root(&path)?;
                // Targets enforcement guard: emit the active manifest if any.
                let manifest = root
                    .join(pixel_index::index::SHARD_DIR)
                    .join("targets.json");
                if manifest.exists() {
                    let body = std::fs::read_to_string(&manifest).map_err(|e| e.to_string())?;
                    write_stdout(&body)?;
                }
                Ok(())
            }
            HookCmd::SessionStart { path } => {
                let _ = discover_root(&path)?;
                // Emit the capability block from the live op registry.
                let ops: Vec<&str> = [
                    "ping", "search", "targets", "symbol", "context", "impact", "uses",
                    "trace", "processes", "clusters", "changes", "graph", "status",
                    "resolve", "history", "lifecycle", "excavate", "reconcile", "journal",
                    "inspect", "review", "diff", "history_op", "publish", "push", "ship",
                    "branch_op", "update", "sync",
                ]
                .to_vec();
                let block = serde_json::json!({
                    "pixel": {
                        "capabilities": ops,
                        "protocol_version": PROTOCOL_VERSION,
                    }
                });
                write_stdout(&serde_json::to_string_pretty(&block).map_err(|e| e.to_string())?)?;
                Ok(())
            }
        },
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
