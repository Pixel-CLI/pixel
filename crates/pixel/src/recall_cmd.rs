//! `gitpixel recall` — machine-wide transcript retrieval commands.

use clap::Subcommand;
use pixel_recall::ingest::ingest_source;
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
        /// Only human-authored user turns (skip harness-injected text).
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
    /// MAX TEST: rank remembered keywords by rarity — the term with the
    /// fewest matches pins the session you're hunting for fastest.
    Maxtest {
        /// Comma-separated keywords (matched as whole words).
        keywords: String,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        until: Option<String>,
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
            &query, agent, repo, since, until, role, human_only, k, lexical_only, json,
        ),
        RecallCmd::Maxtest {
            keywords,
            agent,
            repo,
            since,
            until,
            json,
        } => run_maxtest(&keywords, agent, repo, since, until, json),
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
                let service =
                    pixel_daemon::RecallService::open().map_err(|e| e.to_string())?;
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

/// Daemon-first execution for the hot recall ops; None = no daemon (or the
/// daemon errored) — caller falls back to the in-process path.
fn try_recall_daemon(action: &str, params: serde_json::Value) -> Option<serde_json::Value> {
    let root = pixel_recall::recall_dir();
    let req = pixel_daemon::api::Request::Recall {
        action: action.to_string(),
        params,
    };
    let resp = crate::try_daemon(&root, &req)?;
    if resp.ok { Some(resp.into_data()) } else { None }
}

fn print_daemon_result(data: &serde_json::Value, json: bool) {
    if json {
        println!("{}", data.get("json").unwrap_or(&serde_json::Value::Null));
    } else {
        print!("{}", data.get("text").and_then(|t| t.as_str()).unwrap_or(""));
    }
}

