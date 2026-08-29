//! The transcript-corpus daemon service: watches every CLI's transcript
//! store, ingests changes incrementally, keeps the embedding model warm,
//! and serves `search` / `ask` over the standard daemon transport.

use std::path::{Path, PathBuf};

use pixel_recall::ask::{ask, format_group};
use pixel_recall::embed::{Embedder, open_default_embedder, run_backfill};
use pixel_recall::ingest::ingest_source;
use pixel_recall::search::{SearchFilters, format_hit, search};
use pixel_recall::segment::SegmentSet;
use pixel_recall::sources::SourceAdapter;
use pixel_recall::store::RecallStore;
use pixel_recall::vector::VectorStore;
use serde_json::{Value, json};

use crate::api::{PROTOCOL_VERSION, Request, Response, ServeError, failure_response};
use crate::daemon::Corpus;
use pixel_proto::Envelope;

pub struct RecallService {
    root: PathBuf,
    store: RecallStore,
    /// Lazily opened on first ask; kept warm for the daemon's lifetime.
    embedder: Option<Box<dyn Embedder>>,
    embedder_unavailable: bool,
}

fn all_adapters() -> Vec<Box<dyn SourceAdapter>> {
    use pixel_recall::sources::*;
    vec![
        Box::new(claude::ClaudeAdapter::new()),
        Box::new(codex::Adapter::new()),
        Box::new(cursor::Adapter::new()),
        Box::new(gemini::Adapter::new()),
        Box::new(opencode::Adapter::new()),
        Box::new(zcode::Adapter::new()),
        Box::new(devin::Adapter::new()),
    ]
}

/// The directories whose changes mean "new transcript content", mapped to
/// the adapter that owns them.
fn watch_roots() -> Vec<(&'static str, PathBuf)> {
    let home = std::env::var("HOME").unwrap_or_default();
    let h = |suffix: &str| PathBuf::from(&home).join(suffix);
    vec![
        ("claude", h(".claude/projects")),
        ("codex", h(".codex/sessions")),
        ("cursor", h(".cursor/projects")),
        ("gemini", h(".gemini/antigravity-cli")),
        ("opencode", h(".local/share/opencode")),
        ("zcode", h(".zcode/cli/db")),
        ("devin", h(".local/share/devin/cli")),
    ]
}

impl RecallService {
    pub fn open() -> Result<Self, ServeError> {
        let root = pixel_recall::ensure_recall_dir()
            .map_err(|e| ServeError::Msg(format!("recall dir: {e}")))?;
        let store = RecallStore::open(&pixel_recall::db_path())
            .map_err(|e| ServeError::Msg(format!("recall.db: {e}")))?;
        Ok(Self {
            root,
            store,
            embedder: None,
            embedder_unavailable: false,
        })
    }

    /// Lazy-load the model once; afterwards `self.embedder` stays warm.
    fn ensure_embedder(&mut self) {
        if self.embedder.is_none() && !self.embedder_unavailable {
            match open_default_embedder(false) {
                Ok(e) => self.embedder = Some(e),
                Err(_) => self.embedder_unavailable = true,
            }
        }
    }

