//! Bulk export of ingested sessions to one file per session (md/jsonl).
//!
//! Exports what the corpus already holds — never re-reads raw agent stores.
//! Honesty contract (T2): sessions whose timestamps are approximate or
//! absent disclose that in the exported file, and a date filter that would
//! silently drop timestamp-less sessions instead reports them in
//! `skipped_unresolvable_ts`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::model::{TsSource, format_ms};
use crate::store::{RecallStore, SessionRow, TurnRow};

/// Cap on the session listing pulled from the store. If the corpus somehow
/// holds more matching sessions than this, the summary says `truncated`.
const MAX_EXPORT_SESSIONS: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Md,
    Jsonl,
}

impl ExportFormat {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "md" => Ok(ExportFormat::Md),
            "jsonl" => Ok(ExportFormat::Jsonl),
            other => Err(format!("--format must be md or jsonl (got '{other}')")),
        }
    }

    fn ext(self) -> &'static str {
        match self {
            ExportFormat::Md => "md",
            ExportFormat::Jsonl => "jsonl",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ExportFilters {
    pub agent: Option<String>,
    /// Already-resolved corpus session id (the CLI resolves refs/prefixes).
    pub session_id: Option<i64>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
}

#[derive(Debug)]
pub struct ExportSummary {
    pub sessions_exported: usize,
    pub turns: usize,
    pub out_dir: PathBuf,
    /// Sessions a date filter could not evaluate (no usable timestamps) —
    /// reported instead of silently dropped. `agent:source_session_id`.
    pub skipped_unresolvable_ts: Vec<String>,
    /// True only if the MAX_EXPORT_SESSIONS listing cap was hit.
    pub truncated: bool,
}

/// Export every matching ingested session into `out_dir`, one file per
/// session, named `<agent>-<session-slug>-<date>.<ext>`.
pub fn export(
    store: &RecallStore,
    filters: &ExportFilters,
    out_dir: &Path,
    format: ExportFormat,
) -> Result<ExportSummary, String> {
    let mut candidates: Vec<SessionRow> = match filters.session_id {
        Some(id) => store
            .session_by_id(id)
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|s| {
                filters
                    .agent
                    .as_deref()
                    .is_none_or(|a| a == s.agent)
            })
            .collect(),
        // Date filters are applied in Rust below (not in SQL) so sessions
        // lacking timestamps can be REPORTED rather than silently dropped.
        None => store
            .sessions(
                filters.agent.as_deref(),
                None,
                None,
                None,
                true, // bulk export includes subagent sessions
                MAX_EXPORT_SESSIONS,
            )
            .map_err(|e| e.to_string())?,
    };
    let truncated = candidates.len() >= MAX_EXPORT_SESSIONS;
    // Oldest first: stable, reproducible export order.
    candidates.sort_by_key(|s| (s.ts_last, s.id));

    let has_date_filter = filters.since_ms.is_some() || filters.until_ms.is_some();
    let mut skipped_unresolvable_ts = Vec::new();
    let mut selected = Vec::new();
    for s in candidates {
        if has_date_filter {
            let (Some(first), Some(last)) = (s.ts_first, s.ts_last) else {
                skipped_unresolvable_ts.push(format!("{}:{}", s.agent, s.source_session_id));
                continue;
            };
            // Same window semantics as the store's SQL path: a session
            // overlaps [since, until] when ts_last >= since && ts_first <= until.
            if filters.since_ms.is_some_and(|since| last < since)
                || filters.until_ms.is_some_and(|until| first > until)
            {
                continue;
            }
        }
        selected.push(s);
    }

    std::fs::create_dir_all(out_dir)
        .map_err(|e| format!("create {}: {e}", out_dir.display()))?;

    let mut used_names: HashSet<String> = HashSet::new();
    let mut sessions_exported = 0usize;
    let mut turns_total = 0usize;
    for session in &selected {
        let turns = store
            .turns_for_session(session.id, None)
            .map_err(|e| e.to_string())?;
        let name = unique_file_name(session, format, &mut used_names);
        let content = match format {
            ExportFormat::Md => render_md(session, &turns),
            ExportFormat::Jsonl => render_jsonl(session, &turns)?,
        };
        let path = out_dir.join(&name);
        std::fs::write(&path, content).map_err(|e| format!("write {}: {e}", path.display()))?;
        sessions_exported += 1;
        turns_total += turns.len();
    }

    Ok(ExportSummary {
        sessions_exported,
        turns: turns_total,
        out_dir: out_dir.to_path_buf(),
        skipped_unresolvable_ts,
        truncated,
    })
}