/// Same convention as pixel-context::estimate_tokens (len/4, ceil).
fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
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
    if budget < 100 || budget > 200_000 {
        return Err("--budget must be between 100 and 200000 tokens".to_string());
    }
    let store = open_store()?;
    let segments = SegmentSet::open(&pixel_recall::segments_dir())?;
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
    let result =
        pixel_recall::ask::ask(&store, &segments, &vectors, embedder, query, &filters, 10)?;

    // Greedy layered packing: L0 headers always; L1 snippets; L2 full turns.
    let mut out = String::new();
    let mut dropped = 0usize;
    let header = format!("recall context for: {query}\n");
    out.push_str(&header);
    if let Some(n) = &result.notice {
        out.push_str(&format!("note: {n}\n"));
    }
    // L0: one header line per session group (always emitted, oldest cost first).
    for g in &result.groups {
        let ts = g.best.ts.map(format_ms).unwrap_or_else(|| "?".to_string());
        let line = format!(
            "- [{}:{} #{}] {} {} \"{}\"\n",
            g.best.agent,
            &g.best.source_session_id[..g.best.source_session_id.len().min(8)],
            g.best.session_id,
            ts,
            g.best.cwd.as_deref().unwrap_or("-"),
            g.session_title.as_deref().unwrap_or("(untitled)")
        );
        if estimate_tokens(&out) + estimate_tokens(&line) <= budget {
            out.push_str(&line);
        } else {
            dropped += 1;
        }
    }
    // L1: snippets.
    for g in &result.groups {
        let line = format!(
            "  #{} t{} {}: {}\n",
            g.best.session_id, g.best.seq, g.best.role, g.best.snippet
        );
        if estimate_tokens(&out) + estimate_tokens(&line) <= budget {
            out.push_str(&line);
        }
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
            if estimate_tokens(&out) + estimate_tokens(&block) <= budget {
                out.push_str(&block);
            } else {
                dropped += 1;
            }
        }
    }
    let used = estimate_tokens(&out);
    out.push_str(&format!(
        "\nfitted: budget={budget} used={used} dropped_blocks={dropped}\n"
    ));
    print!("{out}");
    Ok(())
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
    let store = open_store()?;
    let segments = SegmentSet::open(&pixel_recall::segments_dir())?;
    let vectors = pixel_recall::vector::VectorStore::open(&pixel_recall::vectors_dir())?;
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
    if let Some(data) = try_recall_daemon(
        "ask",
        json!({
            "query": query, "k": k, "lexical_only": lexical_only,
            "filters": filters,
        }),
    ) {
        print_daemon_result(&data, json);
        return Ok(());
    }

    let mut embedder_slot = if lexical_only {
        None
    } else {
        pixel_recall::embed::open_default_embedder(false).ok()
    };
    let embedder: Option<&mut (dyn pixel_recall::embed::Embedder + 'static)> =
        embedder_slot.as_deref_mut();

    let result = pixel_recall::ask::ask(
        &store, &segments, &vectors, embedder, query, &filters, k,
    )?;
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

fn run_maxtest(
    keywords: &str,
    agent: Option<String>,
    repo: Option<String>,
    since: Option<String>,
    until: Option<String>,
    json: bool,
) -> Result<(), String> {
    let terms: Vec<&str> = keywords
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    if terms.is_empty() {
        return Err("no keywords given (comma-separated list expected)".to_string());
    }
    if terms.len() > 10 {
        return Err("at most 10 keywords".to_string());
    }
    let store = open_store()?;
    let segments = SegmentSet::open(&pixel_recall::segments_dir())?;
    let now = now_ms();
    let filters = SearchFilters {
        agent,
        repo_prefix: repo.as_deref().map(expand_repo),
        since_ms: since.as_deref().map(|s| parse_time(s, now)).transpose()?,
        until_ms: until.as_deref().map(|s| parse_time(s, now)).transpose()?,
        ..Default::default()
    };
    // Escape each keyword: maxtest terms are literals, not regexes.
    let mut ranked: Vec<(String, usize, std::collections::HashSet<i64>)> = Vec::new();
    for term in &terms {
        let escaped = regex::escape(term);
        let (turns, sessions) =
            pixel_recall::search::count_matches(&store, &segments, &escaped, true, &filters)?;
        ranked.push((term.to_string(), turns, sessions));
    }
    ranked.sort_by_key(|(_, _, sessions)| sessions.len());
    if json {
        let out = json!({
            "ranking": ranked.iter().map(|(t, turns, sess)| json!({
                "term": t, "turns": turns, "sessions": sess.len(),
            })).collect::<Vec<_>>(),
        });
        println!("{out}");
        return Ok(());
    }
    println!("keyword ranking (rarest first — rarest pins the session):");
    for (term, turns, sessions) in &ranked {
        if sessions.is_empty() {
            println!("  {term:24} 0 matches — term does not appear in the corpus");
        } else {
            println!(
                "  {term:24} {} sessions, {} turns",
                sessions.len(),
                turns
            );
        }
    }
    // The pin: intersect the two rarest non-empty terms (or take the single
    // rarest) and show those sessions for recognition.
    let nonempty: Vec<&(String, usize, std::collections::HashSet<i64>)> =
        ranked.iter().filter(|(_, _, s)| !s.is_empty()).collect();
    let pin: Vec<i64> = match nonempty.as_slice() {
        [] => Vec::new(),
        [only] => only.2.iter().copied().collect(),
        [first, second, ..] => first.2.intersection(&second.2).copied().collect(),
    };
    if pin.is_empty() {
        println!("\nno session contains the rarest terms together — widen the window or try other keywords");
        return Ok(());
    }
    println!("\npinned sessions ({}):", pin.len());
    let mut pinned: Vec<SessionRow> = Vec::new();
    for id in pin.iter().take(10) {
        if let Some(row) = store.session_by_id(*id).map_err(|e| e.to_string())? {
            pinned.push(row);
        }
    }
    pinned.sort_by(|a, b| b.ts_last.cmp(&a.ts_last));
    for row in &pinned {
        println!("  {}", session_line(row));
    }
    if pin.len() > 10 {
        println!("  … and {} more (narrow with --repo/--since)", pin.len() - 10);
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
    let store = open_store()?;
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

fn run_index(source: Option<String>, stats: bool, full: bool) -> Result<(), String> {
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
    let store = open_store()?;
    let segments = SegmentSet::open(&pixel_recall::segments_dir())?;
    let now = now_ms();
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
    if let Some(data) = try_recall_daemon(
        "search",
        json!({
            "pattern": pattern, "word": word, "limit": limit, "offset": offset,
            "filters": filters,
        }),
    ) {
        print_daemon_result(&data, json);
        return Ok(());
    }
    let result = search(&store, &segments, pattern, word, &filters, offset, limit)?;
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
        let ts = h.ts.map(format_ms).unwrap_or_else(|| "?".to_string());
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

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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
    let ts = s.ts_last.map(format_ms).unwrap_or_else(|| "?".to_string());
    let ts_note = match s.ts_source {
        pixel_recall::model::TsSource::Iso | pixel_recall::model::TsSource::UnixMs => {
            String::new()
        }
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
    let store = open_store()?;
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
    let store = open_store()?;
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
        println!("{out}");
        return Ok(());
    }
    println!("{}", session_line(&session));
    if let Some(branch) = &session.git_branch {
        println!("branch: {branch}");
    }
    println!("source: {}", session.source_path);
    println!();
    for t in &turns {
        let ts = t.ts.map(format_ms).unwrap_or_else(|| "?".to_string());
        let intent = t
            .intent_source
            .as_deref()
            .filter(|i| *i == "orchestrator")
            .map(|_| " (orchestrator)")
            .unwrap_or("");
        let trunc = if t.truncated { " [truncated]" } else { "" };
        println!("--- #{} {} {}{}{} ---", t.seq, t.role, ts, intent, trunc);
        println!("{}", t.text);
    }
    if turns.is_empty() {
        println!("(no turns in range)");
    }
    Ok(())
}

fn run_status(json: bool) -> Result<(), String> {
    let store = open_store()?;
    let stats = store.stats().map_err(|e| e.to_string())?;
    let total_turns = store.total_turns().map_err(|e| e.to_string())?;
    let backlog = store.embed_backlog().map_err(|e| e.to_string())?;
    let db_bytes = std::fs::metadata(store.path()).map(|m| m.len()).unwrap_or(0);
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
            .map(format_ms)
            .unwrap_or_else(|| "never".to_string());
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
