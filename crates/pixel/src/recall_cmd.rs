// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `gitpixel recall` — machine-wide transcript retrieval commands.

use clap::Subcommand;
use pixel_actionlog::{InProcessReason, ServeRoute, ServeStep, now_ms};
use pixel_recall::ingest::{ingest_recent, ingest_source};
use pixel_recall::model::format_ms;
use pixel_recall::search::{SearchFilters, search};
use pixel_recall::segment::SegmentSet;
use pixel_recall::sources::SourceAdapter;
use pixel_recall::sources::claude::ClaudeAdapter;
use pixel_recall::store::{RecallStore, SessionRow};
use serde_json::json;

const MAX_LIMIT: usize = 200;

#[derive(Subcommand)]
pub enum RecallCmd {
    /// Ingest transcript sources into the corpus (incremental by default).
    Index {
        /// Comma-separated sources (claude,codex,...). Default: all available.
        #[arg(long)]
        source: Option<String>,
        /// Print per-source ingest statistics.
        #[arg(long)]
        stats: bool,
        /// Rebuild the lexical segments from scratch after ingesting.
        #[arg(long)]
        full: bool,
    },
    /// Regex search over every indexed transcript turn, newest first.
    Search {
        pattern: String,
        #[arg(long)]
        agent: Option<String>,
        /// Filter by working-directory prefix (sessions that RAN here).
        #[arg(long)]
        repo: Option<String>,
        /// Relative (7d, 3w, 12h) or ISO date lower bound.
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        until: Option<String>,
        /// Restrict to a role: user, assistant, or tool.
        #[arg(long)]
        role: Option<String>,
        /// Drop harness-injected user turns; assistant and tool turns stay.
        /// Combine with `--role user` for human text only.
        #[arg(long)]
        human_only: bool,
        /// Restrict to one session (numeric id or [agent:]id-prefix).
        #[arg(long)]
        session: Option<String>,
        /// Match the pattern as whole words only.
        #[arg(long)]
        word: bool,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long)]
        json: bool,
    },
    /// List indexed sessions, newest first.
    Sessions {
        #[arg(long)]
        agent: Option<String>,
        /// Filter by working-directory prefix (sessions that RAN here).
        #[arg(long)]
        repo: Option<String>,
        /// Relative (7d, 3w, 12h) or ISO date lower bound.
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        until: Option<String>,
        /// Include subagent sessions (hidden by default).
        #[arg(long)]
        subagents: bool,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Print a session's turns. Ref: numeric id, or [agent:]session-id-prefix.
    Show {
        session_ref: String,
        /// Single turn N or range N..M (sequence numbers).
        #[arg(long)]
        turn: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Download and verify the embedding model (one-time).
    Setup,
    /// Embed pending turns into the semantic index (resumable).
    Embed {
        /// Drop all vectors and re-embed the whole corpus.
        #[arg(long)]
        rebuild: bool,
    },
    /// Natural-language hybrid search (lexical + semantic), grouped by
    /// session.
    Ask {
        query: String,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        until: Option<String>,
        #[arg(long)]
        role: Option<String>,
        /// Drop harness-injected user turns (the lexical channel always
        /// does); assistant and tool turns stay. Combine with `--role user`
        /// for human text only.
        #[arg(long)]
        human_only: bool,
        /// Session groups to return.
        #[arg(long, default_value_t = 10)]
        k: usize,
        /// Skip the semantic channel.
        #[arg(long)]
        lexical_only: bool,
        #[arg(long)]
        json: bool,
    },
    /// Bulk-export ingested sessions, one file per session, into a folder.
    Export {
        #[arg(long)]
        agent: Option<String>,
        /// Restrict to one session (numeric id or [agent:]id-prefix).
        #[arg(long)]
        session: Option<String>,
        /// Relative (7d, 3w, 12h) or ISO date lower bound.
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        until: Option<String>,
        /// Directory to write exported files into (created if missing).
        #[arg(long)]
        out: String,
        /// Output format: md or jsonl.
        #[arg(long, default_value = "md")]
        format: String,
    },
    /// Token-budgeted context pack for a query — headers, snippets, then
    /// full turns, greedily fitted for LLM consumption.
    Context {
        query: String,
        /// Token budget for the emitted pack.
        #[arg(long, default_value_t = 4000)]
        budget: usize,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        until: Option<String>,
        #[arg(long)]
        lexical_only: bool,
    },
    /// Corpus freshness, counts, and storage location.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Manage the transcript-corpus daemon (watches every CLI's transcript
    /// store, keeps the corpus and embeddings fresh, serves warm-model ask).
    Daemon {
        #[command(subcommand)]
        cmd: RecallDaemonCmd,
    },
}

#[derive(Subcommand)]
pub enum RecallDaemonCmd {
    /// Start the recall daemon (background unless --foreground).
    Start {
        #[arg(long)]
        foreground: bool,
    },
    /// Stop the running recall daemon.
    Stop,
    /// Check whether the recall daemon is running.
    Status,
}

pub fn run_recall(cmd: RecallCmd) -> Result<(), String> {
    match cmd {
        RecallCmd::Index {
            source,
            stats,
            full,
        } => run_index(source, stats, full),
        RecallCmd::Search {
            pattern,
            agent,
            repo,
            since,
            until,
            role,
            human_only,
            session,
            word,
            limit,
            offset,
            json,
        } => run_search(
            &pattern, agent, repo, since, until, role, human_only, session, word, limit, offset,
            json,
        ),
        RecallCmd::Sessions {
            agent,
            repo,
            since,
            until,
            subagents,
            limit,
            json,
        } => run_sessions(agent, repo, since, until, subagents, limit, json),
        RecallCmd::Show {
            session_ref,
            turn,
            json,
        } => run_show(&session_ref, turn.as_deref(), json),
        RecallCmd::Setup => run_setup(),
        RecallCmd::Embed { rebuild } => run_embed(rebuild),
        RecallCmd::Ask {
            query,
            agent,
            repo,
            since,
            until,
            role,
            human_only,
            k,
            lexical_only,
            json,
        } => run_ask(
            &query,
            agent,
            repo,
            since,
            until,
            role,
            human_only,
            k,
            lexical_only,
            json,
        ),
        RecallCmd::Context {
            query,
            budget,
            agent,
            repo,
            since,
            until,
            lexical_only,
        } => run_context(&query, budget, agent, repo, since, until, lexical_only),
        RecallCmd::Export {
            agent,
            session,
            since,
            until,
            out,
            format,
        } => run_export(agent, session, since, until, &out, &format),
        RecallCmd::Status { json } => run_status(json),
        RecallCmd::Daemon { cmd } => run_daemon_cmd(cmd),
    }
}

fn run_daemon_cmd(cmd: RecallDaemonCmd) -> Result<(), String> {
    let root = pixel_recall::ensure_recall_dir().map_err(|e| format!("recall dir: {e}"))?;
    match cmd {
        RecallDaemonCmd::Start { foreground } => {
            if foreground {
                let service = pixel_daemon::RecallService::open().map_err(|e| e.to_string())?;
                return pixel_daemon::daemon::run_corpus(service).map_err(|e| e.to_string());
            }
            if crate::daemon_ping(&root) {
                println!(
                    "recall daemon already running ({})",
                    pixel_daemon::daemon::socket_path(&root).display()
                );
                return Ok(());
            }
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let mut command = std::process::Command::new(exe);
            command
                .args(["recall", "daemon", "start", "--foreground"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            {
                use std::os::unix::process::CommandExt;
                command.process_group(0);
            }
            command
                .spawn()
                .map_err(|e| format!("spawn recall daemon: {e}"))?;
            for _ in 0..100 {
                if crate::daemon_ping(&root) {
                    println!(
                        "recall daemon started ({})",
                        pixel_daemon::daemon::socket_path(&root).display()
                    );
                    return Ok(());
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            println!("recall daemon spawned; socket not answering yet");
            Ok(())
        }
        RecallDaemonCmd::Stop => crate::daemon_stop(root),
        RecallDaemonCmd::Status => crate::daemon_status(root),
    }
}

/// The request half of [`routed_to_recall_daemon_at`], for a daemon that
/// answered the ping: `Ok(None)` when it cannot serve this protocol after all.
fn ask_recall_daemon(
    root: &std::path::Path,
    action: &str,
    params: serde_json::Value,
) -> Result<Option<serde_json::Value>, String> {
    let req = pixel_daemon::api::Request::Recall {
        action: action.to_string(),
        params,
    };
    let crate::DaemonRoute::Served(resp) = crate::try_daemon_inner(root, &req) else {
        return Ok(None);
    };
    let resp = *resp;
    if !resp.ok {
        return Err(resp.error_message());
    }
    Ok(Some(resp.into_data()))
}

/// Ask the recall daemon for `action`, and say how the request was served.
fn routed_to_recall_daemon(
    action: &str,
    params: serde_json::Value,
) -> (Result<Option<serde_json::Value>, String>, ServeStep) {
    routed_to_recall_daemon_at(&pixel_recall::recall_dir(), action, params)
}

/// Daemon-first execution for the hot recall ops.
///
/// `Ok(None)` = no recall daemon is listening, so the caller takes the
/// in-process path. `Err` = a daemon answered with a failure envelope, which
/// the caller names before falling back: a daemon whose vectors are corrupt
/// or whose model is incompatible is otherwise indistinguishable from "no
/// daemon", and its work (the model load included) is silently redone.
///
/// Only the recall daemon serves `Recall` (`pixel recall daemon start
/// --foreground`); a repository `Service` answers `Ping` and rejects it. The
/// probe decides whether to ask at all, and never starts a daemon: `root` is
/// a parameter rather than `recall_dir()` so tests can drive a fake socket.
///
/// The ping and the request are timed apart, so a slow probe never reads as
/// a slow answer.
fn routed_to_recall_daemon_at(
    root: &std::path::Path,
    action: &str,
    params: serde_json::Value,
) -> (Result<Option<serde_json::Value>, String>, ServeStep) {
    let (reachable, probe_ms) = crate::serve_trace::timed(|| pixel_daemon::daemon::ping_only(root));
    let (routed, request_ms) = if reachable {
        let (routed, ms) = crate::serve_trace::timed(|| ask_recall_daemon(root, action, params));
        (routed, Some(ms))
    } else {
        (Ok(None), None)
    };
    let step = recall_route_step(&routed, probe_ms, request_ms);
    (routed, step)
}

/// The step of a recall request whose ping took `probe_ms` and whose
/// request, when one was sent, took `request_ms`: served by the daemon, or
/// back in process because none answered or it answered with an error.
fn recall_route_step(
    routed: &Result<Option<serde_json::Value>, String>,
    probe_ms: u64,
    request_ms: Option<u64>,
) -> ServeStep {
    let mut step = match routed {
        Ok(Some(_)) => ServeStep::new(ServeRoute::Daemon),
        Ok(None) => ServeStep::in_process(InProcessReason::DaemonAbsent),
        Err(_) => ServeStep::in_process(InProcessReason::DaemonError),
    };
    step.probe_ms = Some(probe_ms);
    step.request_ms = request_ms;
    step
}

fn print_daemon_result(data: &serde_json::Value, json: bool) {
    if json {
        println!("{}", data.get("json").unwrap_or(&serde_json::Value::Null));
    } else {
        print!(
            "{}",
            data.get("text").and_then(|t| t.as_str()).unwrap_or("")
        );
    }
}

/// Same convention as pixel-context::estimate_tokens (len/4, ceil).
pub(crate) fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// The greedy token-budgeted packing of `recall context`: a piece is kept
/// whole when it fits what is left of the budget, and a dropped block is
/// counted for the closing `fitted:` line.
struct ContextPack {
    out: String,
    budget: usize,
    dropped: usize,
}

impl ContextPack {
    fn new(budget: usize) -> Self {
        Self {
            out: String::new(),
            budget,
            dropped: 0,
        }
    }

    fn fits(&self, piece: &str) -> bool {
        self.out.len().saturating_add(piece.len()).div_ceil(4) <= self.budget
    }

    /// Keep `piece` if it fits, else count it as a dropped block.
    fn offer(&mut self, piece: &str) {
        if self.fits(piece) {
            self.out.push_str(piece);
        } else {
            self.dropped += 1;
        }
    }

    /// Keep `piece` if it fits; an optional layer is not counted when dropped.
    fn offer_quietly(&mut self, piece: &str) {
        if self.fits(piece) {
            self.out.push_str(piece);
        }
    }

    /// The packed text with its `fitted:` summary line.
    fn finish(mut self) -> String {
        let used = estimate_tokens(&self.out);
        let (budget, dropped) = (self.budget, self.dropped);
        self.out.push_str(&format!(
            "\nfitted: budget={budget} used={used} dropped_blocks={dropped}\n"
        ));
        self.out
    }
}

#[allow(clippy::too_many_arguments)]
fn run_context(
    query: &str,
    budget: usize,
    agent: Option<String>,
    repo: Option<String>,
    since: Option<String>,
    until: Option<String>,
    lexical_only: bool,
) -> Result<(), String> {
    if !(100..=200_000).contains(&budget) {
        return Err("--budget must be between 100 and 200000 tokens".to_string());
    }
    let opening = std::time::Instant::now();
    let mut step = ServeStep::in_process(InProcessReason::NotRouted);
    let mut store = open_store()?;
    let mut segments = SegmentSet::open(&pixel_recall::segments_dir())?;
    let written = lazy_catch_up(&mut store);
    index_after_catch_up(&store, &mut segments, written)?;
    let vectors = pixel_recall::vector::VectorStore::open(&pixel_recall::vectors_dir())?;
    let now = now_ms();
    let filters = SearchFilters {
        agent,
        repo_prefix: repo.as_deref().map(expand_repo),
        since_ms: since.as_deref().map(|s| parse_time(s, now)).transpose()?,
        until_ms: until.as_deref().map(|s| parse_time(s, now)).transpose()?,
        ..Default::default()
    };
    let mut embedder_slot = if lexical_only {
        None
    } else {
        pixel_recall::embed::open_default_embedder(false).ok()
    };
    let embedder: Option<&mut (dyn pixel_recall::embed::Embedder + 'static)> =
        embedder_slot.as_deref_mut();
    step.open_ms = Some(crate::serve_trace::millis_since(opening));
    let (result, handle_ms) = crate::serve_trace::timed(|| {
        pixel_recall::ask::ask(
            &store, &segments, &vectors, embedder, query, &filters, 10, false,
        )
    });
    step.handle_ms = Some(handle_ms);
    crate::serve_trace::record(step);
    let result = result?;

    // Greedy layered packing: L0 headers always; L1 snippets; L2 full turns.
    let mut pack = ContextPack::new(budget);
    pack.out.push_str(&format!("recall context for: {query}\n"));
    if let Some(n) = &result.notice {
        pack.out.push_str(&format!("note: {n}\n"));
    }
    // L0: one header line per session group (always emitted, oldest cost first).
    for g in &result.groups {
        let ts = g.best.ts.map_or_else(|| "?".to_string(), format_ms);
        let line = format!(
            "- [{}:{} #{}] {} {} \"{}\"\n",
            g.best.agent,
            &g.best.source_session_id[..g.best.source_session_id.len().min(8)],
            g.best.session_id,
            ts,
            g.best.cwd.as_deref().unwrap_or("-"),
            g.session_title.as_deref().unwrap_or("(untitled)")
        );
        pack.offer(&line);
    }
    // L1: snippets.
    for g in &result.groups {
        let line = format!(
            "  #{} t{} {}: {}\n",
            g.best.session_id, g.best.seq, g.best.role, g.best.snippet
        );
        pack.offer_quietly(&line);
    }
    // L2: full turn texts, best-first, until the budget is spent.
    for g in &result.groups {
        let turns = store
            .turns_for_session(g.best.session_id, Some((g.best.seq, g.best.seq)))
            .map_err(|e| e.to_string())?;
        for t in turns {
            let block = format!(
                "\n--- session #{} turn {} ({}) ---\n{}\n",
                g.best.session_id, t.seq, t.role, t.text
            );
            pack.offer(&block);
        }
    }
    let out = pack.finish();
    // Composed text, not a JSON document: `write_stdout` is what keeps these
    // bytes in the output counters the metrics line reports.
    crate::write_stdout(&out)
}

fn run_setup() -> Result<(), String> {
    use pixel_recall::embed::{EmbedKind, open_default_embedder};
    eprintln!(
        "downloading embedding model into {} …",
        pixel_recall::models_dir().display()
    );
    let mut embedder = open_default_embedder(true)?;
    let probe = embedder.embed_batch(&["setup probe"], EmbedKind::Query)?;
    println!(
        "model ready: {} ({}d, probe embedding ok)",
        embedder.model_id(),
        probe[0].len()
    );
    Ok(())
}

fn run_embed(rebuild: bool) -> Result<(), String> {
    let store = open_store()?;
    let mut vectors = pixel_recall::vector::VectorStore::open(&pixel_recall::vectors_dir())?;
    if rebuild {
        vectors.clear()?;
        store.reset_embeddings().map_err(|e| e.to_string())?;
        eprintln!("vector store cleared; re-embedding entire corpus");
    }
    let mut embedder = pixel_recall::embed::open_default_embedder(false)?;
    let report = pixel_recall::embed::run_backfill(
        &store,
        &mut vectors,
        embedder.as_mut(),
        |done, backlog| eprintln!("  embedded {done} turns, {backlog} remaining"),
    )?;
    println!(
        "embedded {} turns ({} chunks) into {} segment(s), {} ms — backlog {}",
        report.turns_embedded,
        report.chunks_written,
        report.segments_written,
        report.elapsed_ms,
        report.backlog_remaining
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_ask(
    query: &str,
    agent: Option<String>,
    repo: Option<String>,
    since: Option<String>,
    until: Option<String>,
    role: Option<String>,
    human_only: bool,
    k: usize,
    lexical_only: bool,
    json: bool,
) -> Result<(), String> {
    if k == 0 || k > 50 {
        return Err("--k must be between 1 and 50".to_string());
    }
    let opening = std::time::Instant::now();
    let mut store = open_store()?;
    let mut segments = SegmentSet::open(&pixel_recall::segments_dir())?;
    let vectors = pixel_recall::vector::VectorStore::open(&pixel_recall::vectors_dir())?;
    let opened_ms = crate::serve_trace::millis_since(opening);
    let now = now_ms();
    let filters = SearchFilters {
        agent,
        repo_prefix: repo.as_deref().map(expand_repo),
        since_ms: since.as_deref().map(|s| parse_time(s, now)).transpose()?,
        until_ms: until.as_deref().map(|s| parse_time(s, now)).transpose()?,
        role,
        human_only,
        session_id: None,
    };
    // The daemon keeps the model warm — ask is much faster through it.
    let (routed, mut step) = routed_to_recall_daemon(
        "ask",
        json!({
            "query": query, "k": k, "lexical_only": lexical_only,
            "filters": filters,
        }),
    );
    step.open_ms = Some(opened_ms);
    match routed {
        Ok(Some(data)) => {
            crate::serve_trace::record(step);
            print_daemon_result(&data, json);
            return Ok(());
        }
        Ok(None) => {}
        Err(e) => eprintln!("recall daemon: {e} — running in-process instead"),
    }

    // The in-process opening resumes after the daemon wait, never counting it.
    let resumed = std::time::Instant::now();
    let written = lazy_catch_up(&mut store);
    let indexed = index_after_catch_up(&store, &mut segments, written);
    step.open_ms = Some(opened_ms.saturating_add(crate::serve_trace::millis_since(resumed)));
    if let Err(error) = indexed {
        // The route is known by now: keep it on the line even though the
        // command fails.
        crate::serve_trace::record(step);
        return Err(error);
    }
    let mut embedder_slot = if lexical_only {
        None
    } else {
        pixel_recall::embed::open_default_embedder(false).ok()
    };
    let embedder: Option<&mut (dyn pixel_recall::embed::Embedder + 'static)> =
        embedder_slot.as_deref_mut();
    step.open_ms = Some(opened_ms.saturating_add(crate::serve_trace::millis_since(resumed)));

    let (result, handle_ms) = crate::serve_trace::timed(|| {
        pixel_recall::ask::ask(
            &store, &segments, &vectors, embedder, query, &filters, k, false,
        )
    });
    step.handle_ms = Some(handle_ms);
    crate::serve_trace::record(step);
    let result = result?;
    if json {
        let out = json!({
            "groups": result.groups.iter().map(|g| json!({
                "session_id": g.best.session_id,
                "agent": g.best.agent,
                "source_session_id": g.best.source_session_id,
                "title": g.session_title,
                "cwd": g.best.cwd,
                "turn_id": g.best.turn_id,
                "seq": g.best.seq,
                "ts": g.best.ts,
                "ts_source": g.best.ts_source,
                "snippet": g.best.snippet,
                "score": g.best.score,
                "matched_lexical": g.best.matched_lexical,
                "matched_semantic": g.best.matched_semantic,
                "extra_hits": g.extra_hits,
            })).collect::<Vec<_>>(),
            "notice": result.notice,
        });
        println!("{out}");
        return Ok(());
    }
    if let Some(n) = &result.notice {
        eprintln!("note: {n}");
    }
    if result.groups.is_empty() {
        println!("no matching sessions — nothing in the corpus resembles that query");
        return Ok(());
    }
    for g in &result.groups {
        println!("{}", pixel_recall::ask::format_group(g));
    }
    Ok(())
}

fn run_export(
    agent: Option<String>,
    session: Option<String>,
    since: Option<String>,
    until: Option<String>,
    out: &str,
    format: &str,
) -> Result<(), String> {
    let format = pixel_recall::export::ExportFormat::parse(format)?;
    let mut store = open_store()?;
    lazy_catch_up(&mut store);
    let now = now_ms();
    let session_id = session
        .as_deref()
        .map(|s| resolve_session(&store, s).map(|row| row.id))
        .transpose()?;
    let filters = pixel_recall::export::ExportFilters {
        agent,
        session_id,
        since_ms: since.as_deref().map(|s| parse_time(s, now)).transpose()?,
        until_ms: until.as_deref().map(|s| parse_time(s, now)).transpose()?,
    };
    let summary =
        pixel_recall::export::export(&store, &filters, std::path::Path::new(out), format)?;
    let out = json!({
        "sessions_exported": summary.sessions_exported,
        "turns": summary.turns,
        "out_dir": summary.out_dir,
        "skipped_unresolvable_ts": summary.skipped_unresolvable_ts,
        "truncated": summary.truncated,
    });
    println!("{out}");
    Ok(())
}

fn open_store() -> Result<RecallStore, String> {
    pixel_recall::ensure_recall_dir().map_err(|e| format!("recall dir: {e}"))?;
    RecallStore::open(&pixel_recall::db_path()).map_err(|e| format!("recall.db: {e}"))
}

fn adapters(filter: Option<&str>) -> Result<Vec<Box<dyn SourceAdapter>>, String> {
    let all: Vec<Box<dyn SourceAdapter>> = vec![
        Box::new(ClaudeAdapter::new()),
        Box::new(pixel_recall::sources::codex::Adapter::new()),
        Box::new(pixel_recall::sources::cursor::Adapter::new()),
        Box::new(pixel_recall::sources::gemini::Adapter::new()),
        Box::new(pixel_recall::sources::opencode::Adapter::new()),
        Box::new(pixel_recall::sources::zcode::Adapter::new()),
        Box::new(pixel_recall::sources::devin::Adapter::new()),
        Box::new(pixel_recall::sources::pi::Adapter::new()),
    ];
    match filter {
        None => Ok(all),
        Some(csv) => {
            let wanted: Vec<&str> = csv.split(',').map(str::trim).collect();
            let known: Vec<&str> = all.iter().map(|a| a.agent()).collect();
            for w in &wanted {
                if !known.contains(w) {
                    return Err(format!(
                        "unknown source '{w}' (available: {})",
                        known.join(", ")
                    ));
                }
            }
            Ok(all
                .into_iter()
                .filter(|a| wanted.contains(&a.agent()))
                .collect())
        }
    }
}

/// A cold query ingests the last week of transcripts — enough for "what
/// did I work on" without paying for a full-history scan. Older ground
/// needs `pixel recall index`.
const LAZY_COLD_WINDOW_MS: i64 = 7 * 24 * 3600 * 1000;
/// Catch-up never looks back further than this even when the corpus is
/// stale — the window bounds the worst case.
const LAZY_MAX_WINDOW_MS: i64 = 30 * 24 * 3600 * 1000;
/// An agent whose newest ingest is this fresh cannot have missed a turn —
/// skip its discovery pass so back-to-back queries don't re-scan.
const LAZY_FRESH_MS: i64 = 30_000;

/// Look-back for an on-demand catch-up: since the agent's last ingest,
/// capped at LAZY_MAX_WINDOW_MS; a never-ingested agent gets
/// LAZY_COLD_WINDOW_MS.
fn lazy_window_ms(last_ingest_at: Option<i64>, now_ms: i64) -> i64 {
    let cutoff = last_ingest_at
        .unwrap_or(now_ms - LAZY_COLD_WINDOW_MS)
        .max(now_ms - LAZY_MAX_WINDOW_MS);
    now_ms - cutoff
}

/// Fresh enough to skip discovery: a query issued within LAZY_FRESH_MS of
/// the agent's last scan cannot have missed a turn.
fn is_fresh(last_ingest_at: Option<i64>, now_ms: i64) -> bool {
    last_ingest_at.is_some_and(|t| now_ms - t < LAZY_FRESH_MS)
}

/// Re-index after an on-demand catch-up that wrote turns — a stale
/// segment set must not serve the query that just ingested them.
fn index_after_catch_up(
    store: &RecallStore,
    segments: &mut SegmentSet,
    written: usize,
) -> Result<(), String> {
    if written != 0 {
        segments.index_new(store)?;
    }
    Ok(())
}

/// On-demand catch-up for the in-process query paths: bounded ingest per
/// agent (since its last ingest, capped) — returns the turns written so
/// callers holding a `SegmentSet` can re-index. Agents whose newest ingest
/// is fresh are skipped so repeated queries don't re-scan; per-adapter
/// failures warn and move on. Never invoked by `build-index`,
/// `prepare-repo`, or daemon start — transcripts are only scanned when a
/// recall command actually runs.
#[cfg_attr(test, mutants::skip)] // adapter list comes from the process env
fn lazy_catch_up(store: &mut RecallStore) -> usize {
    lazy_catch_up_with(store, adapters(None).unwrap_or_default())
}

/// Unit key for the per-agent scan watermark: written after every catch-up
/// pass, even when nothing was new — otherwise an agent whose transcripts
/// are unchanged (or absent) never advances `last_ingest_at` and every
/// query older than LAZY_FRESH_MS would re-walk its whole store. Not a real
/// path, so it can never collide with a unit key.
const PROBE_UNIT_KEY: &str = "@probe";

fn lazy_catch_up_with(store: &mut RecallStore, adapters: Vec<Box<dyn SourceAdapter>>) -> usize {
    let now = now_ms();
    let mut new_turns = 0usize;
    for adapter in adapters {
        let last = store.agent_last_ingest_at(adapter.agent()).ok().flatten();
        if is_fresh(last, now) {
            continue;
        }
        match ingest_recent(store, adapter.as_ref(), now, lazy_window_ms(last, now)) {
            Ok(r) => {
                new_turns += r.turns_written;
                // Advance the watermark on a clean pass even at zero writes.
                let _ = store.touch_state(
                    adapter.agent(),
                    PROBE_UNIT_KEY,
                    &pixel_recall::store::IngestState {
                        file_size: 0,
                        mtime_ms: 0,
                        bytes_ingested: 0,
                        cursor: None,
                    },
                );
            }
            Err(e) => eprintln!("recall lazy ingest ({}): {e}", adapter.agent()),
        }
    }
    if new_turns == 0 {
        return 0;
    }
    eprintln!("recall: caught up {new_turns} turns on demand");
    new_turns
}

pub fn run_index(source: Option<String>, stats: bool, full: bool) -> Result<(), String> {
    let mut store = open_store()?;
    for adapter in adapters(source.as_deref())? {
        let report = ingest_source(&mut store, adapter.as_ref()).map_err(|e| e.to_string())?;
        let line = format!(
            "{}: {} units ({} new, {} appended, {} rewritten, {} unchanged) -> {} sessions, {} turns, {} parse errors, {} ms",
            report.agent,
            report.units_seen,
            report.units_new,
            report.units_appended,
            report.units_rewritten,
            report.units_unchanged,
            report.sessions_written,
            report.turns_written,
            report.parse_errors,
            report.elapsed_ms
        );
        eprintln!("{line}");
        if stats {
            let turns = store.total_turns().map_err(|e| e.to_string())?;
            eprintln!("  corpus turns total: {turns}");
        }
    }
    let mut segments = SegmentSet::open(&pixel_recall::segments_dir())?;
    let seg_report = if full {
        segments.rebuild(&store)?
    } else {
        segments.index_new(&store)?
    };
    if seg_report.turns_indexed > 0 || stats {
        eprintln!(
            "segments: {} turns indexed into {} new segment(s), {} ms ({} segments total)",
            seg_report.turns_indexed,
            seg_report.segments_written,
            seg_report.elapsed_ms,
            segments.manifest.segments.len()
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_search(
    pattern: &str,
    agent: Option<String>,
    repo: Option<String>,
    since: Option<String>,
    until: Option<String>,
    role: Option<String>,
    human_only: bool,
    session: Option<String>,
    word: bool,
    limit: usize,
    offset: usize,
    json: bool,
) -> Result<(), String> {
    check_limit(limit)?;
    if let Some(r) = role.as_deref()
        && !matches!(r, "user" | "assistant" | "tool")
    {
        return Err("--role must be user, assistant, or tool".to_string());
    }
    let opening = std::time::Instant::now();
    let mut store = open_store()?;
    let mut segments = SegmentSet::open(&pixel_recall::segments_dir())?;
    let written = lazy_catch_up(&mut store);
    index_after_catch_up(&store, &mut segments, written)?;
    let open_ms = crate::serve_trace::millis_since(opening);
    let now = now_ms();
    // Resolve --session after the catch-up: a cold store must not fail a
    // session ref that exists on disk but was never ingested.
    let session_id = session
        .as_deref()
        .map(|s| resolve_session(&store, s).map(|row| row.id))
        .transpose()?;
    let filters = SearchFilters {
        agent,
        repo_prefix: repo.as_deref().map(expand_repo),
        since_ms: since.as_deref().map(|s| parse_time(s, now)).transpose()?,
        until_ms: until.as_deref().map(|s| parse_time(s, now)).transpose()?,
        role,
        human_only,
        session_id,
    };
    // The catch-up above runs in this process whichever route answers.
    let (routed, mut step) = routed_to_recall_daemon(
        "search",
        json!({
            "pattern": pattern, "word": word, "limit": limit, "offset": offset,
            "filters": filters,
        }),
    );
    step.open_ms = Some(open_ms);
    match routed {
        Ok(Some(data)) => {
            crate::serve_trace::record(step);
            print_daemon_result(&data, json);
            return Ok(());
        }
        Ok(None) => {}
        Err(e) => eprintln!("recall daemon: {e} — running in-process instead"),
    }
    let (result, handle_ms) = crate::serve_trace::timed(|| {
        search(&store, &segments, pattern, word, &filters, offset, limit)
    });
    step.handle_ms = Some(handle_ms);
    crate::serve_trace::record(step);
    let result = result?;
    if json {
        let out = json!({
            "hits": result.hits.iter().map(|h| json!({
                "turn_id": h.turn_id,
                "session_id": h.session_id,
                "seq": h.seq,
                "agent": h.agent,
                "source_session_id": h.source_session_id,
                "cwd": h.cwd,
                "role": h.role,
                "ts": h.ts,
                "ts_source": h.ts_source,
                "snippet": h.snippet,
                "snippet_truncated": h.snippet_truncated,
                "turn_truncated": h.turn_truncated,
            })).collect::<Vec<_>>(),
            "turns_considered": result.turns_considered,
            "truncated": result.truncated,
        });
        println!("{out}");
        return Ok(());
    }
    if result.hits.is_empty() {
        println!(
            "no matches ({} turns considered) — the pattern does not appear in the indexed corpus",
            result.turns_considered
        );
        return Ok(());
    }
    for h in &result.hits {
        let ts = h.ts.map_or_else(|| "?".to_string(), format_ms);
        let cwd = h.cwd.as_deref().unwrap_or("-");
        println!(
            "{}:{} #{} t{} {} {} {} \"{}\"",
            h.agent,
            &h.source_session_id[..h.source_session_id.len().min(8)],
            h.session_id,
            h.seq,
            ts,
            cwd,
            h.role,
            h.snippet
        );
    }
    if result.truncated {
        println!(
            "(showing {} — more matches exist, use --offset {} or narrow the query)",
            result.hits.len(),
            offset + result.hits.len()
        );
    }
    Ok(())
}

/// Parse `7d` / `3w` / `12h` / `30m` relative windows or an ISO date into a
/// unix-ms lower/upper bound.
fn parse_time(spec: &str, now_ms: i64) -> Result<i64, String> {
    let spec = spec.trim();
    if let Some(unit) = spec.chars().last()
        && let Ok(n) = spec[..spec.len() - 1].parse::<i64>()
    {
        let ms = match unit {
            'm' => n * 60_000,
            'h' => n * 3_600_000,
            'd' => n * 86_400_000,
            'w' => n * 7 * 86_400_000,
            _ => -1,
        };
        if ms >= 0 {
            return Ok(now_ms - ms);
        }
    }
    // ISO date or datetime.
    let full = if spec.len() == 10 {
        format!("{spec}T00:00:00Z")
    } else if spec.ends_with('Z') || spec.contains('+') {
        spec.to_string()
    } else {
        format!("{spec}Z")
    };
    pixel_recall::model::parse_iso_ms(&full)
        .ok_or_else(|| format!("cannot parse time '{spec}' (use 7d, 3w, 12h, or ISO date)"))
}

fn check_limit(limit: usize) -> Result<(), String> {
    if limit == 0 || limit > MAX_LIMIT {
        return Err(format!("--limit must be between 1 and {MAX_LIMIT}"));
    }
    Ok(())
}

fn expand_repo(repo: &str) -> String {
    if let Some(rest) = repo.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        return format!("{home}/{rest}");
    }
    repo.to_string()
}

fn session_line(s: &SessionRow) -> String {
    let ts = s.ts_last.map_or_else(|| "?".to_string(), format_ms);
    let ts_note = match s.ts_source {
        pixel_recall::model::TsSource::Iso | pixel_recall::model::TsSource::UnixMs => String::new(),
        other => format!(" [ts:{}]", other.as_str()),
    };
    let cwd = s.cwd.as_deref().unwrap_or("-");
    let title = s.title.as_deref().unwrap_or("(untitled)");
    let sub = if s.is_subagent { " [subagent]" } else { "" };
    format!(
        "{}:{} #{} {}{} {} ({} turns){} \"{}\"",
        s.agent,
        &s.source_session_id[..s.source_session_id.len().min(8)],
        s.id,
        ts,
        ts_note,
        cwd,
        s.turn_count,
        sub,
        title
    )
}

fn session_json(s: &SessionRow) -> serde_json::Value {
    json!({
        "id": s.id,
        "agent": s.agent,
        "source_session_id": s.source_session_id,
        "source_path": s.source_path,
        "cwd": s.cwd,
        "git_branch": s.git_branch,
        "title": s.title,
        "ts_first": s.ts_first,
        "ts_last": s.ts_last,
        "ts_source": s.ts_source.as_str(),
        "turn_count": s.turn_count,
        "is_subagent": s.is_subagent,
        "parent_session_id": s.parent_session_id,
    })
}

#[allow(clippy::too_many_arguments)]
fn run_sessions(
    agent: Option<String>,
    repo: Option<String>,
    since: Option<String>,
    until: Option<String>,
    subagents: bool,
    limit: usize,
    json: bool,
) -> Result<(), String> {
    check_limit(limit)?;
    let mut store = open_store()?;
    lazy_catch_up(&mut store);
    let now = now_ms();
    let since_ms = since.as_deref().map(|s| parse_time(s, now)).transpose()?;
    let until_ms = until.as_deref().map(|s| parse_time(s, now)).transpose()?;
    let repo = repo.as_deref().map(expand_repo);
    let rows = store
        .sessions(
            agent.as_deref(),
            repo.as_deref(),
            since_ms,
            until_ms,
            subagents,
            limit,
        )
        .map_err(|e| e.to_string())?;
    if json {
        let out = json!({
            "sessions": rows.iter().map(session_json).collect::<Vec<_>>(),
            "count": rows.len(),
        });
        println!("{out}");
    } else if rows.is_empty() {
        println!("no sessions match");
    } else {
        for s in &rows {
            println!("{}", session_line(s));
        }
    }
    Ok(())
}

fn resolve_session(store: &RecallStore, session_ref: &str) -> Result<SessionRow, String> {
    if let Ok(id) = session_ref.parse::<i64>() {
        return store
            .session_by_id(id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no session with id {id}"));
    }
    let (agent, prefix) = match session_ref.split_once(':') {
        Some((a, p)) => (Some(a), p),
        None => (None, session_ref),
    };
    let candidates = store
        .sessions_by_prefix(agent, prefix)
        .map_err(|e| e.to_string())?;
    match candidates.len() {
        0 => Err(format!("no session matches '{session_ref}'")),
        1 => Ok(candidates.into_iter().next().unwrap()),
        n => {
            let mut msg = format!("'{session_ref}' is ambiguous ({n} matches):\n");
            for c in candidates.iter().take(10) {
                msg.push_str(&format!("  {}\n", session_line(c)));
            }
            Err(msg)
        }
    }
}

fn parse_turn_range(spec: &str) -> Result<(i64, i64), String> {
    if let Some((lo, hi)) = spec.split_once("..") {
        let lo = lo.parse::<i64>().map_err(|_| "bad turn range")?;
        let hi = hi.parse::<i64>().map_err(|_| "bad turn range")?;
        Ok((lo, hi))
    } else {
        let n = spec.parse::<i64>().map_err(|_| "bad turn number")?;
        Ok((n, n))
    }
}

fn run_show(session_ref: &str, turn: Option<&str>, json: bool) -> Result<(), String> {
    let mut store = open_store()?;
    lazy_catch_up(&mut store);
    let session = resolve_session(&store, session_ref)?;
    let range = turn.map(parse_turn_range).transpose()?;
    let turns = store
        .turns_for_session(session.id, range)
        .map_err(|e| e.to_string())?;
    if json {
        let out = json!({
            "session": session_json(&session),
            "turns": turns.iter().map(|t| json!({
                "id": t.id,
                "seq": t.seq,
                "role": t.role,
                "intent_source": t.intent_source,
                "ts": t.ts,
                "text": t.text,
                "truncated": t.truncated,
            })).collect::<Vec<_>>(),
        });
        // `print_data` caps the document (structurally, so it stays one JSON
        // object with `truncated`) and counts the bytes it emits.
        return crate::print_data(&out, true);
    }
    let mut out = String::new();
    out.push_str(&session_line(&session));
    out.push('\n');
    if let Some(branch) = &session.git_branch {
        out.push_str(&format!("branch: {branch}\n"));
    }
    out.push_str(&format!("source: {}\n\n", session.source_path));
    for t in &turns {
        let ts = t.ts.map_or_else(|| "?".to_string(), format_ms);
        let intent = t
            .intent_source
            .as_deref()
            .filter(|i| *i == "orchestrator")
            .map_or("", |_| " (orchestrator)");
        let trunc = if t.truncated { " [truncated]" } else { "" };
        out.push_str(&format!(
            "--- #{} {} {}{}{} ---\n{}",
            t.seq, t.role, ts, intent, trunc, t.text
        ));
        out.push('\n');
    }
    if turns.is_empty() {
        out.push_str("(no turns in range)\n");
    }
    crate::write_stdout(&out)
}

fn run_status(json: bool) -> Result<(), String> {
    let store = open_store()?;
    let stats = store.stats().map_err(|e| e.to_string())?;
    let total_turns = store.total_turns().map_err(|e| e.to_string())?;
    let backlog = store.embed_backlog().map_err(|e| e.to_string())?;
    let db_bytes = std::fs::metadata(store.path()).map_or(0, |m| m.len());
    let segments = SegmentSet::open(&pixel_recall::segments_dir())?;
    let vectors = pixel_recall::vector::VectorStore::open(&pixel_recall::vectors_dir())?;
    let unsegmented: i64 = store
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM turns WHERE id > ?1",
            [segments.manifest.last_turn_id],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    if json {
        let out = json!({
            "location": pixel_recall::recall_dir(),
            "db_bytes": db_bytes,
            "total_turns": total_turns,
            "embed_backlog": backlog,
            "lexical_segments": segments.manifest.segments.len(),
            "unsegmented_turns": unsegmented,
            "vector_segments": vectors.meta.segments.len(),
            "vector_model": vectors.meta.model_id,
            "agents": stats.iter().map(|a| json!({
                "agent": a.agent,
                "sessions": a.sessions,
                "turns": a.turns,
                "last_ingest_at": a.last_ingest_at,
            })).collect::<Vec<_>>(),
        });
        println!("{out}");
        return Ok(());
    }
    println!(
        "corpus: {} ({:.1} MB)",
        pixel_recall::recall_dir().display(),
        db_bytes as f64 / 1_048_576.0
    );
    if stats.is_empty() {
        println!("empty — run `pixel recall index` first");
        return Ok(());
    }
    for a in &stats {
        let last = a
            .last_ingest_at
            .map_or_else(|| "never".to_string(), format_ms);
        println!(
            "{:10} {:>7} sessions {:>9} turns  last ingest {}",
            a.agent, a.sessions, a.turns, last
        );
    }
    println!("{total_turns} turns total");
    println!(
        "lexical: {} segment(s), {} turn(s) searched unindexed (tail)",
        segments.manifest.segments.len(),
        unsegmented
    );
    if vectors.meta.model_id.is_empty() {
        println!("semantic: no vectors yet — run `pixel recall setup` then `pixel recall embed`");
    } else {
        println!(
            "semantic: {} vector segment(s), model {}, embed backlog {}",
            vectors.meta.segments.len(),
            vectors.meta.model_id,
            backlog
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_context_pack_keeps_what_fits_and_counts_each_dropped_block() {
        let mut pack = super::ContextPack::new(3);
        pack.offer("abcd"); // 1 token
        pack.offer(&"x".repeat(12)); // 3 tokens: over
        pack.offer_quietly(&"y".repeat(12)); // over, not counted
        pack.offer(&"z".repeat(9)); // 3 tokens: over
        pack.offer("efgh"); // 1 token: fits exactly at 2
        pack.offer("ijkl"); // 3 == budget: fits
        assert_eq!(
            pack.finish(),
            "abcdefghijkl\nfitted: budget=3 used=3 dropped_blocks=2\n"
        );
    }

    #[test]
    fn the_context_pack_fits_a_piece_that_fills_the_budget_exactly() {
        let mut pack = super::ContextPack::new(100);
        pack.offer(&"p".repeat(397));
        pack.offer("abc"); // 400 bytes in all: exactly 100 tokens
        pack.offer("d"); // 401 bytes: 101 tokens, over
        let out = pack.finish();
        assert!(
            out.starts_with(&format!("{}abc\n", "p".repeat(397))),
            "{out}"
        );
        assert!(
            out.ends_with("\nfitted: budget=100 used=100 dropped_blocks=1\n"),
            "{out}"
        );
    }

    use super::*;
    use pixel_recall::model::{IntentSource, Role, TsSource, UnifiedSession, UnifiedTurn};
    use pixel_recall::sources::{
        Change, IngestError, ParseOutput, ParsedSession, SessionOp, SourceUnit,
    };
    use pixel_recall::store::IngestState;
    use std::cell::Cell;
    use std::rc::Rc;

    /// The clock helper returns the wall clock in milliseconds: bracketed
    /// by two reads and above 2020-01-01, which rules out a constant.
    #[test]
    fn now_ms_is_the_wall_clock_in_milliseconds() {
        let read = || {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64
        };
        let before = read();
        let now = now_ms();
        let after = read();
        assert!(
            before <= now && now <= after,
            "{before} <= {now} <= {after}"
        );
        assert!(now > 1_577_836_800_000, "after 2020-01-01");
    }

    /// A temp root for the daemon-socket tests: `socket_path` keys off the
    /// canonical root, so it has to exist.
    fn scratch_root(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("pixel-recall-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root.canonicalize().unwrap()
    }

    /// A cold query gets the one-week window, a recent watermark narrows
    /// the catch-up to the delta, and a stale watermark is capped so the
    /// pass stays bounded instead of scanning the whole transcript history.
    #[test]
    fn lazy_window_bounds_cold_recent_and_stale() {
        let now = 100_000_000_000i64;
        assert_eq!(lazy_window_ms(None, now), 604_800_000); // 7 days
        assert_eq!(lazy_window_ms(Some(now - 60_000), now), 60_000);
        assert_eq!(lazy_window_ms(Some(1), now), 2_592_000_000); // 30 days
        // A watermark in the future cannot produce work.
        assert!(lazy_window_ms(Some(now + 5_000), now) <= 0);
    }

    /// The freshness gate is exact: at the boundary the agent is stale and
    /// gets walked; one millisecond inside it, it is skipped.
    #[test]
    fn is_fresh_boundary_is_exact() {
        let now = 100_000_000_000i64;
        assert!(!is_fresh(None, now));
        assert!(is_fresh(Some(now - LAZY_FRESH_MS + 1), now));
        assert!(!is_fresh(Some(now - LAZY_FRESH_MS), now));
        assert!(!is_fresh(Some(1), now));
    }

    /// Minimal adapter: counts its calls so the tests can prove the
    /// freshness skip never walks a fresh source, and emits one turn per
    /// unit so indexing is observable through `search`.
    struct StubAdapter {
        agent: &'static str,
        units: Vec<SourceUnit>,
        discovered: Rc<Cell<u32>>,
        parsed: Rc<Cell<u32>>,
    }

    impl SourceAdapter for StubAdapter {
        fn agent(&self) -> &'static str {
            self.agent
        }

        fn discover(&self) -> Result<Vec<SourceUnit>, IngestError> {
            self.discovered.set(self.discovered.get() + 1);
            Ok(self.units.clone())
        }

        fn parse(
            &self,
            unit: &SourceUnit,
            _change: Change,
            _state: Option<&IngestState>,
        ) -> Result<ParseOutput, IngestError> {
            self.parsed.set(self.parsed.get() + 1);
            Ok(ParseOutput {
                sessions: vec![ParsedSession {
                    op: SessionOp::Replace,
                    session: UnifiedSession {
                        agent: self.agent,
                        source_session_id: format!("s-{}", unit.unit_key),
                        source_path: unit.unit_key.clone(),
                        cwd: None,
                        git_branch: None,
                        title: None,
                        ts_source: TsSource::Iso,
                        is_subagent: false,
                        parent_source_session_id: None,
                    },
                    turns: vec![UnifiedTurn {
                        role: Role::User,
                        intent_source: Some(IntentSource::Human),
                        ts: Some(unit.mtime_ms),
                        text: "stub turn text".to_string(),
                        truncated: false,
                        source_byte_start: None,
                        source_byte_len: None,
                    }],
                }],
                skipped_records: 0,
                consumed_bytes: unit.size,
                cursor: None,
            })
        }
    }

    fn stub(
        agent: &'static str,
        units: Vec<SourceUnit>,
        discovered: &Rc<Cell<u32>>,
        parsed: &Rc<Cell<u32>>,
    ) -> Box<dyn SourceAdapter> {
        Box::new(StubAdapter {
            agent,
            units,
            discovered: discovered.clone(),
            parsed: parsed.clone(),
        })
    }

    /// A fresh watermark skips discovery entirely: a back-to-back query
    /// must not walk the source again.
    #[test]
    fn lazy_catch_up_skips_agents_with_fresh_watermarks() {
        let root = scratch_root("lazy-skip");
        let mut store = RecallStore::open(&root.join("recall.db")).unwrap();
        store
            .touch_state(
                "stub",
                "/u",
                &IngestState {
                    file_size: 1,
                    mtime_ms: 1,
                    bytes_ingested: 1,
                    cursor: None,
                },
            )
            .unwrap();
        let (discovered, parsed) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
        let written = lazy_catch_up_with(
            &mut store,
            vec![stub("stub", Vec::new(), &discovered, &parsed)],
        );
        assert_eq!(written, 0);
        assert_eq!(discovered.get(), 0, "fresh agent must not be walked");
    }

    /// A cold agent is ingested within the lazy window, the new turns are
    /// searchable once the caller re-indexes, and the next catch-up
    /// reparses nothing.
    #[test]
    fn lazy_catch_up_ingests_indexes_and_stays_incremental() {
        let root = scratch_root("lazy-cold");
        let mut store = RecallStore::open(&root.join("recall.db")).unwrap();
        let mut segments = SegmentSet::open(&root.join("seg")).unwrap();
        let unit = || SourceUnit {
            unit_key: "u1".to_string(),
            path: root.join("u1"),
            size: 42,
            mtime_ms: now_ms(),
        };
        let (discovered, parsed) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
        let written = lazy_catch_up_with(
            &mut store,
            vec![stub("stub", vec![unit()], &discovered, &parsed)],
        );
        assert_eq!((written, discovered.get(), parsed.get()), (1, 1, 1));
        // The caller re-indexes only when something was written.
        index_after_catch_up(&store, &mut segments, written).unwrap();
        let hits = search(
            &store,
            &segments,
            "stub turn text",
            false,
            &SearchFilters::default(),
            0,
            10,
        )
        .unwrap();
        assert_eq!(hits.hits.len(), 1, "just-ingested turn must be searchable");
        // The recorded watermark is now fresh: a second catch-up walks
        // nothing and reparses nothing.
        let written = lazy_catch_up_with(
            &mut store,
            vec![stub("stub", vec![unit()], &discovered, &parsed)],
        );
        assert_eq!(
            (written, discovered.get(), parsed.get()),
            (0, 1, 1),
            "back-to-back catch-up must be a no-op"
        );
    }

    /// An agent whose units all fall outside the window writes nothing —
    /// but the probe still advances its watermark, so the next catch-up
    /// skips discovery instead of re-walking an unchanged store.
    #[test]
    fn lazy_catch_up_probe_skips_agents_with_nothing_to_ingest() {
        let root = scratch_root("lazy-probe");
        let mut store = RecallStore::open(&root.join("recall.db")).unwrap();
        let ancient = || SourceUnit {
            unit_key: "old".to_string(),
            path: root.join("old"),
            size: 42,
            mtime_ms: 1, // far outside the cold window
        };
        let (discovered, parsed) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
        let written = lazy_catch_up_with(
            &mut store,
            vec![stub("stub", vec![ancient()], &discovered, &parsed)],
        );
        assert_eq!((written, discovered.get(), parsed.get()), (0, 1, 0));
        let written = lazy_catch_up_with(
            &mut store,
            vec![stub("stub", vec![ancient()], &discovered, &parsed)],
        );
        assert_eq!(
            (written, discovered.get()),
            (0, 1),
            "the probe watermark must skip the second walk"
        );
    }

    /// `index_after_catch_up` is the gate: zero writes leave the segment
    /// set alone, a nonzero count indexes the just-ingested turns so the
    /// query that triggered the catch-up can see them.
    #[test]
    fn index_after_catch_up_indexes_only_when_turns_were_written() {
        let root = scratch_root("lazy-index-gate");
        let mut store = RecallStore::open(&root.join("recall.db")).unwrap();
        let mut segments = SegmentSet::open(&root.join("seg")).unwrap();
        // Seed one unindexed turn: search would still find it through the
        // always-scanned tail, so segment state — not hits — is the only
        // thing that can prove whether the gate ran the indexer.
        let (discovered, parsed) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
        let unit = SourceUnit {
            unit_key: "u1".to_string(),
            path: root.join("u1"),
            size: 42,
            mtime_ms: now_ms(),
        };
        let written = lazy_catch_up_with(
            &mut store,
            vec![stub("stub", vec![unit], &discovered, &parsed)],
        );
        assert!(written > 0);
        // Nothing written on this pass: no index run — the manifest
        // watermark must not move and no shard may appear, even with an
        // unindexed turn waiting in the store.
        index_after_catch_up(&store, &mut segments, 0).unwrap();
        assert_eq!(segments.manifest.last_turn_id, 0);
        assert!(segments.manifest.segments.is_empty());
        // A catch-up that wrote turns must index them.
        index_after_catch_up(&store, &mut segments, written).unwrap();
        assert_eq!(segments.manifest.segments.len(), 1);
        assert!(segments.manifest.last_turn_id > 0);
    }

    /// A recall-daemon socket that answers the client's `Ping` and then one
    /// canned `Recall` envelope: the wire contract, with no corpus behind it.
    fn fake_recall_daemon(
        root: &std::path::Path,
        answer: pixel_daemon::Response,
    ) -> std::thread::JoinHandle<()> {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        let listener = UnixListener::bind(pixel_daemon::daemon::socket_path(root)).unwrap();
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
            // The client probes with `ping_only` on one connection and opens
            // another for the op, so serve connections until one carries the
            // `Recall`. Poll with a deadline: a client that never gets that
            // far must fail its own assertion, not hang here.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut served = false;
            while !served && std::time::Instant::now() < deadline {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    continue;
                };
                // A non-blocking listener hands back a non-blocking socket on
                // BSD: reset it, or the read races the client's write.
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                while !served {
                    let mut line = String::new();
                    let Ok(n) = reader.read_line(&mut line) else {
                        break;
                    };
                    if n == 0 {
                        break;
                    }
                    let reply = match serde_json::from_str::<pixel_daemon::Request>(&line).unwrap()
                    {
                        pixel_daemon::Request::Ping => pixel_daemon::Response::success(
                            "ping",
                            json!({"pong": true, "protocol_version": pixel_daemon::api::PROTOCOL_VERSION}),
                        ),
                        pixel_daemon::Request::Recall { .. } => {
                            served = true;
                            answer.clone()
                        }
                        other => panic!("unexpected request: {other:?}"),
                    };
                    writeln!(stream, "{}", serde_json::to_string(&reply).unwrap()).unwrap();
                }
            }
        })
    }

    /// Each recall outcome is logged under its own route and phase: an
    /// absent daemon cost a probe, an answer — even an error — a round trip.
    #[test]
    fn recall_route_step_names_the_route_and_times_the_right_phase() {
        let served = recall_route_step(&Ok(Some(json!({}))), 2, Some(7));
        assert_eq!(
            (
                served.route,
                served.reason,
                served.probe_ms,
                served.request_ms
            ),
            (ServeRoute::Daemon, None, Some(2), Some(7))
        );
        let absent = recall_route_step(&Ok(None), 3, None);
        assert_eq!(
            (
                absent.route,
                absent.reason,
                absent.probe_ms,
                absent.request_ms
            ),
            (
                ServeRoute::InProcess,
                Some(InProcessReason::DaemonAbsent),
                Some(3),
                None
            )
        );
        let failed = recall_route_step(&Err("corrupt vectors".into()), 4, Some(9));
        assert_eq!(
            (
                failed.route,
                failed.reason,
                failed.probe_ms,
                failed.request_ms
            ),
            (
                ServeRoute::InProcess,
                Some(InProcessReason::DaemonError),
                Some(4),
                Some(9)
            )
        );
    }

    /// No recall daemon listening: the daemon path declines with no error, so
    /// the caller runs in-process.
    #[test]
    fn recall_daemon_path_declines_when_no_daemon_listens() {
        let root = scratch_root("no-daemon");
        let (routed, step) =
            routed_to_recall_daemon_at(&root, "search", json!({"pattern": "needle"}));
        assert_eq!(routed, Ok(None));
        assert_eq!(step.reason, Some(InProcessReason::DaemonAbsent));
        assert!(
            step.probe_ms.is_some() && step.request_ms.is_none(),
            "{step:?}"
        );
    }

    /// A daemon's answer is handed back as the op's payload, unchanged.
    #[test]
    fn recall_daemon_answer_is_returned_as_is() {
        let root = scratch_root("answering");
        let server = fake_recall_daemon(
            &root,
            pixel_daemon::Response::success(
                "recall",
                json!({"json": {"hits": []}, "text": "no matches"}),
            ),
        );
        let (routed, step) =
            routed_to_recall_daemon_at(&root, "search", json!({"pattern": "needle"}));
        assert_eq!(
            routed,
            Ok(Some(json!({"json": {"hits": []}, "text": "no matches"})))
        );
        assert_eq!(step.route, ServeRoute::Daemon);
        assert!(
            step.probe_ms.is_some() && step.request_ms.is_some(),
            "{step:?}"
        );
        server.join().unwrap();
        let _ = std::fs::remove_file(pixel_daemon::daemon::socket_path(&root));
    }

    /// A daemon that answers with a failure envelope is an error to report,
    /// not "no daemon": the in-process fallback must not be silent.
    #[test]
    fn recall_daemon_failure_is_reported_instead_of_swallowed() {
        let root = scratch_root("failing");
        let server = fake_recall_daemon(
            &root,
            pixel_daemon::api::failure_response("recall", "vector store is corrupt"),
        );
        let (routed, step) = routed_to_recall_daemon_at(&root, "ask", json!({"query": "needle"}));
        let err = routed.unwrap_err();
        assert_eq!(step.reason, Some(InProcessReason::DaemonError));
        assert!(err.contains("vector store is corrupt"), "{err}");
        server.join().unwrap();
        let _ = std::fs::remove_file(pixel_daemon::daemon::socket_path(&root));
    }

    fn row() -> SessionRow {
        SessionRow {
            id: 42,
            agent: "claude".to_string(),
            source_session_id: "0123abcd-0000-4000-8000-000000000001".to_string(),
            source_path: "/x.jsonl".to_string(),
            cwd: Some("/work/pixel".to_string()),
            git_branch: None,
            title: Some("fix the engine".to_string()),
            first_user_prompt: None,
            ts_first: None,
            ts_last: Some(1_760_000_000_000),
            ts_source: TsSource::Iso,
            turn_count: 7,
            is_subagent: false,
            parent_session_id: None,
        }
    }

    /// The one-line session summary is what `sessions` and `show` print;
    /// every field an agent uses to pick or cite a session is on it.
    #[test]
    fn session_line_carries_ref_time_cwd_count_and_title() {
        assert_eq!(
            session_line(&row()),
            "claude:0123abcd #42 2025-10-09 08:53 /work/pixel (7 turns) \"fix the engine\""
        );
        let mut r = row();
        r.ts_last = None;
        r.ts_source = TsSource::Mtime;
        r.cwd = None;
        r.title = None;
        r.is_subagent = true;
        assert_eq!(
            session_line(&r),
            "claude:0123abcd #42 ? [ts:mtime] - (7 turns) [subagent] \"(untitled)\""
        );
    }
}