/// `<agent>-<session-slug>-<date>.<ext>`, deduped with the corpus id on
/// collision so two same-titled sessions never overwrite each other.
fn unique_file_name(
    session: &SessionRow,
    format: ExportFormat,
    used: &mut HashSet<String>,
) -> String {
    let slug_src = session
        .title
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or(&session.source_session_id);
    let slug = slugify(slug_src);
    let date = session
        .ts_last
        .map(date_stamp)
        .unwrap_or_else(|| "nodate".to_string());
    let base = format!("{}-{}-{}", session.agent, slug, date);
    let mut name = format!("{base}.{}", format.ext());
    if !used.insert(name.clone()) {
        name = format!("{base}-{}.{}", session.id, format.ext());
        used.insert(name.clone());
    }
    name
}

fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true; // suppress leading dash
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
        if out.len() >= 48 {
            break;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "session".to_string()
    } else {
        trimmed
    }
}

/// `YYYYMMDD` from unix ms (UTC), reusing the model's formatter.
fn date_stamp(ms: i64) -> String {
    let full = format_ms(ms); // "YYYY-MM-DD HH:MM" (or "?")
    if full.len() >= 10 {
        full[..10].replace('-', "")
    } else {
        "nodate".to_string()
    }
}

/// T2 disclosure line for timestamp provenance.
fn ts_source_note(ts_source: TsSource) -> String {
    let base = format!("ts_source: {}", ts_source.as_str());
    match ts_source {
        TsSource::Mtime => format!(
            "{base} (approximate — derived from file mtime, not per-turn records)"
        ),
        TsSource::Absent => format!("{base} (no timestamps recorded in the source)"),
        TsSource::Iso | TsSource::UnixMs => base,
    }
}

fn render_md(session: &SessionRow, turns: &[TurnRow]) -> String {
    let title = session.title.as_deref().unwrap_or("(untitled)");
    let cwd = session.cwd.as_deref().unwrap_or("-");
    let mut out = String::new();
    out.push_str(&format!("# {title} — {} @ {cwd}\n\n", session.agent));
    out.push_str(&format!(
        "session: {}:{} (#{})\n",
        session.agent, session.source_session_id, session.id
    ));
    out.push_str(&format!("source: {}\n", session.source_path));
    out.push_str(&format!("{}\n\n", ts_source_note(session.ts_source)));
    for t in turns {
        let ts = t.ts.map(format_ms).unwrap_or_else(|| "?".to_string());
        out.push_str(&format!("## [{}] {}\n\n", t.role, ts));
        let fence = fence_for(&t.text);
        out.push_str(&format!("{fence}\n{}\n{fence}\n\n", t.text));
    }
    out
}

/// A fence one backtick longer than the longest run inside the text, never
/// shorter than three — so embedded ``` blocks cannot break the framing.
fn fence_for(text: &str) -> String {
    let mut longest = 0usize;
    let mut run = 0usize;
    for c in text.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat((longest + 1).max(3))
}

fn render_jsonl(session: &SessionRow, turns: &[TurnRow]) -> Result<String, String> {
    let mut out = String::new();
    for t in turns {
        let line = serde_json::json!({
            "agent": session.agent,
            "session": session.source_session_id,
            "idx": t.seq,
            "role": t.role,
            "ts": t.ts,
            "ts_source": session.ts_source.as_str(),
            "text": t.text,
        });
        out.push_str(&serde_json::to_string(&line).map_err(|e| e.to_string())?);
        out.push('\n');
    }
    Ok(out)
}
