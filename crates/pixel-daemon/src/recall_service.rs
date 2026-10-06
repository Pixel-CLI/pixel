// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The transcript-corpus daemon service: watches every CLI's transcript
//! store, ingests changes incrementally, keeps the embedding model warm,
//! and serves `search` / `ask` over the standard daemon transport.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pixel_recall::ask::{ask, format_group};
use pixel_recall::embed::{Embedder, open_default_embedder, reset_if_stale, run_backfill_limited};
use pixel_recall::ingest::{IngestReport, ingest_recent, ingest_source};
use pixel_recall::search::{SearchFilters, format_hit, search};
use pixel_recall::segment::SegmentSet;
use pixel_recall::sources::SourceAdapter;
use pixel_recall::store::RecallStore;
use pixel_recall::vector::VectorStore;
use serde_json::{Value, json};

/// The embed backlog the daemon drains inline, and the slice it embeds per
/// pass. The daemon loop is single-threaded, and a bulk backfill would block
/// the socket for minutes.
const MAX_INLINE_BACKLOG: i64 = 5_000;

/// True when the daemon should embed now: any backlog of a store already
/// built with a model (a re-embed after a revision change, or a large
/// catch-up), a slice per pass; for a store never built, only a backlog it
/// can drain in one pass — a whole corpus is `pixel recall embed`'s job.
fn drains_inline(backlog: i64, store_built: bool) -> bool {
    backlog > 0 && (store_built || backlog <= MAX_INLINE_BACKLOG)
}
/// How often the daemon re-stats the recently modified transcripts
/// independently of the watcher. FSEvents on macOS holds back the modify
/// event of a file while its writer keeps it open, and an agent streams
/// its transcript exactly that way: the session being written right now
/// is the one the watcher does not report. The sweep is one directory
/// walk per source; unchanged files are skipped on size and mtime.
const SWEEP_INTERVAL: Duration = Duration::from_secs(5);
/// Only transcripts modified this recently are re-stated by the sweep;
/// anything older is either already ingested or will arrive through the
/// watcher when its writer closes it.
const SWEEP_WINDOW_MS: i64 = 21_600_000; // 6 h
/// How often one agent's "skipped records" line may repeat. A transcript
/// that stays unreadable must be visible in the log, not printed on every
/// five-second sweep.
const SKIPPED_LOG_INTERVAL: Duration = Duration::from_secs(60);

use crate::api::{PROTOCOL_VERSION, Request, Response, ServeError, failure_response};
use crate::daemon::Corpus;
use pixel_proto::Envelope;

/// One transcript store the daemon serves: the directory whose changes
/// mean "new transcript content" and the adapter that parses it.
pub struct RecallSource {
    pub root: PathBuf,
    pub adapter: Box<dyn SourceAdapter>,
}

/// The ingest log's rate limit: when each agent last got a skipped-records
/// line.
#[derive(Default)]
struct IngestLog {
    skipped_at: BTreeMap<String, Instant>,
}

impl IngestLog {
    /// The line for one ingest pass, or `None` when the pass is quiet. A
    /// pass that wrote sessions always speaks; a pass that only skipped
    /// unreadable records speaks at most once per `SKIPPED_LOG_INTERVAL`,
    /// because staying silent is how a corrupt transcript went unnoticed.
    fn line(&mut self, report: &IngestReport, now: Instant) -> Option<String> {
        if report.sessions_written > 0 {
            return Some(format!(
                "recall daemon: {} +{} sessions, +{} turns, {} skipped records",
                report.agent, report.sessions_written, report.turns_written, report.skipped_records
            ));
        }
        if report.skipped_records == 0 {
            return None;
        }
        let due = self
            .skipped_at
            .get(&report.agent)
            .is_none_or(|last| now.saturating_duration_since(*last) >= SKIPPED_LOG_INTERVAL);
        if !due {
            return None;
        }
        self.skipped_at.insert(report.agent.clone(), now);
        Some(format!(
            "recall daemon: {} wrote no session, skipped {} unreadable record(s) — the transcript is truncated or corrupt",
            report.agent, report.skipped_records
        ))
    }
}