    fn op(&mut self, action: &str, params: Value) -> Result<Value, String> {
        match action {
            "search" => {
                let pattern = params
                    .get("pattern")
                    .and_then(Value::as_str)
                    .ok_or("missing pattern")?
                    .to_string();
                let word = params.get("word").and_then(Value::as_bool).unwrap_or(false);
                let limit = params
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(20)
                    .min(200) as usize;
                let offset = params.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
                let filters: SearchFilters = params
                    .get("filters")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|e| format!("bad filters: {e}"))?
                    .unwrap_or_default();
                let segments = SegmentSet::open(&pixel_recall::segments_dir())?;
                let result = search(
                    &self.store,
                    &segments,
                    &pattern,
                    word,
                    &filters,
                    offset,
                    limit,
                )?;
                let mut text = String::new();
                if result.hits.is_empty() {
                    text.push_str(&format!(
                        "no matches ({} turns considered) — the pattern does not appear in the indexed corpus\n",
                        result.turns_considered
                    ));
                } else {
                    for h in &result.hits {
                        text.push_str(&format_hit(h));
                        text.push('\n');
                    }
                    if result.truncated {
                        text.push_str(&format!(
                            "(showing {} — more matches exist, use --offset {} or narrow the query)\n",
                            result.hits.len(),
                            offset + result.hits.len()
                        ));
                    }
                }
                Ok(json!({
                    "text": text,
                    "json": {
                        "hits": result.hits,
                        "turns_considered": result.turns_considered,
                        "truncated": result.truncated,
                    },
                }))
            }
            "ask" => {
                let query = params
                    .get("query")
                    .and_then(Value::as_str)
                    .ok_or("missing query")?
                    .to_string();
                let k = params
                    .get("k")
                    .and_then(Value::as_u64)
                    .unwrap_or(10)
                    .clamp(1, 50) as usize;
                let lexical_only = params
                    .get("lexical_only")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let filters: SearchFilters = params
                    .get("filters")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|e| format!("bad filters: {e}"))?
                    .unwrap_or_default();
                let segments = SegmentSet::open(&pixel_recall::segments_dir())?;
                let vectors = VectorStore::open(&pixel_recall::vectors_dir())?;
                if !lexical_only {
                    self.ensure_embedder();
                }
                let embedder = if lexical_only {
                    None
                } else {
                    self.embedder.as_deref_mut()
                };
                let result = ask(&self.store, &segments, &vectors, embedder, &query, &filters, k)?;
                let mut text = String::new();
                if let Some(n) = &result.notice {
                    text.push_str(&format!("note: {n}\n"));
                }
                if result.groups.is_empty() {
                    text.push_str("no matching sessions — nothing in the corpus resembles that query\n");
                }
                for g in &result.groups {
                    text.push_str(&format_group(g));
                    text.push('\n');
                }
                Ok(json!({
                    "text": text,
                    "json": {
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
                    },
                }))
            }
            other => Err(format!("unknown recall action '{other}'")),
        }
    }

    /// Incrementally ingest the agents whose stores changed, refresh the
    /// lexical segments, and drain the embed backlog while the model is
    /// warm. Best-effort: watcher-driven maintenance must never kill the
    /// daemon.
    fn refresh_agents(&mut self, agents: &std::collections::BTreeSet<&'static str>) {
        for adapter in all_adapters() {
            if !agents.contains(adapter.agent()) {
                continue;
            }
            match ingest_source(&mut self.store, adapter.as_ref()) {
                Ok(report) => {
                    if report.sessions_written > 0 {
                        eprintln!(
                            "recall daemon: {} +{} sessions, +{} turns",
                            report.agent, report.sessions_written, report.turns_written
                        );
                    }
                }
                Err(e) => eprintln!("recall daemon: ingest {}: {e}", adapter.agent()),
            }
        }
        match SegmentSet::open(&pixel_recall::segments_dir()) {
            Ok(mut segments) => {
                if let Err(e) = segments.index_new(&self.store) {
                    eprintln!("recall daemon: segment index: {e}");
                }
            }
            Err(e) => eprintln!("recall daemon: segments: {e}"),
        }
        // Drain the embed backlog only when it is small: the daemon loop is
        // single-threaded, and a bulk backfill here would block the socket
        // for minutes (that is `gitpixel recall embed`'s job).
        const MAX_INLINE_BACKLOG: i64 = 5_000;
        match self.store.embed_backlog() {
            Ok(backlog) if backlog > 0 && backlog <= MAX_INLINE_BACKLOG => {
                self.ensure_embedder();
                if let Some(embedder) = self.embedder.as_deref_mut() {
                    let mut vectors = match VectorStore::open(&pixel_recall::vectors_dir()) {
                        Ok(v) => v,
                        Err(e) => {
                            eprintln!("recall daemon: vectors: {e}");
                            return;
                        }
                    };
                    if let Err(e) = run_backfill(&self.store, &mut vectors, embedder, |_, _| {}) {
                        eprintln!("recall daemon: embed: {e}");
                    }
                }
            }
            Ok(backlog) if backlog > MAX_INLINE_BACKLOG => {
                eprintln!(
                    "recall daemon: embed backlog {backlog} exceeds inline cap — run `gitpixel recall embed`"
                );
            }
            _ => {}
        }
    }
}

impl Corpus for RecallService {
    fn root(&self) -> &Path {
        &self.root
    }

    fn handle(&mut self, req: Request) -> Response {
        let op_name = req.op_name();
        match req {
            Request::Ping => Envelope::success(op_name, json!({
                "pong": true,
                "root": self.root.display().to_string(),
                "corpus": "recall",
                "protocol_version": PROTOCOL_VERSION,
            })),
            Request::Shutdown => Envelope::success(op_name, json!({"shutting_down": true})),
            Request::Recall { action, params } => match self.op(&action, params) {
                Ok(data) => Envelope::success(op_name, data),
                Err(e) => failure_response(op_name, e),
            },
            _ => failure_response(op_name,
                "this daemon serves the transcript corpus; repository ops go to a repo daemon"
                    .to_string(),
            ),
        }
    }

    fn apply_change(&mut self, abs: &Path, _removed: bool) {
        let mut touched = std::collections::BTreeSet::new();
        for (agent, root) in watch_roots() {
            if abs.starts_with(&root) {
                touched.insert(agent);
            }
        }
        if !touched.is_empty() {
            self.refresh_agents(&touched);
        }
    }

    fn watch_paths(&self) -> Vec<PathBuf> {
        watch_roots()
            .into_iter()
            .map(|(_, p)| p)
            .filter(|p| p.exists())
            .collect()
    }
}