pub struct RecallService {
    root: PathBuf,
    store: RecallStore,
    segments_dir: PathBuf,
    vectors_dir: PathBuf,
    sources: Vec<RecallSource>,
    /// Rate-limits the ingest log's skipped-records line, per agent.
    ingest_log: IngestLog,
    /// Lazily opened on first ask; kept warm for the daemon's lifetime.
    embedder: Option<Box<dyn Embedder>>,
    embedder_unavailable: bool,
}

/// Every transcript store this machine's agents write, under `HOME`.
fn machine_sources() -> Vec<RecallSource> {
    use pixel_recall::sources::*;
    let home = std::env::var("HOME").unwrap_or_default();
    let h = |suffix: &str| PathBuf::from(&home).join(suffix);
    vec![
        RecallSource {
            root: h(".claude/projects"),
            adapter: Box::new(claude::ClaudeAdapter::new()),
        },
        RecallSource {
            root: h(".codex/sessions"),
            adapter: Box::new(codex::Adapter::new()),
        },
        RecallSource {
            root: h(".cursor/projects"),
            adapter: Box::new(cursor::Adapter::new()),
        },
        RecallSource {
            root: h(".gemini/antigravity-cli"),
            adapter: Box::new(gemini::Adapter::new()),
        },
        RecallSource {
            root: h(".local/share/opencode"),
            adapter: Box::new(opencode::Adapter::new()),
        },
        RecallSource {
            root: h(".zcode/cli/db"),
            adapter: Box::new(zcode::Adapter::new()),
        },
        RecallSource {
            root: h(".local/share/devin/cli"),
            adapter: Box::new(devin::Adapter::new()),
        },
        RecallSource {
            root: pi::machine_sessions_dir(),
            adapter: Box::new(pi::Adapter::new()),
        },
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
            segments_dir: pixel_recall::segments_dir(),
            vectors_dir: pixel_recall::vectors_dir(),
            sources: machine_sources(),
            ingest_log: IngestLog::default(),
            embedder: None,
            embedder_unavailable: false,
        })
    }

    /// A service over explicit directories and sources, no `HOME` and no
    /// embedding model: the seam the tests drive `sweep` through.
    #[cfg(test)]
    fn with_sources(
        root: PathBuf,
        store: RecallStore,
        segments_dir: PathBuf,
        vectors_dir: PathBuf,
        sources: Vec<RecallSource>,
    ) -> Self {
        Self {
            root,
            store,
            segments_dir,
            vectors_dir,
            sources,
            ingest_log: IngestLog::default(),
            embedder: None,
            embedder_unavailable: true,
        }
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
                let segments = SegmentSet::open(&self.segments_dir)?;
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
                let segments = SegmentSet::open(&self.segments_dir)?;
                let vectors = VectorStore::open(&self.vectors_dir)?;
                if !lexical_only {
                    self.ensure_embedder();
                }
                let embedder = if lexical_only {
                    None
                } else {
                    self.embedder.as_deref_mut()
                };
                let result = ask(
                    &self.store,
                    &segments,
                    &vectors,
                    embedder,
                    &query,
                    &filters,
                    k,
                    true,
                )?;
                let mut text = String::new();
                if let Some(n) = &result.notice {
                    text.push_str(&format!("note: {n}\n"));
                }
                if result.groups.is_empty() {
                    text.push_str(
                        "no matching sessions — nothing in the corpus resembles that query\n",
                    );
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
    // Watcher-driven glue over this machine's real agent stores (HOME);
    // `ingest_source`, the adapters, segments and backfill are unit-tested
    // in pixel-recall.
    #[cfg_attr(test, mutants::skip)]
    fn refresh_agents(&mut self, agents: &std::collections::BTreeSet<&'static str>) {
        for source in &self.sources {
            if !agents.contains(source.adapter.agent()) {
                continue;
            }
            let report = ingest_source(&mut self.store, source.adapter.as_ref());
            Self::log_ingest(&mut self.ingest_log, source.adapter.agent(), report);
        }
        self.after_ingest();
    }

    /// The periodic sweep: re-stat the transcripts modified within
    /// `SWEEP_WINDOW_MS` for every source present on this machine and
    /// ingest the ones that grew, whether or not the watcher reported them.
    fn sweep_recent(&mut self) {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        for source in &self.sources {
            if !source.root.exists() {
                continue;
            }
            let report = ingest_recent(
                &mut self.store,
                source.adapter.as_ref(),
                now_ms,
                SWEEP_WINDOW_MS,
            );
            Self::log_ingest(&mut self.ingest_log, source.adapter.agent(), report);
        }
        self.after_ingest();
    }

    #[cfg_attr(test, mutants::skip)] // stderr diagnostics only
    fn log_ingest(
        ingest_log: &mut IngestLog,
        agent: &str,
        report: Result<IngestReport, pixel_recall::sources::IngestError>,
    ) {
        match report {
            Ok(report) => {
                if let Some(line) = ingest_log.line(&report, Instant::now()) {
                    eprintln!("{line}");
                }
            }
            Err(e) => eprintln!("recall daemon: ingest {agent}: {e}"),
        }
    }

    /// Refresh the lexical segments and drain a small embed backlog after
    /// any ingest pass.
    // Glue over the real segments and vectors directories; both are
    // unit-tested in pixel-recall.
    #[cfg_attr(test, mutants::skip)]
    fn after_ingest(&mut self) {
        match SegmentSet::open(&self.segments_dir) {
            Ok(mut segments) => {
                if let Err(e) = segments.index_new(&self.store) {
                    eprintln!("recall daemon: segment index: {e}");
                }
            }
            Err(e) => eprintln!("recall daemon: segments: {e}"),
        }
        // A store written at another revision of its model empties itself
        // here, and the backlog below rebuilds it a slice per pass: nobody
        // has to run `pixel recall embed --rebuild`.
        let mut vectors = if self.vectors_dir.exists() {
            match VectorStore::open(&self.vectors_dir) {
                Ok(v) => Some(v),
                Err(e) => {
                    eprintln!("recall daemon: vectors: {e}");
                    return;
                }
            }
        } else {
            None
        };
        if let Some(v) = vectors.as_mut() {
            match reset_if_stale(&self.store, v) {
                Ok(true) => eprintln!(
                    "recall daemon: vectors were embedded by an older model revision — re-embedding the corpus in the background"
                ),
                Ok(false) => {}
                Err(e) => eprintln!("recall daemon: vector reset: {e}"),
            }
        }
        let store_built = vectors
            .as_ref()
            .is_some_and(|v| !v.meta.model_id.is_empty());
        match self.store.embed_backlog() {
            Ok(backlog) if drains_inline(backlog, store_built) => {
                self.ensure_embedder();
                if let Some(embedder) = self.embedder.as_deref_mut() {
                    let mut vectors = match vectors {
                        Some(v) => v,
                        None => match VectorStore::open(&self.vectors_dir) {
                            Ok(v) => v,
                            Err(e) => {
                                eprintln!("recall daemon: vectors: {e}");
                                return;
                            }
                        },
                    };
                    if let Err(e) = run_backfill_limited(
                        &self.store,
                        &mut vectors,
                        embedder,
                        MAX_INLINE_BACKLOG as usize,
                        |_, _| {},
                    ) {
                        eprintln!("recall daemon: embed: {e}");
                    }
                }
            }
            Ok(backlog) if backlog > MAX_INLINE_BACKLOG => {
                eprintln!(
                    "recall daemon: embed backlog {backlog} exceeds inline cap — run `pixel recall embed`"
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
            Request::Ping => Envelope::success(
                op_name,
                json!({
                    "pong": true,
                    "root": self.root.display().to_string(),
                    "corpus": "recall",
                    "protocol_version": PROTOCOL_VERSION,
                }),
            ),
            Request::Shutdown => Envelope::success(op_name, json!({"shutting_down": true})),
            Request::Recall { action, params } => match self.op(&action, params) {
                Ok(data) => Envelope::success(op_name, data),
                Err(e) => failure_response(op_name, e),
            },
            _ => failure_response(
                op_name,
                "this daemon serves the transcript corpus; repository ops go to a repo daemon"
                    .to_string(),
            ),
        }
    }

    fn apply_change(&mut self, abs: &Path, _removed: bool) {
        let mut touched = std::collections::BTreeSet::new();
        for source in &self.sources {
            if abs.starts_with(&source.root) {
                touched.insert(source.adapter.agent());
            }
        }
        if !touched.is_empty() {
            self.refresh_agents(&touched);
        }
    }

    fn watch_paths(&self) -> Vec<PathBuf> {
        self.sources
            .iter()
            .map(|s| s.root.clone())
            .filter(|p| p.exists())
            .collect()
    }

    fn sweep_interval(&self) -> Option<Duration> {
        Some(SWEEP_INTERVAL)
    }

    fn sweep(&mut self) {
        self.sweep_recent();
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::SystemTime;

    use pixel_recall::sources::claude::ClaudeAdapter;

    use super::*;

    /// A built store drains any backlog a slice per pass (a re-embed after a
    /// model update rebuilds itself); a store never built drains only what
    /// one pass can take, leaving a whole corpus to `pixel recall embed`.
    #[test]
    fn drains_inline_takes_any_backlog_of_a_built_store_and_small_ones_otherwise() {
        assert!(!drains_inline(0, true), "nothing to embed");
        assert!(!drains_inline(0, false));
        assert!(drains_inline(1, false));
        assert!(drains_inline(MAX_INLINE_BACKLOG, false), "at the cap");
        assert!(!drains_inline(MAX_INLINE_BACKLOG + 1, false));
        assert!(drains_inline(MAX_INLINE_BACKLOG + 1, true));
        assert!(drains_inline(248_000, true));
    }

    /// A recall service over a scratch corpus and one Claude store holding a
    /// single-turn session in `projects/-work-pixel/<id>.jsonl`.
    struct Fixture {
        scratch: PathBuf,
        transcript: PathBuf,
        service: RecallService,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let scratch = std::env::temp_dir().join(format!(
                "pixel-recall-sweep-{tag}-{}-{}",
                std::process::id(),
                line!()
            ));
            let _ = fs::remove_dir_all(&scratch);
            let projects = scratch.join("projects");
            let slug = projects.join("-work-pixel");
            fs::create_dir_all(&slug).unwrap();
            let transcript = slug.join("0123abcd-0000-4000-8000-000000000001.jsonl");
            let record = json!({
                "type": "user",
                "cwd": "/work/pixel",
                "timestamp": "2025-10-09T08:53:20.000Z",
                "message": {"content": [{"type": "text", "text": "the streamed needle"}]},
            });
            fs::write(&transcript, format!("{record}\n")).unwrap();
            let store = RecallStore::open(&scratch.join("recall.db")).unwrap();
            let service = RecallService::with_sources(
                scratch.clone(),
                store,
                scratch.join("segments"),
                scratch.join("vectors"),
                vec![RecallSource {
                    root: projects,
                    adapter: Box::new(ClaudeAdapter::with_root(
                        slug.parent().unwrap().to_path_buf(),
                    )),
                }],
            );
            Self {
                scratch,
                transcript,
                service,
            }
        }

        fn age_transcript(&self, age: Duration) {
            let f = fs::OpenOptions::new()
                .write(true)
                .open(&self.transcript)
                .unwrap();
            f.set_modified(SystemTime::now() - age).unwrap();
        }

        /// Replace the transcript's bytes (a writer that crashed mid-flush).
        fn write_transcript(&self, text: &str) {
            fs::write(&self.transcript, text).unwrap();
        }

        fn turns(&self) -> i64 {
            self.service.store.total_turns().unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.scratch);
        }
    }

    /// The reason the sweep exists: a transcript that grew without a
    /// watcher event is in the corpus after one `sweep`.
    #[test]
    fn sweep_ingests_a_transcript_modified_within_the_window() {
        let mut fx = Fixture::new("fresh");
        assert_eq!(fx.turns(), 0);
        fx.service.sweep();
        assert_eq!(
            fx.turns(),
            1,
            "the streamed turn must be ingested by the sweep"
        );
        // A second sweep re-stats the file and reparses nothing.
        fx.service.sweep();
        assert_eq!(fx.turns(), 1);
    }

    /// The window bounds the sweep's work: a transcript older than it is
    /// left to the watcher, and picked up as soon as it is touched again.
    #[test]
    fn sweep_leaves_a_transcript_older_than_the_window_to_the_watcher() {
        let mut fx = Fixture::new("stale");
        fx.age_transcript(
            Duration::from_millis(SWEEP_WINDOW_MS as u64) + Duration::from_secs(3600),
        );
        fx.service.sweep();
        assert_eq!(
            fx.turns(),
            0,
            "a transcript outside the window must not be re-stated"
        );
        fx.age_transcript(Duration::ZERO);
        fx.service.sweep();
        assert_eq!(fx.turns(), 1);
    }

    /// A source whose root is absent on this machine is skipped, not an
    /// error that stops the other sources.
    #[test]
    fn sweep_skips_sources_whose_root_is_missing() {
        let mut fx = Fixture::new("missing");
        fx.service.sources.insert(
            0,
            RecallSource {
                root: fx.scratch.join("no-such-store"),
                adapter: Box::new(ClaudeAdapter::with_root(fx.scratch.join("no-such-store"))),
            },
        );
        fx.service.sweep();
        assert_eq!(fx.turns(), 1);
    }

    /// The machine's sources are the eight stores under `HOME`, one adapter
    /// each; an empty list would make the daemon watch and sweep nothing.
    #[test]
    fn machine_sources_cover_every_agent_store_under_home() {
        let sources = machine_sources();
        let mut agents: Vec<&str> = sources.iter().map(|s| s.adapter.agent()).collect();
        agents.sort_unstable();
        assert_eq!(
            agents,
            vec![
                "claude", "codex", "cursor", "devin", "gemini", "opencode", "pi", "zcode"
            ]
        );
        let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
        for source in &sources {
            assert!(
                source.root.starts_with(&home),
                "{} is not under HOME",
                source.root.display()
            );
        }
        let claude = sources
            .iter()
            .find(|s| s.adapter.agent() == "claude")
            .unwrap();
        assert!(claude.root.ends_with(".claude/projects"));
    }

    /// The watcher observes exactly the source roots present on disk: a
    /// missing store is neither watched (notify would refuse it) nor
    /// replaced by an empty path.
    #[test]
    fn watch_paths_are_the_existing_source_roots() {
        let mut fx = Fixture::new("watch");
        let projects = fx.scratch.join("projects");
        fx.service.sources.push(RecallSource {
            root: fx.scratch.join("no-such-store"),
            adapter: Box::new(ClaudeAdapter::with_root(fx.scratch.join("no-such-store"))),
        });
        assert_eq!(fx.service.watch_paths(), vec![projects]);
    }

    /// A watcher event under a source root ingests that source; an event
    /// elsewhere on the machine touches nothing.
    #[test]
    fn apply_change_ingests_only_the_source_owning_the_path() {
        let mut fx = Fixture::new("apply");
        let outside = fx.scratch.join("elsewhere").join("x.jsonl");
        fx.service.apply_change(&outside, false);
        assert_eq!(fx.turns(), 0, "a path outside every root must not ingest");
        let transcript = fx.transcript.clone();
        fx.service.apply_change(&transcript, false);
        assert_eq!(
            fx.turns(),
            1,
            "a path under the root must ingest its source"
        );
    }

    /// The daemon's `search` action answers from the corpus the sweep
    /// filled, and an unknown action is an error, not an empty success.
    #[test]
    fn op_search_finds_the_swept_turn_and_rejects_unknown_actions() {
        let mut fx = Fixture::new("op");
        fx.service.sweep();
        let out = fx
            .service
            .op("search", json!({"pattern": "streamed needle"}))
            .unwrap();
        assert_eq!(out["json"]["hits"].as_array().map(Vec::len), Some(1));
        assert_eq!(out["json"]["truncated"], json!(false));
        assert!(
            out["text"].as_str().unwrap().contains("streamed needle"),
            "text view must carry the hit: {out}"
        );
        let err = fx.service.op("bogus", json!({})).unwrap_err();
        assert!(err.contains("unknown recall action"), "{err}");
    }

    /// The recall corpus opts into the sweep at the documented cadence; the
    /// repo corpus does not.
    #[test]
    fn recall_service_sweeps_every_five_seconds() {
        let fx = Fixture::new("interval");
        assert_eq!(fx.service.sweep_interval(), Some(Duration::from_secs(5)));
    }

    /// A pass that only skipped unreadable records still produces a line:
    /// silence is how a transcript whose records never parse stayed
    /// invisible. A pass with nothing to report stays quiet.
    #[test]
    fn a_pass_that_only_skipped_records_still_produces_a_line() {
        let mut log = IngestLog::default();
        let now = Instant::now();
        let report = |sessions: usize, skipped: usize| IngestReport {
            agent: "claude".to_string(),
            sessions_written: sessions,
            turns_written: sessions,
            skipped_records: skipped,
            ..Default::default()
        };

        assert_eq!(
            log.line(&report(0, 1), now).as_deref(),
            Some(
                "recall daemon: claude wrote no session, skipped 1 unreadable record(s) — the transcript is truncated or corrupt"
            ),
            "skipped records must not be reported as a clean pass"
        );
        assert!(
            log.line(&report(0, 0), now).is_none(),
            "an untouched pass has nothing to say"
        );
        assert_eq!(
            log.line(&report(1, 0), now).as_deref(),
            Some("recall daemon: claude +1 sessions, +1 turns, 0 skipped records"),
            "a written session still logs the pass"
        );
    }

    /// The skipped-records line repeats at most once per
    /// `SKIPPED_LOG_INTERVAL` and per agent: a file that stays unreadable
    /// is reported without a line on every five-second sweep, and a held
    /// back pass does not postpone the next one.
    #[test]
    fn the_skipped_line_repeats_only_after_the_interval() {
        let mut log = IngestLog::default();
        let report = |agent: &str| IngestReport {
            agent: agent.to_string(),
            skipped_records: 2,
            ..Default::default()
        };
        let t0 = Instant::now();
        let interval = SKIPPED_LOG_INTERVAL;

        assert!(log.line(&report("claude"), t0).is_some());
        assert!(
            log.line(&report("claude"), t0 + interval - Duration::from_millis(1))
                .is_none(),
            "inside the interval the line is held back"
        );
        let after = t0 + interval + Duration::from_secs(1);
        assert!(
            log.line(&report("claude"), after).is_some(),
            "past the interval the line is due again"
        );
        assert!(
            log.line(&report("claude"), after + interval).is_some(),
            "exactly at the interval the line is due"
        );
        assert!(
            log.line(&report("claude"), after + 3 * interval).is_some(),
            "long past the interval the line is still due"
        );
        assert!(
            log.line(&report("codex"), t0).is_some(),
            "the limit is per agent, not global"
        );
    }

    /// A transcript whose only record is truncated reaches the log: the
    /// sweep parses it, the adapter counts the unreadable record, and the
    /// daemon records a line. The old guard (`sessions_written > 0`) kept
    /// that file invisible for every sweep.
    #[test]
    fn sweep_of_a_transcript_with_a_malformed_line_logs_a_skipped_line() {
        let mut fx = Fixture::new("corrupt");
        fx.write_transcript("{\"type\":\"user\",\"message\":{\"conte\n");

        fx.service.sweep();

        assert_eq!(fx.turns(), 0, "no readable record in the transcript");
        assert!(
            fx.service.ingest_log.skipped_at.contains_key("claude"),
            "only a produced line records the agent, so a skipped record must have one"
        );
    }

    /// `Shutdown` is acknowledged and `Recall` reaches the corpus ops; only
    /// repository ops get the "wrong daemon" refusal.
    #[test]
    fn handle_should_acknowledge_shutdown_and_route_recall_to_the_corpus() {
        let mut fx = Fixture::new("handle");
        let shutdown = fx.service.handle(Request::Shutdown);
        assert!(shutdown.ok);
        assert_eq!(shutdown.result, Some(json!({"shutting_down": true})));

        let recall = fx.service.handle(Request::Recall {
            action: "nope".into(),
            params: Value::Null,
        });
        assert!(!recall.ok);
        let message = recall.error.map(|e| e.message).unwrap_or_default();
        assert!(
            message.contains("unknown recall action 'nope'"),
            "{message}"
        );
    }
}
