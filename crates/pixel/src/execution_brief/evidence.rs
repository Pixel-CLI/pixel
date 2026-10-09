// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The live evidence source of the prompt-start brief.
//!
//! A warm daemon answers when one serves the repository; otherwise the same
//! facts come from read-only readers over the published index and graph.
//! Neither route builds an index or a graph, refreshes one, or starts a
//! daemon: a source that is not current fails its operation and the brief
//! reports it as unresolved. Every daemon request carries what is left of the
//! brief's shared window as its socket timeout.

use std::collections::HashSet;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use pixel_daemon::Request;
use pixel_daemon::relevance::{keyword_df, relevance_on, row_weight};
use pixel_graph::GraphStore;
use pixel_graph::build::{
    EXTRACTOR_VERSION, EXTRACTOR_VERSION_KEY, FRESHNESS_KEY, freshness_signature_trusting_stat,
};
use pixel_index::delta::{DeltaState, delta_shard_path};
use pixel_index::indexset::IndexSet;
use pixel_index::shard::Shard;
use pixel_index::{GramExtractor, TrigramExtractor, gitsync};
use pixel_proto::{MeaningResult, Relevance};
use serde_json::Value;

use super::chain::{
    CONCEPT_ROWS, CallerHit, Evidence, Flow, Found, HistoryHit, MAX_TARGETS, MeaningHit,
    RelevanceAnswer, RichFound, RichHit, SEARCH_ROWS, SYMBOL_ROWS, StatusProbe, SymbolHit,
};
use super::relevance::{CoFileStat, KeywordStat, RelevanceInput};

/// The path search's bound: the daemon's `evaluate` default and its
/// `trace` depth, so the local route searches the same radius the
/// daemon's does.
const FLOW_DEPTH: u32 = 8;

/// Targets the relevance probe asks `targets_facts` for: as many files as a
/// brief lists. The probe reads `facts.relevance`; the targets ride along.
const RELEVANCE_TARGETS: usize = 8;

/// Where the facts come from, decided once per brief.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    /// A current daemon is serving this repository.
    Daemon,
    /// Read-only readers over the published index and graph.
    Local,
}

/// The evidence source of one brief.
struct Live {
    root: PathBuf,
    route: Route,
    /// The graph, opened read-only and checked for currency on first use;
    /// the failure is kept so the next operation does not repeat the check.
    graph: Mutex<Option<Result<GraphStore, String>>>,
}

/// The live source for `root`: the daemon when its socket answers a ping on
/// this build's protocol within `deadline`, the local readers otherwise.
pub(super) fn open(root: &Path, deadline: Instant) -> Box<dyn Evidence> {
    let route = if daemon_serves(root, deadline) {
        Route::Daemon
    } else {
        Route::Local
    };
    Box::new(Live::new(root, route))
}

impl Live {
    fn new(root: &Path, route: Route) -> Self {
        Self {
            root: root.to_path_buf(),
            route,
            graph: Mutex::new(None),
        }
    }

    /// One request to the daemon on its own connection.
    fn ask(&self, request: &Request, deadline: Instant) -> Result<Value, String> {
        let mut stream = connect(&self.root, deadline)
            .ok_or_else(|| "no daemon answered within the window".to_string())?;
        let response = crate::roundtrip(&mut stream, request)
            .ok_or_else(|| "the daemon did not answer within the window".to_string())?;
        crate::unwrap_response(response)
    }

    /// Run `read` on the graph once it is proven current.
    fn with_graph<T>(
        &self,
        read: impl FnOnce(&GraphStore) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut slot = self.graph.lock().unwrap_or_else(PoisonError::into_inner);
        match slot.get_or_insert_with(|| open_current_graph(&self.root)) {
            Ok(store) => read(store),
            Err(reason) => Err(reason.clone()),
        }
    }

    fn local_rows(&self, anchor: &str) -> Result<RichFound, String> {
        if !index_current(&self.root) {
            return Err("the text index does not cover HEAD".into());
        }
        let set = IndexSet::open_or_build(&self.root, Box::new(TrigramExtractor))
            .map_err(|error| error.to_string())?;
        let (rows, stats) = set
            .search_page_filtered(&regex::escape(anchor), 0, Some(SEARCH_ROWS), None, None)
            .map_err(|error| error.to_string())?;
        Ok(RichFound {
            hits: rows
                .into_iter()
                .filter(|row| !pixel_index::index::credential_path(Path::new(&row.path)))
                .map(|row| RichHit {
                    path: row.path,
                    line: row.line_number,
                    text: (!row.line.is_empty()).then_some(row.line),
                })
                .collect(),
            capped: stats.truncated,
        })
    }

    /// The concept index on the already-open graph store: same cascade as
    /// the daemon's `find-code`, defaults only — never an `ensure_graph`
    /// or a signal build.
    fn local_concept(&self, phrase: &str) -> Result<RichFound, String> {
        self.with_graph(|store| {
            let outcome = pixel_graph::concept_resolve::resolve(
                store,
                phrase,
                &pixel_graph::concept_resolve::ResolveOptions {
                    limit: CONCEPT_ROWS,
                    ..Default::default()
                },
            )
            .map_err(|error| error.to_string())?;
            Ok(RichFound {
                hits: outcome
                    .matches
                    .iter()
                    .map(|m| RichHit {
                        path: m.path.clone(),
                        line: u64::from(m.start_line),
                        text: concept_text(m),
                    })
                    .collect(),
                capped: outcome.scan_capped,
            })
        })
    }

    /// The picked definition's body from the file itself, bounded to the
    /// token budget's ~4 chars each — the local `pack-context`.
    fn local_context(&self, hit: &SymbolHit, budget_tokens: usize) -> Result<String, String> {
        let text = std::fs::read_to_string(self.root.join(&hit.path)).map_err(|e| e.to_string())?;
        let max_chars = budget_tokens.saturating_mul(4);
        let start = (hit.start_line as usize).saturating_sub(1);
        let rows = (hit.end_line.max(hit.start_line) as usize).saturating_sub(start);
        let mut body = String::new();
        for line in text.lines().skip(start).take(rows.max(1)) {
            if body.len() + line.len() + 1 > max_chars {
                break;
            }
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(line);
        }
        if body.is_empty() {
            return Err("empty definition".to_string());
        }
        Ok(body)
    }

    /// `impact d1` on the local graph kept to callers under a test path —
    /// the same rows `uses`' `caller_test_files` would keep.
    fn local_test_files(&self, uid: &str) -> Result<Vec<String>, String> {
        self.with_graph(|store| {
            let data = pixel_daemon::api::impact_on_graph(store, uid, "upstream", Some(1))?;
            if data.get("candidates").is_some() {
                return Err("the name is ambiguous, a uid is needed".into());
            }
            let mut files: Vec<String> = data
                .get("d1_will_break")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|caller| caller.get("path").and_then(Value::as_str))
                .filter(|path| pixel_proto::query::looks_like_test_path(path))
                .map(ToString::to_string)
                .collect();
            files.sort();
            files.dedup();
            Ok(files)
        })
    }

    /// A call path on the local graph: uid resolution by name, then the
    /// bounded BFS of `pixel_graph::trace`.
    fn local_flow(&self, from: &str, to: &str) -> Result<Flow, String> {
        self.with_graph(|store| {
            let from_uid = resolve_uid(store, from)?;
            let to_uid = resolve_uid(store, to)?;
            let result = pixel_graph::trace::trace(store, &from_uid, &to_uid, FLOW_DEPTH)
                .map_err(|error| error.to_string())?;
            Ok(if result.found {
                Flow::Path {
                    hops: result.hops.iter().map(|hop| hop.name.clone()).collect(),
                    notes: Vec::new(),
                }
            } else {
                Flow::Absent
            })
        })
    }

    /// `list-signatures` on the local graph: the file's symbols as the
    /// graph recorded them.
    fn local_skeleton(&self, file: &str) -> Result<Vec<SymbolHit>, String> {
        self.with_graph(|store| {
            let row = store
                .file_by_path(file)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("no indexed file matching '{file}'"))?;
            let symbols = store
                .symbols_in_file(row.id)
                .map_err(|error| error.to_string())?;
            Ok(symbols
                .into_iter()
                .map(|row_sym| SymbolHit {
                    uid: row_sym.uid,
                    name: row_sym.name,
                    kind: row_sym.kind.as_str().to_string(),
                    path: row.path.clone(),
                    start_line: u64::from(row_sym.start_line),
                    end_line: u64::from(row_sym.end_line),
                })
                .collect())
        })
    }

    /// `facts.relevance` computed in process over the published text index and,
    /// when it is current, the graph: what the daemon would have attached to
    /// `targets_facts`. Without a graph no keyword is structural and the block
    /// says so.
    fn local_relevance(&self, typed: &str) -> Result<Relevance, String> {
        if !index_current(&self.root) {
            return Err("the text index does not cover HEAD".into());
        }
        let set = IndexSet::open_or_build(&self.root, Box::new(TrigramExtractor))
            .map_err(|error| error.to_string())?;
        if self.with_graph(|_| Ok(())).is_ok() {
            self.with_graph(|store| relevance_on(&set, Some(store), typed))
        } else {
            relevance_on(&set, None, typed)
        }
    }

    /// `evaluate` on the daemon, `trace` when it cannot answer: an `Err`,
    /// a technical `{"kind":"error"}` and an `unknown` verdict all hand
    /// the question to the bounded BFS.
    fn daemon_flow(
        &self,
        from: &str,
        to: &str,
        budget_ms: u64,
        deadline: Instant,
    ) -> Result<Flow, String> {
        let request = Request::Evaluate {
            from: from.to_string(),
            to: to.to_string(),
            traversal: None,
            tiers: None,
            max_depth: None,
            time_budget_ms: Some(budget_ms.max(1)),
            scope: None,
            at_snapshot: true,
        };
        let reason = match self.ask(&request, deadline) {
            Ok(data) => match evaluate_flow(&data) {
                Verdict::Answer(flow) => return Ok(flow),
                Verdict::Unknown(reason) => reason,
                Verdict::Unreadable => "evaluate: unreadable reply".to_string(),
            },
            Err(reason) => reason,
        };
        let data = self.ask(
            &Request::Trace {
                from: from.to_string(),
                to: to.to_string(),
            },
            deadline,
        )?;
        if data.get("found").and_then(Value::as_bool) == Some(true) {
            let hops = data
                .get("hops")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|hop| hop.get("name").and_then(Value::as_str))
                .map(ToString::to_string)
                .collect();
            return Ok(Flow::Path {
                hops,
                notes: vec![format!("evaluate: {reason}")],
            });
        }
        // `trace` resolves names first and answers `{candidates}` on an
        // ambiguous endpoint: that is a naming problem, not a negative.
        if data
            .get("candidates")
            .and_then(Value::as_array)
            .is_some_and(|rows| !rows.is_empty())
        {
            return Err(format!("evaluate: {reason}; trace: ambiguous name"));
        }
        Err(format!(
            "evaluate: {reason}; trace found no path within depth {FLOW_DEPTH}"
        ))
    }
}

impl Evidence for Live {
    fn files_with(&self, anchor: &str, deadline: Instant) -> Result<Found, String> {
        self.files_matching(anchor, deadline).map(Found::from)
    }

    fn files_matching(&self, anchor: &str, deadline: Instant) -> Result<RichFound, String> {
        match self.route {
            Route::Daemon => {
                let request = Request::Search {
                    pattern: regex::escape(anchor),
                    json: true,
                    limit: Some(SEARCH_ROWS),
                    offset: None,
                    paths: None,
                    scope: None,
                    globs: Vec::new(),
                    types: Vec::new(),
                };
                Ok(search_rows(&self.ask(&request, deadline)?))
            }
            Route::Local => self.local_rows(anchor),
        }
    }

    fn concept(&self, phrase: &str, deadline: Instant) -> Result<Found, String> {
        self.concept_matching(phrase, deadline).map(Found::from)
    }

    fn concept_matching(&self, phrase: &str, deadline: Instant) -> Result<RichFound, String> {
        match self.route {
            Route::Daemon => {
                let request = Request::Resolve {
                    phrase: phrase.to_string(),
                    limit: Some(CONCEPT_ROWS),
                };
                Ok(resolve_rows(&self.ask(&request, deadline)?))
            }
            Route::Local => self.local_concept(phrase),
        }
    }

    fn context(
        &self,
        hit: &SymbolHit,
        budget_tokens: usize,
        deadline: Instant,
    ) -> Result<(String, Vec<String>), String> {
        match self.route {
            Route::Daemon => {
                // `context` would otherwise let the daemon open a graph it
                // still needed to build: the gate proves the published one
                // is current first, so the request can only read.
                self.with_graph(|_| Ok(()))?;
                let data = self.ask(
                    &Request::Context {
                        uid: hit.uid.clone(),
                        budget_tokens: Some(budget_tokens),
                    },
                    deadline,
                )?;
                let body = context_body(&data)?;
                Ok((body, caps_of(&data)))
            }
            Route::Local => self
                .local_context(hit, budget_tokens)
                .map(|body| (body, Vec::new())),
        }
    }

    fn test_files(
        &self,
        uid: &str,
        deadline: Instant,
    ) -> Result<(Vec<String>, Vec<String>), String> {
        match self.route {
            Route::Daemon => {
                self.with_graph(|_| Ok(()))?;
                let data = self.ask(
                    &Request::Uses {
                        uid_or_name: uid.to_string(),
                        role: "callers".to_string(),
                        offset: None,
                    },
                    deadline,
                )?;
                if data.get("candidates").is_some() {
                    return Err("the name is ambiguous, a uid is needed".into());
                }
                Ok((pixel_proto::query::caller_test_files(&data), caps_of(&data)))
            }
            Route::Local => self.local_test_files(uid).map(|files| (files, Vec::new())),
        }
    }

    fn flow(
        &self,
        from: &str,
        to: &str,
        budget_ms: u64,
        deadline: Instant,
    ) -> Result<Flow, String> {
        match self.route {
            Route::Daemon => {
                self.with_graph(|_| Ok(()))?;
                self.daemon_flow(from, to, budget_ms, deadline)
            }
            Route::Local => self.local_flow(from, to),
        }
    }

    fn skeleton(&self, file: &str, deadline: Instant) -> Result<Vec<SymbolHit>, String> {
        match self.route {
            Route::Daemon => {
                self.with_graph(|_| Ok(()))?;
                let data = self.ask(
                    &Request::Skeleton {
                        file: file.to_string(),
                    },
                    deadline,
                )?;
                Ok(row_rows(&data, "symbols", "start_line"))
            }
            Route::Local => self.local_skeleton(file),
        }
    }

    fn status(&self, deadline: Instant) -> Result<StatusProbe, String> {
        match self.route {
            Route::Daemon => {
                let data = self.ask(&Request::Status {}, deadline)?;
                Ok(StatusProbe {
                    facts_fresh: data
                        .get("facts")
                        .and_then(|f| f.get("fresh"))
                        .and_then(Value::as_bool),
                })
            }
            // No daemon to ask, and the brief's own graph gate already
            // says whether the local graph is current — the facts index is
            // then a question the run names when it cannot answer it.
            Route::Local => Err("no daemon to probe".into()),
        }
    }

    fn history(
        &self,
        query: &str,
        limit: usize,
        deadline: Instant,
    ) -> Result<(Vec<HistoryHit>, Vec<String>), String> {
        match self.route {
            Route::Daemon => {
                let data = self.ask(
                    &Request::History {
                        query: query.to_string(),
                        facet: Some("all".to_string()),
                        limit: Some(limit),
                        // The facts db exactly as it stands: the request
                        // never ingests, builds or spawns the warmer — an
                        // absent db is an error, not a reason to write.
                        read_only: true,
                    },
                    deadline,
                )?;
                Ok((history_hits(&data)?, caps_of(&data)))
            }
            Route::Local => Err("history needs a running daemon".into()),
        }
    }

    fn task_facts(&self, task: &str, deadline: Instant) -> Result<Vec<String>, String> {
        match self.route {
            Route::Daemon => {
                let data = self.ask(
                    &Request::TargetsFacts {
                        task: task.to_string(),
                        limit: Some(MAX_TARGETS),
                    },
                    deadline,
                )?;
                targets_of(&data).ok_or_else(|| {
                    data.get("reason").and_then(Value::as_str).map_or_else(
                        || "facts did not name targets".to_string(),
                        |reason| format!("facts: {reason}"),
                    )
                })
            }
            Route::Local => Err("task facts need a running daemon".into()),
        }
    }

    fn relevance(&self, typed: &str, deadline: Instant) -> Result<RelevanceAnswer, String> {
        let block = match self.route {
            Route::Daemon => {
                let data = self.ask(
                    &Request::TargetsFacts {
                        task: typed.to_string(),
                        limit: Some(RELEVANCE_TARGETS),
                    },
                    deadline,
                )?;
                relevance_block(&data)?
            }
            Route::Local => self.local_relevance(typed)?,
        };
        Ok(relevance_answer(&block, is_french(typed)))
    }

    fn meaning(
        &self,
        query: &str,
        limit: usize,
        deadline: Instant,
    ) -> Result<Vec<MeaningHit>, String> {
        match self.route {
            Route::Daemon => {
                let data = self.ask(
                    &Request::Meaning {
                        query: query.to_string(),
                        limit: Some(limit),
                    },
                    deadline,
                )?;
                meaning_hits(&data)
            }
            Route::Local => Err("meaning needs a running daemon".into()),
        }
    }

    fn semantic_hint(&self, phrase: &str) -> Option<String> {
        match self.route {
            // Read-only: the probe checks the model's on-disk marker and the
            // vector store's manifest only — it never loads a model,
            // embeds, downloads or writes. A warm index makes
            // `search-meaning` the follow-up; a cold one makes the first
            // call itself the embed step.
            Route::Daemon => {
                let probe = pixel_recall::code_search::warm_probe(&self.root);
                if !probe.model_on_disk {
                    return None;
                }
                let command = format!("pixel search-meaning {}", super::routes::q(phrase));
                Some(if probe.vectors_present {
                    format!("{command} (index warm, {} chunks)", probe.vectors_chunks)
                } else {
                    format!("{command} (model on disk; first run embeds the repo)")
                })
            }
            Route::Local => None,
        }
    }

    fn symbols(&self, name: &str, deadline: Instant) -> Result<Vec<SymbolHit>, String> {
        self.with_graph(|_| Ok(()))?;
        match self.route {
            Route::Daemon => {
                let request = Request::Symbol {
                    name: name.to_string(),
                };
                Ok(symbol_hits(&self.ask(&request, deadline)?))
            }
            Route::Local => self.with_graph(|store| local_symbols(store, name)),
        }
    }

    fn callers(&self, target: &str, deadline: Instant) -> Result<Vec<CallerHit>, String> {
        self.with_graph(|_| Ok(()))?;
        let data = match self.route {
            Route::Daemon => self.ask(
                &Request::Impact {
                    uid_or_name: target.to_string(),
                    direction: "upstream".to_string(),
                    depth: Some(1),
                },
                deadline,
            )?,
            Route::Local => self.with_graph(|store| {
                pixel_daemon::api::impact_on_graph(store, target, "upstream", Some(1))
            })?,
        };
        caller_hits(&data)
    }

    fn line_at(&self, path: &str, line: u64, deadline: Instant) -> Result<String, String> {
        if Instant::now() >= deadline {
            return Err("out of time".to_string());
        }
        let text = std::fs::read_to_string(self.root.join(path)).map_err(|e| e.to_string())?;
        // One line is the smallest read that answers "what is it": a header
        // like `export const X = defineMultiStyleConfig(` carries the
        // right-hand side on the line the index recorded.
        let line = text
            .lines()
            .nth(line.saturating_sub(1) as usize)
            .ok_or_else(|| "line out of range".to_string())?
            .trim();
        if line.is_empty() {
            return Err("empty line".to_string());
        }
        let mut head: String = line.chars().take(140).collect();
        if line.chars().count() > 140 {
            head.push('…');
        }
        Ok(head)
    }
}

/// A connection to the repository's daemon socket whose reads and writes give
/// up when `deadline` does; `None` when nothing listens or no time is left.
fn connect(root: &Path, deadline: Instant) -> Option<UnixStream> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())?;
    let stream = UnixStream::connect(pixel_daemon::socket_path(root)).ok()?;
    stream.set_read_timeout(Some(remaining)).ok()?;
    stream.set_write_timeout(Some(remaining)).ok()?;
    Some(stream)
}

/// Whether a daemon on this build's protocol answers a ping.
fn daemon_serves(root: &Path, deadline: Instant) -> bool {
    connect(root, deadline)
        .and_then(|mut stream| crate::roundtrip(&mut stream, &Request::Ping))
        .is_some_and(|ping| {
            ping.ok
                && ping.data().get("protocol_version").and_then(Value::as_u64)
                    == Some(pixel_daemon::api::PROTOCOL_VERSION)
        })
}

/// Whether opening the text index only reads it: the base shard is this
/// extractor's and sits at HEAD, or a delta pinned to HEAD sits on a base
/// whose commit is still here. Any other state would make
/// `IndexSet::open_or_build` build or diff, which a hook never does.
fn index_current(root: &Path) -> bool {
    let dir = root.join(pixel_index::index::SHARD_DIR);
    let extractor = TrigramExtractor.id();
    let Ok(base) = Shard::open(&dir.join(pixel_index::index::SHARD_FILE)) else {
        return false;
    };
    let (Some(head), Some(base_oid)) = (gitsync::rev_parse_head(root), base.commit_oid()) else {
        return false;
    };
    if base.extractor_id() != extractor {
        return false;
    }
    if base_oid == head {
        return true;
    }
    gitsync::commit_exists(root, base_oid)
        && DeltaState::load(&dir).is_some_and(|state| {
            state.base_oid == base_oid && state.delta_oid.as_deref() == Some(head.as_str())
        })
        && Shard::open(&delta_shard_path(&dir)).is_ok_and(|delta| delta.extractor_id() == extractor)
}

/// The published graph, read-only, once its extractor version and source
/// signature match the tree; any other state is an error that names it.
fn open_current_graph(root: &Path) -> Result<GraphStore, String> {
    let database = root
        .join(pixel_index::index::SHARD_DIR)
        .join(pixel_daemon::api::GRAPH_DB_FILE);
    if !database.is_file() {
        return Err("the graph is not built".into());
    }
    let store = GraphStore::open_read_only(&database).map_err(|error| error.to_string())?;
    store
        .conn()
        .busy_timeout(Duration::ZERO)
        .map_err(|error| error.to_string())?;
    let version = store
        .meta_get(EXTRACTOR_VERSION_KEY)
        .map_err(|error| error.to_string())?;
    if version.as_deref() != Some(EXTRACTOR_VERSION) {
        return Err("the graph extractor is outdated".into());
    }
    let stored = store
        .meta_get(FRESHNESS_KEY)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "the graph freshness is unknown".to_string())?;
    let current =
        freshness_signature_trusting_stat(root, &store).map_err(|error| error.to_string())?;
    if stored != current {
        return Err("the graph is stale".into());
    }
    Ok(store)
}

fn local_symbols(store: &GraphStore, name: &str) -> Result<Vec<SymbolHit>, String> {
    let rows = store
        .symbols_by_name(name, None, SYMBOL_ROWS)
        .map_err(|error| error.to_string())?;
    let mut hits = Vec::with_capacity(rows.len());
    for row in rows {
        let path = store
            .file_by_id(row.file_id)
            .map_err(|error| error.to_string())?
            .map(|file| file.path)
            .unwrap_or_default();
        hits.push(SymbolHit {
            uid: row.uid,
            name: row.name,
            kind: row.kind.as_str().to_string(),
            path,
            start_line: u64::from(row.start_line),
            end_line: u64::from(row.end_line),
        });
    }
    Ok(hits)
}

/// `search` rows (`{path, line, text}`) with the matched text kept.
fn search_rows(data: &Value) -> RichFound {
    RichFound {
        hits: rich_rows(data, "matches", "line"),
        capped: data.get("truncated").and_then(Value::as_bool) == Some(true),
    }
}

/// `resolve` matches (`{path, start_line, kind, score, raw}`) keeping the
/// raw span's text as the row's snippet.
fn resolve_rows(data: &Value) -> RichFound {
    RichFound {
        hits: rich_rows(data, "matches", "start_line"),
        capped: data
            .get("scan_capped")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

/// `list-skeleton`/`skeleton` rows (`{name, kind, path?, start_line}`).
fn row_rows(data: &Value, key: &str, line_key: &str) -> Vec<SymbolHit> {
    data.get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let path = row.get("path").and_then(Value::as_str).unwrap_or_default();
            Some(SymbolHit {
                uid: row
                    .get("uid")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                name: row.get("name")?.as_str()?.to_string(),
                kind: row
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                path: path.to_string(),
                start_line: row.get(line_key).and_then(Value::as_u64).unwrap_or(0),
                end_line: row
                    .get("end_line")
                    .and_then(Value::as_u64)
                    .unwrap_or_else(|| row.get(line_key).and_then(Value::as_u64).unwrap_or(0)),
            })
        })
        .collect()
}

fn rich_rows(data: &Value, key: &str, line_key: &str) -> Vec<RichHit> {
    data.get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let text = row
                .get("text")
                .or_else(|| row.get("raw"))
                .or_else(|| row.get("detail"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(ToString::to_string);
            Some(RichHit {
                path: row.get("path")?.as_str()?.to_string(),
                line: row.get(line_key).and_then(Value::as_u64).unwrap_or(0),
                text,
            })
        })
        .collect()
}

/// What an `evaluate` reply means for the path question.
enum Verdict {
    /// A found path or a proven no-path.
    Answer(Flow),
    /// `verdict: "unknown"` or an error — hand `trace` the same pair.
    Unknown(String),
    /// The reply did not fit any known shape.
    Unreadable,
}

/// `evaluate` replies serialize `pixel_proto::evaluate::Output`: an
/// `evaluation` envelope flattens `Outcome` into the `status`, `witness`,
/// `reason` and `next_actions` fields; `error` carries `code` +
/// `message`. `absent_in_snapshot` is an honest negative, `established`
/// carries the path witness, `unknown` and `error` hand the question to
/// `trace`. The `path`/`none`/`unknown` shapes the first fakes wrote stay
/// readable so a hand-built fixture still parses.
fn evaluate_flow(data: &Value) -> Verdict {
    match data.get("kind").and_then(Value::as_str) {
        Some("evaluation") => evaluation_flow(data),
        Some("path") => {
            let hops: Vec<String> = data
                .get("path")
                .or_else(|| data.get("hops"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|hop| {
                    hop.get("name")
                        .or_else(|| hop.get("uid"))
                        .and_then(Value::as_str)
                        .map(ToString::to_string)
                })
                .collect();
            let notes: Vec<String> = data
                .get("notes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect();
            Verdict::Answer(Flow::Path { hops, notes })
        }
        Some("none") => Verdict::Answer(Flow::Absent),
        Some("unknown") => Verdict::Unknown(
            data.get("reason")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
        ),
        Some("error") => Verdict::Unknown(
            data.get("message")
                .or_else(|| data.get("reason"))
                .and_then(Value::as_str)
                .unwrap_or("error")
                .to_string(),
        ),
        _ => Verdict::Unreadable,
    }
}

/// The `status` of a `{"kind":"evaluation"}` envelope. `absent_in_snapshot`
/// is a real answer — the relation was exhausted, so no `trace` round-trip
/// is spent confirming it.
fn evaluation_flow(data: &Value) -> Verdict {
    match data.get("status").and_then(Value::as_str) {
        Some("established") => Verdict::Answer(Flow::Path {
            hops: witness_hops(data.get("witness")),
            notes: Vec::new(),
        }),
        Some("absent_in_snapshot") => Verdict::Answer(Flow::Absent),
        Some("unknown") => Verdict::Unknown(evaluation_reason(data)),
        _ => Verdict::Unreadable,
    }
}

/// The witness a `path`/`identity` evaluation carries. Each `SymbolRef`
/// renders as the qualified segment of its `path#qualified#kind` uid —
/// first the `from` of the first edge, then every edge's `to`. `identity`
/// is a zero-length path: the target is itself the source.
fn witness_hops(witness: Option<&Value>) -> Vec<String> {
    let Some(witness) = witness else {
        return Vec::new();
    };
    let name = |symbol: &Value| {
        symbol
            .get("uid")
            .and_then(Value::as_str)
            .map(|uid| uid.split('#').nth(1).unwrap_or(uid).to_string())
    };
    match witness.get("kind").and_then(Value::as_str) {
        Some("path") => {
            let mut hops = Vec::new();
            for (i, edge) in witness
                .get("edges")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
            {
                if i == 0
                    && let Some(from) = edge.get("from").and_then(name)
                {
                    hops.push(from);
                }
                if let Some(to) = edge.get("to").and_then(name) {
                    hops.push(to);
                }
            }
            hops
        }
        Some("identity") => witness.get("symbol").and_then(name).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// `reason.code` names why the predicate could not be evaluated —
/// `graph_stale`, `ambiguous_symbol`, `symbol_not_found` and friends —
/// humanised for the `unresolved:` line, with the argument appended when
/// the reason names one.
fn evaluation_reason(data: &Value) -> String {
    let reason = data.get("reason");
    let Some(code) = reason.and_then(|r| r.get("code")).and_then(Value::as_str) else {
        return "unknown".to_string();
    };
    match reason
        .and_then(|r| r.get("argument"))
        .and_then(Value::as_str)
    {
        Some(argument) => format!("{}: {argument}", code.replace('_', " ")),
        None => code.replace('_', " "),
    }
}

/// The advisory strings an op's envelope or body carried under `caps` —
/// truncation, lower-bound and witness notes the brief must not drop.
fn caps_of(data: &Value) -> Vec<String> {
    let read = |list: &Value| {
        list.as_array().map(|rows| {
            rows.iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect::<Vec<String>>()
        })
    };
    let mut caps = data.get("caps").and_then(read).unwrap_or_default();
    if let Some(envelope) = data.get("envelope") {
        caps.extend(envelope.get("caps").and_then(read).unwrap_or_default());
    }
    caps
}

/// `history`/`dig-history` candidates (`{oid|sha, subject|message}`).
fn history_hits(data: &Value) -> Result<Vec<HistoryHit>, String> {
    let rows = data
        .get("candidates")
        .or_else(|| data.get("matches"))
        .or_else(|| data.get("results"))
        .and_then(Value::as_array)
        .ok_or_else(|| "the reply carries no history rows".to_string())?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let sha = row
                .get("sha")
                .or_else(|| row.get("oid"))
                .or_else(|| row.get("commit_oid"))
                .and_then(Value::as_str)?
                .chars()
                .take(10)
                .collect();
            let subject = row
                .get("subject")
                .or_else(|| row.get("message"))
                .and_then(Value::as_str)
                .map(|s| s.lines().next().unwrap_or("").trim().to_string())
                .unwrap_or_default();
            Some(HistoryHit { sha, subject })
        })
        .collect())
}

/// `targets_facts` rows (`facts.targets[].path`) — the files the task
/// facts index names for the task.
fn targets_of(data: &Value) -> Option<Vec<String>> {
    let paths: Vec<String> = data
        .get("targets")
        .or_else(|| data.get("facts")?.get("targets"))
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|row| {
            row.get("path")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .collect();
    Some(paths)
}

/// The `facts.relevance` block of a `targets_facts` reply, or why there is
/// none: an unavailable result names its reason, an older daemon's result has
/// no block at all.
fn relevance_block(data: &Value) -> Result<Relevance, String> {
    if data.get("status").and_then(Value::as_str) == Some("unavailable") {
        return Err(data.get("reason").and_then(Value::as_str).map_or_else(
            || "facts are unavailable".to_string(),
            |reason| format!("facts: {reason}"),
        ));
    }
    let block = data
        .get("facts")
        .and_then(|facts| facts.get("relevance"))
        .or_else(|| data.get("relevance"))
        .ok_or_else(|| "facts carry no relevance".to_string())?;
    serde_json::from_value(block.clone()).map_err(|error| format!("facts.relevance: {error}"))
}

/// Whether the prompt is French, as the task tokenizer decides it.
fn is_french(typed: &str) -> bool {
    pixel_rank::tokenize_task(typed)
        .is_ok_and(|query| query.language == pixel_rank::TaskLanguage::French)
}

/// The scorer's input from the daemon's block: each keyword weighs what
/// `row_weight` says, each co-file what the daemon summed, and a French word
/// the repository lacks entirely (no match as typed, none through a synonym)
/// is marked so the scorer gives it no weight. The co-files' lines ride along
/// in the daemon's order.
fn relevance_answer(block: &Relevance, french: bool) -> RelevanceAnswer {
    RelevanceAnswer {
        input: RelevanceInput {
            graph: block.graph,
            keywords: block
                .keywords
                .iter()
                .map(|row| KeywordStat {
                    keyword: row.keyword.clone(),
                    weight: row_weight(row, block.files_considered),
                    french_only: french && keyword_df(row) == 0 && row.via_expansion.is_none(),
                })
                .collect(),
            cofiles: block
                .cofiles
                .iter()
                .map(|cofile| CoFileStat {
                    path: cofile.path.clone(),
                    keywords: cofile.keywords.clone(),
                    weight: cofile.weight,
                    structural: cofile.structural,
                })
                .collect(),
        },
        lines: block
            .cofiles
            .iter()
            .map(|cofile| RichHit {
                path: cofile.path.clone(),
                line: cofile.line.map_or(0, u64::from),
                text: cofile
                    .text
                    .as_deref()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(ToString::to_string),
            })
            .collect(),
    }
}

/// The leads of a `meaning` reply; vectors that are not ready are an error
/// naming why, so the brief falls back to the relevance co-files.
fn meaning_hits(data: &Value) -> Result<Vec<MeaningHit>, String> {
    let result: MeaningResult = serde_json::from_value(data.clone())
        .map_err(|error| format!("meaning: unreadable reply: {error}"))?;
    match result {
        MeaningResult::Ready { hits, .. } => Ok(hits
            .into_iter()
            .map(|hit| MeaningHit {
                path: hit.path,
                start_line: hit.start_line,
                end_line: hit.end_line,
                symbol: hit.symbol,
                score: hit.score,
                snippet: hit.snippet,
            })
            .collect()),
        MeaningResult::Unavailable { reason, detail, .. } => Err(match detail {
            Some(detail) => format!("meaning {}: {detail}", reason.as_str()),
            None => format!("meaning {}", reason.as_str()),
        }),
    }
}

/// `pack-context`/`context` replies carry the packed source under
/// `body`/`context`/`text`; a reply that only names the uid is no
/// body at all.
fn context_body(data: &Value) -> Result<String, String> {
    if let Some(raw) = data
        .get("body")
        .or_else(|| data.get("context"))
        .and_then(Value::as_str)
        .filter(|body| !body.is_empty())
    {
        return Ok(raw.to_string());
    }
    let Some(rendered) = data
        .get("text")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    else {
        return Err("the reply carries no source body".to_string());
    };
    // `text` is the rendered pack; a signature-only reply carries no
    // snippet lines and yields an empty body, not an error.
    Ok(target_snippet(rendered).unwrap_or_default())
}

/// The target item's body inside pack-context's rendered text: its first
/// line is the item's own header (`path:range kind name — sig`), which a
/// caller rendering a `defined` line already prints, so only the indented
/// snippet lines that follow — including `… body cut` and `crux:` markers
/// — belong in the body. `None` when the pack carried signatures only.
fn target_snippet(rendered: &str) -> Option<String> {
    let mut lines = rendered.lines();
    let first = lines.next()?;
    let mut kept: Vec<&str> = Vec::new();
    // A pack that starts mid-body (no header line) keeps its first line.
    if first.starts_with("    ") {
        kept.push(first);
    }
    for line in lines {
        if !line.starts_with("    ") {
            break;
        }
        kept.push(line);
    }
    if kept.is_empty() {
        return None;
    }
    Some(
        kept.iter()
            .map(|line| line.trim_start())
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// A concept match's best snippet: its detail when it differs from the
/// raw span, else the raw span itself.
fn concept_text(m: &pixel_graph::concept_resolve::ConceptMatch) -> Option<String> {
    let text = if m.detail.is_empty() {
        &m.raw
    } else {
        &m.detail
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.chars().take(140).collect())
}

/// Name → uid on the graph store: one exact symbol, or an ambiguity/
/// absence error that names what happened.
fn resolve_uid(store: &GraphStore, name_or_uid: &str) -> Result<String, String> {
    if store
        .symbol_by_uid(name_or_uid)
        .map_err(|error| error.to_string())?
        .is_some()
    {
        return Ok(name_or_uid.to_string());
    }
    let rows = store
        .symbols_by_name(name_or_uid, None, 2)
        .map_err(|error| error.to_string())?;
    match rows.len() {
        1 => Ok(rows[0].uid.clone()),
        0 => Err(format!("no symbol named '{name_or_uid}'")),
        _ => Err(format!("'{name_or_uid}' names more than one symbol")),
    }
}

/// `symbol` rows (`{uid, name, kind, path, start_line, end_line}`).
fn symbol_hits(data: &Value) -> Vec<SymbolHit> {
    data.get("symbols")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let text = |key: &str| row.get(key)?.as_str().map(ToString::to_string);
            let number = |key: &str| row.get(key).and_then(Value::as_u64).unwrap_or(0);
            Some(SymbolHit {
                uid: text("uid")?,
                name: text("name")?,
                kind: text("kind")?,
                path: text("path")?,
                start_line: number("start_line"),
                end_line: number("end_line"),
            })
        })
        .collect()
}

/// The direct callers of an `impact` reply, one per file and symbol. A name
/// that matched several symbols answers with candidates instead.
fn caller_hits(data: &Value) -> Result<Vec<CallerHit>, String> {
    if data.get("candidates").is_some() {
        return Err("the name is ambiguous, a uid is needed".into());
    }
    let direct = data
        .get("d1_will_break")
        .and_then(Value::as_array)
        .ok_or_else(|| "the reply carries no direct callers".to_string())?;
    let mut seen: HashSet<(&str, &str)> = HashSet::new();
    let mut callers = Vec::with_capacity(direct.len());
    for item in direct {
        let (Some(path), Some(via)) = (
            item.get("path").and_then(Value::as_str),
            item.get("name").and_then(Value::as_str),
        ) else {
            continue;
        };
        if seen.insert((path, via)) {
            callers.push(CallerHit {
                path: path.to_string(),
                via: via.to_string(),
                line: item.get("line").and_then(Value::as_u64).unwrap_or(0),
            });
        }
    }
    Ok(callers)
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use serde_json::json;

    use super::*;

    const WINDOW: Duration = Duration::from_secs(5);

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pixel-brief-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    /// A daemon on `root`'s socket that answers each request kind with a
    /// canned result and records every request it sees.
    struct FakeDaemon {
        seen: Arc<Mutex<Vec<Value>>>,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl FakeDaemon {
        fn start(root: &Path, protocol: u64) -> Self {
            let socket = pixel_daemon::socket_path(root);
            let _ = std::fs::remove_file(&socket);
            let listener = UnixListener::bind(&socket).unwrap();
            listener.set_nonblocking(true).unwrap();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let (log, halt) = (Arc::clone(&seen), Arc::clone(&stop));
            let thread = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(10);
                while !halt.load(Ordering::SeqCst) && Instant::now() < deadline {
                    let Ok((mut stream, _)) = listener.accept() else {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    };
                    // BSD hands back a non-blocking socket from a non-blocking listener.
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let mut line = String::new();
                    if BufReader::new(stream.try_clone().unwrap())
                        .read_line(&mut line)
                        .unwrap_or(0)
                        == 0
                    {
                        continue;
                    }
                    let request: Value = serde_json::from_str(&line).unwrap();
                    log.lock().unwrap().push(request.clone());
                    let result = match request["op"].as_str() {
                        Some("ping") => json!({"pong": true, "protocol_version": protocol}),
                        Some("search") => json!({"matches": [
                            {"path": "src/a.ts", "line": 4, "text": "x"},
                            {"path": "src/a.ts", "line": 9, "text": "y"},
                            {"path": "data/out.json", "line": 1, "text": "z"}
                        ], "truncated": true}),
                        Some("resolve") => json!({"matches": [
                            {"path": "src/retry.ts", "start_line": 12, "kind": "function"}
                        ]}),
                        Some("symbol") => json!({"symbols": [
                            {"uid": "src/a.ts#go#function", "name": "go", "kind": "function",
                             "path": "src/a.ts", "start_line": 3, "end_line": 8}
                        ]}),
                        Some("impact") => json!({"d1_will_break": [
                            {"path": "src/b.ts", "name": "main", "line": 7}
                        ]}),
                        Some("uses") => json!({"edges": [
                            {"symbol": {"path": "tests/a_test.rs"}},
                            {"symbol": {"path": "src/lib.rs"}},
                            {"symbol": {"path": "tests/a_test.rs"}}
                        ]}),
                        Some("context") => json!({"body": "fn go() {\n  go_body();\n}"}),
                        Some("evaluate") => json!({"kind": "evaluation",
                            "predicate": "path", "status": "established",
                            "answer": true,
                            "witness": {"kind": "path", "probable_edges": 0,
                                "edges": [
                                    {"from": {"uid": "src/a.ts#go#function"},
                                     "to": {"uid": "src/a.ts#mid#function"}},
                                    {"from": {"uid": "src/a.ts#mid#function"},
                                     "to": {"uid": "src/b.ts#stop#function"}}
                                ]},
                            "reason": null, "next_actions": []}),
                        Some("trace") => json!({"found": true, "hops": [
                            {"name": "go"}, {"name": "stop"}
                        ]}),
                        Some("skeleton") => json!({"symbols": [
                            {"uid": "cfg/x.json#top#const", "name": "top", "kind": "const",
                             "path": "cfg/x.json", "start_line": 1, "end_line": 3}
                        ]}),
                        Some("status") => json!({"facts": {"present": true, "fresh": true},
                            "embedding": {"model_on_disk": true, "vectors_present": false,
                                          "vectors_chunks": 0, "embedder_resident": false}}),
                        Some("history") => json!({"candidates": [
                            {"oid": "0123456789abcdef", "subject": "add the retry loop"}
                        ]}),
                        Some("targets_facts") => json!({
                            "targets": [{"path": "src/flag.ts"}, {"path": "cfg/app.toml"}],
                            "facts": {"relevance": {
                                "files_considered": 100,
                                "graph": true,
                                "keywords": [
                                    {"keyword": "daemon", "content_files": 4, "symbol_files": 2},
                                    {"keyword": "weather"},
                                    {"keyword": "the", "content_files": 60}
                                ],
                                "cofiles": [
                                    {"path": "src/daemon.ts", "keywords": ["daemon"],
                                     "weight": 3.006, "structural": true,
                                     "line": 280, "text": "  fn watch_ready  "},
                                    {"path": "docs/notes.md", "keywords": ["daemon"],
                                     "weight": 1.5}
                                ]
                            }}
                        }),
                        Some("meaning") => json!({
                            "status": "ready",
                            "hits": [
                                {"path": "src/a.ts", "start_line": 3, "end_line": 12,
                                 "symbol": "go", "score": 0.5, "snippet": "function go() {"},
                                {"path": "src/b.ts", "start_line": 7, "end_line": 9,
                                 "score": 0.25, "snippet": ""}
                            ],
                            "pool": {"model": "m", "dims": 4, "files": 2, "chunks": 5,
                                     "eligible_files": 2, "generation": 1, "age_ms": 10,
                                     "resident_bytes": 100},
                            "caps": []
                        }),
                        _ => json!({}),
                    };
                    let reply = pixel_daemon::Response::success("test", result);
                    let _ = writeln!(stream, "{}", serde_json::to_string(&reply).unwrap());
                }
            });
            Self {
                seen,
                stop,
                thread: Some(thread),
            }
        }

        fn requests(&self) -> Vec<Value> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl Drop for FakeDaemon {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    #[test]
    fn search_rows_should_read_text_and_the_truncation_flag() {
        let data = json!({"matches": [
            {"path": "src/a.ts", "line": 4, "text": "x"},
            {"line": 5},
            {"path": "src/b.ts"}
        ], "truncated": true});
        assert_eq!(
            search_rows(&data),
            RichFound {
                hits: vec![
                    RichHit {
                        path: "src/a.ts".into(),
                        line: 4,
                        text: Some("x".into())
                    },
                    RichHit {
                        path: "src/b.ts".into(),
                        line: 0,
                        text: None
                    }
                ],
                capped: true
            }
        );
        assert_eq!(search_rows(&json!({})), RichFound::default());
        assert!(!search_rows(&json!({"matches": [], "truncated": false})).capped);
    }

    #[test]
    fn resolve_rows_should_read_the_start_line_and_raw_text_of_each_match() {
        let data = json!({"matches": [
            {"path": "app/routes.rb", "start_line": 12, "kind": "route", "score": 0.9, "raw": "x"}
        ]});
        assert_eq!(
            resolve_rows(&data).hits,
            [RichHit {
                path: "app/routes.rb".into(),
                line: 12,
                text: Some("x".into())
            }]
        );
    }

    #[test]
    fn symbol_hits_should_read_the_daemon_symbol_shape_and_skip_incomplete_rows() {
        let data = json!({"symbols": [
            {"uid": "a.ts#go#function", "name": "go", "qualified": "go", "kind": "function",
             "path": "a.ts", "start_line": 3, "end_line": 9, "sig": "go()"},
            {"name": "orphan"}
        ], "envelope": {}});
        assert_eq!(
            symbol_hits(&data),
            [SymbolHit {
                uid: "a.ts#go#function".into(),
                name: "go".into(),
                kind: "function".into(),
                path: "a.ts".into(),
                start_line: 3,
                end_line: 9
            }]
        );
        assert_eq!(symbol_hits(&json!({})), []);
    }

    #[test]
    fn caller_hits_should_list_direct_callers_once_per_file_and_symbol() {
        let data = json!({"target": "t", "d1_will_break": [
            {"uid": "u1", "path": "src/b.ts", "name": "main", "line": 7, "tier": "exact"},
            {"uid": "u2", "path": "src/b.ts", "name": "main", "line": 9, "tier": "exact"},
            {"uid": "u3", "path": "src/c.ts", "name": "main"},
            {"uid": "u4", "name": "nameless-path"}
        ], "d2_likely_affected": [{"path": "src/d.ts", "name": "far", "line": 1}]});
        assert_eq!(
            caller_hits(&data).unwrap(),
            [
                CallerHit {
                    path: "src/b.ts".into(),
                    via: "main".into(),
                    line: 7
                },
                CallerHit {
                    path: "src/c.ts".into(),
                    via: "main".into(),
                    line: 0
                }
            ]
        );
        assert_eq!(
            caller_hits(&json!({"d1_will_break": []})).unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn caller_hits_should_refuse_an_ambiguous_name_and_a_reply_without_callers() {
        let ambiguous = json!({"candidates": [{"uid": "a"}, {"uid": "b"}], "hint": "ambiguous"});
        assert_eq!(
            caller_hits(&ambiguous).unwrap_err(),
            "the name is ambiguous, a uid is needed"
        );
        assert_eq!(
            caller_hits(&json!({"summary": "x"})).unwrap_err(),
            "the reply carries no direct callers"
        );
    }

    #[test]
    fn index_current_should_be_false_without_a_shard() {
        let root = scratch("no-shard");
        assert!(!index_current(&root));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn graph_should_be_refused_when_missing_or_not_a_current_graph() {
        let root = scratch("no-graph");
        assert_eq!(
            open_current_graph(&root).err().as_deref(),
            Some("the graph is not built")
        );
        let dir = root.join(pixel_index::index::SHARD_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        drop(GraphStore::open(&dir.join(pixel_daemon::api::GRAPH_DB_FILE)).unwrap());
        assert_eq!(
            open_current_graph(&root).err().as_deref(),
            Some("the graph extractor is outdated")
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn graph_should_be_refused_when_its_signature_no_longer_matches_the_tree() {
        let root = scratch("stale-graph");
        let dir = root.join(pixel_index::index::SHARD_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let store = GraphStore::open(&dir.join(pixel_daemon::api::GRAPH_DB_FILE)).unwrap();
        store
            .meta_set(EXTRACTOR_VERSION_KEY, EXTRACTOR_VERSION)
            .unwrap();
        drop(store);
        assert_eq!(
            open_current_graph(&root).err().as_deref(),
            Some("the graph freshness is unknown")
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_failed_graph_check_should_fail_symbols_and_callers_without_asking_the_daemon() {
        let root = scratch("graph-gate");
        let daemon = FakeDaemon::start(&root, pixel_daemon::api::PROTOCOL_VERSION);
        let live = Live::new(&root, Route::Daemon);
        let deadline = Instant::now() + WINDOW;
        assert_eq!(
            live.symbols("go", deadline).unwrap_err(),
            "the graph is not built"
        );
        assert_eq!(
            live.callers("go", deadline).unwrap_err(),
            "the graph is not built"
        );
        assert_eq!(daemon.requests(), Vec::<Value>::new());
        drop(daemon);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn open_should_choose_the_daemon_only_for_a_ping_on_this_protocol() {
        let root = scratch("route");
        let deadline = Instant::now() + WINDOW;
        assert!(!daemon_serves(&root, deadline));
        {
            let _daemon = FakeDaemon::start(&root, pixel_daemon::api::PROTOCOL_VERSION);
            assert!(daemon_serves(&root, deadline));
        }
        {
            let _older = FakeDaemon::start(&root, pixel_daemon::api::PROTOCOL_VERSION - 1);
            assert!(!daemon_serves(&root, deadline));
        }
        assert!(!daemon_serves(&root, Instant::now()));
        let _ = std::fs::remove_file(pixel_daemon::socket_path(&root));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn daemon_route_should_send_each_operation_as_its_request_and_read_the_reply() {
        let root = scratch("daemon-route");
        let daemon = FakeDaemon::start(&root, pixel_daemon::api::PROTOCOL_VERSION);
        let live = Live::new(&root, Route::Daemon);
        // A graph the readiness check already proved current.
        *live.graph.lock().unwrap() = Some(Ok(GraphStore::open_in_memory().unwrap()));
        let deadline = Instant::now() + WINDOW;

        let files = live.files_with("a.b", deadline).unwrap();
        assert_eq!(
            files
                .hits
                .iter()
                .map(|hit| (hit.path.as_str(), hit.line))
                .collect::<Vec<_>>(),
            [("src/a.ts", 4), ("src/a.ts", 9), ("data/out.json", 1)]
        );
        assert!(files.capped);
        assert_eq!(
            live.concept("retry logic", deadline).unwrap().hits[0].path,
            "src/retry.ts"
        );
        assert_eq!(
            live.symbols("go", deadline).unwrap()[0].uid,
            "src/a.ts#go#function"
        );
        assert_eq!(
            live.callers("src/a.ts#go#function", deadline).unwrap(),
            [CallerHit {
                path: "src/b.ts".into(),
                via: "main".into(),
                line: 7
            }]
        );

        let requests = daemon.requests();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[0]["op"], "search");
        assert_eq!(requests[0]["pattern"], r"a\.b");
        assert_eq!(requests[0]["json"], true);
        assert_eq!(requests[0]["limit"], SEARCH_ROWS);
        assert_eq!(requests[1]["op"], "resolve");
        assert_eq!(requests[1]["phrase"], "retry logic");
        assert_eq!(requests[1]["limit"], CONCEPT_ROWS);
        assert_eq!(requests[2]["op"], "symbol");
        assert_eq!(requests[2]["name"], "go");
        assert_eq!(requests[3]["op"], "impact");
        assert_eq!(requests[3]["uid_or_name"], "src/a.ts#go#function");
        assert_eq!(requests[3]["direction"], "upstream");
        assert_eq!(requests[3]["depth"], 1);
        drop(daemon);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn daemon_route_should_send_the_kind_operations_and_read_their_replies() {
        let root = scratch("daemon-kinds");
        let daemon = FakeDaemon::start(&root, pixel_daemon::api::PROTOCOL_VERSION);
        let live = Live::new(&root, Route::Daemon);
        *live.graph.lock().unwrap() = Some(Ok(GraphStore::open_in_memory().unwrap()));
        let deadline = Instant::now() + WINDOW;
        let hit = SymbolHit {
            uid: "src/a.ts#go#function".into(),
            name: "go".into(),
            kind: "function".into(),
            path: "src/a.ts".into(),
            start_line: 3,
            end_line: 8,
        };

        assert_eq!(
            live.files_matching("a.b", deadline).unwrap().hits[0]
                .text
                .as_deref(),
            Some("x")
        );
        assert_eq!(
            live.context(&hit, 400, deadline).unwrap().0,
            "fn go() {\n  go_body();\n}"
        );
        assert_eq!(
            live.test_files(&hit.uid, deadline).unwrap().0,
            ["tests/a_test.rs"]
        );
        assert_eq!(
            live.flow("go", "stop", 100, deadline).unwrap(),
            Flow::Path {
                hops: vec!["go".into(), "mid".into(), "stop".into()],
                notes: vec![]
            }
        );
        assert_eq!(
            live.skeleton("cfg/x.json", deadline).unwrap()[0].name,
            "top"
        );
        assert_eq!(
            live.status(deadline).unwrap(),
            StatusProbe {
                facts_fresh: Some(true)
            }
        );
        assert_eq!(
            live.history("retry", 3, deadline).unwrap().0,
            [HistoryHit {
                sha: "0123456789".into(),
                subject: "add the retry loop".into()
            }]
        );
        assert_eq!(
            live.task_facts("add flag", deadline).unwrap(),
            ["src/flag.ts", "cfg/app.toml"]
        );

        let requests = daemon.requests();
        let ops: Vec<&str> = requests
            .iter()
            .map(|req| req["op"].as_str().unwrap_or("?"))
            .collect();
        assert_eq!(
            ops,
            [
                "search",
                "context",
                "uses",
                "evaluate",
                "skeleton",
                "status",
                "history",
                "targets_facts"
            ]
        );
        assert_eq!(requests[1]["uid"], "src/a.ts#go#function");
        assert_eq!(requests[1]["budget_tokens"], 400);
        assert_eq!(requests[2]["uid_or_name"], "src/a.ts#go#function");
        assert_eq!(requests[2]["role"], "callers");
        assert_eq!(requests[3]["from"], "go");
        assert_eq!(requests[3]["to"], "stop");
        assert_eq!(requests[3]["at_snapshot"], true);
        assert_eq!(requests[4]["file"], "cfg/x.json");
        // Read-only history: the flag rides the wire only when set, so its
        // presence here is the contract — an absent flag would let a daemon
        // take the ingest-and-warm path a hook must never start.
        assert_eq!(requests[6]["read_only"], true);
        assert_eq!(requests[7]["task"], "add flag");
        drop(daemon);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn daemon_flow_should_fall_back_to_trace_when_evaluate_cannot_answer() {
        // `evaluate` answered `unknown`; `trace` still found the path —
        // the flow carries the path and the reason evaluate gave up.
        let data = json!({"kind": "unknown", "reason": "budget"});
        assert!(matches!(evaluate_flow(&data), Verdict::Unknown(r) if r == "budget"));
        let none = json!({"kind": "none"});
        assert!(matches!(
            evaluate_flow(&none),
            Verdict::Answer(Flow::Absent)
        ));
        let err = json!({"kind": "error", "message": "no uid"});
        assert!(matches!(evaluate_flow(&err), Verdict::Unknown(r) if r == "no uid"));
        assert!(matches!(
            evaluate_flow(&json!({"unexpected": true})),
            Verdict::Unreadable
        ));
    }

    #[test]
    fn evaluate_flow_should_read_the_real_evaluation_envelope() {
        // The daemon serializes `evaluate::Output`: `established` carries
        // the witness edges, `absent_in_snapshot` is a final answer that
        // needs no trace, `unknown` carries the structured reason.
        let established = json!({"kind": "evaluation", "status": "established",
        "answer": true, "predicate": "path",
        "witness": {"kind": "path", "probable_edges": 0, "edges": [
            {"from": {"uid": "a.ts#go#function"}, "to": {"uid": "a.ts#mid#function"}},
            {"from": {"uid": "a.ts#mid#function"}, "to": {"uid": "b.ts#stop#function"}}
        ]}});
        assert!(
            matches!(evaluate_flow(&established), Verdict::Answer(Flow::Path { hops, .. }) if hops == ["go", "mid", "stop"])
        );
        let identity = json!({"kind": "evaluation", "status": "established",
            "witness": {"kind": "identity", "symbol": {"uid": "a.ts#go#function"}}});
        assert!(
            matches!(evaluate_flow(&identity), Verdict::Answer(Flow::Path { hops, .. }) if hops == ["go"])
        );
        let absent = json!({"kind": "evaluation", "status": "absent_in_snapshot",
            "answer": false});
        assert!(matches!(
            evaluate_flow(&absent),
            Verdict::Answer(Flow::Absent)
        ));
        let unknown = json!({"kind": "evaluation", "status": "unknown",
            "answer": null, "reason": {"code": "graph_stale"}});
        assert!(matches!(evaluate_flow(&unknown), Verdict::Unknown(r) if r == "graph stale"));
        let ambiguous = json!({"kind": "evaluation", "status": "unknown",
            "reason": {"code": "ambiguous_symbol", "argument": "go",
                       "candidates": []}});
        assert!(
            matches!(evaluate_flow(&ambiguous), Verdict::Unknown(r) if r == "ambiguous symbol: go")
        );
        let no_status = json!({"kind": "evaluation"});
        assert!(matches!(evaluate_flow(&no_status), Verdict::Unreadable));
    }

    #[test]
    fn history_hits_should_take_the_first_subject_line_and_a_short_sha() {
        let data = json!({"candidates": [
            {"oid": "0123456789abcdef", "subject": "first line\nsecond line"},
            {"sha": "abc", "message": "subject via message"},
            {"subject": "no sha"}
        ]});
        assert_eq!(
            history_hits(&data).unwrap(),
            [
                HistoryHit {
                    sha: "0123456789".into(),
                    subject: "first line".into()
                },
                HistoryHit {
                    sha: "abc".into(),
                    subject: "subject via message".into()
                }
            ]
        );
        assert_eq!(
            history_hits(&json!({})).unwrap_err(),
            "the reply carries no history rows"
        );
    }

    #[test]
    fn targets_of_should_read_either_the_top_or_facts_list() {
        assert_eq!(
            targets_of(&json!({"targets": [{"path": "a.ts"}, {"nope": 1}] })).unwrap(),
            ["a.ts"]
        );
        assert_eq!(
            targets_of(&json!({"facts": {"targets": [{"path": "b.ts"}]}})).unwrap(),
            ["b.ts"]
        );
        assert!(targets_of(&json!({})).is_none());
    }

    #[test]
    fn context_body_should_take_the_packed_source_or_fail() {
        assert_eq!(
            context_body(&json!({"body": "fn go() {}"})).unwrap(),
            "fn go() {}"
        );
        assert_eq!(
            context_body(&json!({"context": "packed"})).unwrap(),
            "packed"
        );
        assert_eq!(
            context_body(&json!({"uid": "x"})).unwrap_err(),
            "the reply carries no source body"
        );
    }

    #[test]
    fn daemon_route_should_fail_instead_of_waiting_when_no_time_is_left() {
        let root = scratch("daemon-late");
        let _daemon = FakeDaemon::start(&root, pixel_daemon::api::PROTOCOL_VERSION);
        let live = Live::new(&root, Route::Daemon);
        assert_eq!(
            live.files_with("x", Instant::now()).unwrap_err(),
            "no daemon answered within the window"
        );
    }

    #[test]
    fn local_route_should_refuse_a_concept_search_and_an_uncovered_text_index() {
        let root = scratch("local-refusals");
        let live = Live::new(&root, Route::Local);
        let deadline = Instant::now() + WINDOW;
        // The local concept cascade runs on the open graph store: no graph,
        // no answer — the error names the missing prerequisite.
        assert_eq!(
            live.concept("anything", deadline).unwrap_err(),
            "the graph is not built"
        );
        assert_eq!(
            live.files_with("anything", deadline).unwrap_err(),
            "the text index does not cover HEAD"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn daemon_route_should_ask_targets_facts_for_the_relevance_block_and_weigh_it_the_daemons_way()
    {
        let root = scratch("daemon-relevance");
        let daemon = FakeDaemon::start(&root, pixel_daemon::api::PROTOCOL_VERSION);
        let live = Live::new(&root, Route::Daemon);
        let deadline = Instant::now() + WINDOW;

        let answer = live
            .relevance("the daemon and the weather", deadline)
            .unwrap();
        assert_eq!(
            daemon.requests(),
            [json!({"op": "targets_facts", "task": "the daemon and the weather", "limit": 8})]
        );
        let input = &answer.input;
        assert!(input.graph);
        // `daemon` is in 4 of 100 files: ln(101 / 5); the weather is in none,
        // which weighs the cap; `the` is in 60 of 100, ubiquitous, nothing.
        let weights: Vec<(&str, f64)> = input
            .keywords
            .iter()
            .map(|stat| (stat.keyword.as_str(), stat.weight))
            .collect();
        assert_eq!(weights.len(), 3);
        assert_eq!(weights[0].0, "daemon");
        assert!(
            (weights[0].1 - (101.0_f64 / 5.0).ln()).abs() < 1e-9,
            "{weights:?}"
        );
        assert_eq!(weights[1].0, "weather");
        assert!((weights[1].1 - pixel_daemon::relevance::IDF_CAP).abs() < 1e-9);
        assert_eq!(weights[2].0, "the");
        assert!(weights[2].1.abs() < f64::EPSILON);
        assert!(input.keywords.iter().all(|stat| !stat.french_only));
        // The files carry the weight the daemon summed, in its order.
        assert_eq!(
            input.cofiles,
            [
                CoFileStat {
                    path: "src/daemon.ts".into(),
                    keywords: vec!["daemon".into()],
                    weight: 3.006,
                    structural: true,
                },
                CoFileStat {
                    path: "docs/notes.md".into(),
                    keywords: vec!["daemon".into()],
                    weight: 1.5,
                    structural: false,
                },
            ]
        );
        assert_eq!(
            answer.lines,
            [
                RichHit {
                    path: "src/daemon.ts".into(),
                    line: 280,
                    text: Some("fn watch_ready".into()),
                },
                RichHit {
                    path: "docs/notes.md".into(),
                    line: 0,
                    text: None,
                },
            ]
        );
        drop(daemon);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn daemon_route_should_ask_the_meaning_op_and_read_its_leads() {
        let root = scratch("daemon-meaning");
        let daemon = FakeDaemon::start(&root, pixel_daemon::api::PROTOCOL_VERSION);
        let live = Live::new(&root, Route::Daemon);
        let deadline = Instant::now() + WINDOW;

        let leads = live.meaning("how the daemon starts", 8, deadline).unwrap();
        assert_eq!(
            daemon.requests(),
            [json!({"op": "meaning", "query": "how the daemon starts", "limit": 8})]
        );
        assert_eq!(
            leads,
            [
                MeaningHit {
                    path: "src/a.ts".into(),
                    start_line: 3,
                    end_line: 12,
                    symbol: Some("go".into()),
                    score: 0.5,
                    snippet: "function go() {".into(),
                },
                MeaningHit {
                    path: "src/b.ts".into(),
                    start_line: 7,
                    end_line: 9,
                    symbol: None,
                    score: 0.25,
                    snippet: String::new(),
                },
            ]
        );
        drop(daemon);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn relevance_answer_should_mark_only_a_french_word_the_repository_lacks_entirely() {
        let row = |content: usize, symbol: usize, filename: usize, via: Option<&str>| {
            pixel_proto::KeywordEvidence {
                keyword: "mot".into(),
                content_files: content,
                symbol_files: symbol,
                filename_files: filename,
                via_expansion: via.map(str::to_string),
                ..pixel_proto::KeywordEvidence::default()
            }
        };
        let marked = |french: bool, row: pixel_proto::KeywordEvidence| {
            let block = Relevance {
                files_considered: 100,
                keywords: vec![row],
                ..Relevance::default()
            };
            relevance_answer(&block, french).input.keywords[0].french_only
        };
        assert!(marked(true, row(0, 0, 0, None)), "absent French");
        assert!(!marked(false, row(0, 0, 0, None)), "absent English");
        assert!(
            !marked(true, row(0, 0, 0, Some("login"))),
            "found through a synonym"
        );
        assert!(!marked(true, row(2, 0, 0, None)), "in the text");
        assert!(!marked(true, row(0, 3, 0, None)), "in a symbol name");
        assert!(!marked(true, row(0, 0, 1, None)), "in a path");
    }

    #[test]
    fn relevance_answer_should_weigh_a_keyword_by_its_widest_channel() {
        let block = Relevance {
            files_considered: 100,
            keywords: vec![pixel_proto::KeywordEvidence {
                keyword: "daemon".into(),
                content_files: 1,
                symbol_files: 9,
                filename_files: 2,
                ..pixel_proto::KeywordEvidence::default()
            }],
            ..Relevance::default()
        };
        let weight = relevance_answer(&block, false).input.keywords[0].weight;
        assert!((weight - (101.0_f64 / 10.0).ln()).abs() < 1e-9, "{weight}");
        // A truncated probe is a lower bound: the daemon weighs the count it
        // has, and a word it calls common weighs nothing whatever its counts.
        let row = |truncated: bool, common: bool| Relevance {
            keywords: vec![pixel_proto::KeywordEvidence {
                truncated,
                common,
                content_files: 3,
                keyword: "daemon".into(),
                ..pixel_proto::KeywordEvidence::default()
            }],
            files_considered: 100,
            ..Relevance::default()
        };
        let weight_of = |block: &Relevance| relevance_answer(block, false).input.keywords[0].weight;
        assert!((weight_of(&row(true, false)) - (101.0_f64 / 4.0).ln()).abs() < 1e-9);
        assert!(weight_of(&row(false, true)).abs() < f64::EPSILON);
        assert!(weight_of(&row(true, true)).abs() < f64::EPSILON);
    }

    #[test]
    fn relevance_block_should_name_why_facts_cannot_answer() {
        assert_eq!(
            relevance_block(&json!({"status": "unavailable", "reason": "graph_stale"}))
                .unwrap_err(),
            "facts: graph_stale"
        );
        assert_eq!(
            relevance_block(&json!({"status": "unavailable"})).unwrap_err(),
            "facts are unavailable"
        );
        assert_eq!(
            relevance_block(&json!({"status": "available", "facts": {"targets": []}})).unwrap_err(),
            "facts carry no relevance"
        );
        assert!(
            relevance_block(&json!({"facts": {"relevance": {"files_considered": "x"}}}))
                .unwrap_err()
                .starts_with("facts.relevance: ")
        );
        let block = relevance_block(&json!({"facts": {"relevance": {
            "files_considered": 7, "graph": true
        }}}))
        .unwrap();
        assert_eq!((block.files_considered, block.graph), (7, true));
        let top = relevance_block(&json!({"relevance": {"files_considered": 3}})).unwrap();
        assert_eq!(top.files_considered, 3);
    }

    #[test]
    fn meaning_hits_should_turn_vectors_that_are_not_ready_into_an_error_with_the_reason() {
        assert_eq!(
            meaning_hits(&json!({"status": "unavailable", "reason": "cold", "caps": []}))
                .unwrap_err(),
            "meaning cold"
        );
        assert_eq!(
            meaning_hits(&json!({"status": "unavailable", "reason": "failed",
                "detail": "no model", "caps": []}))
            .unwrap_err(),
            "meaning failed: no model"
        );
        assert!(
            meaning_hits(&json!({"hits": 3}))
                .unwrap_err()
                .starts_with("meaning: unreadable reply: ")
        );
    }

    #[test]
    fn is_french_should_follow_the_task_tokenizers_language() {
        assert!(is_french(
            "pourquoi la connexion est très lente quand même ?"
        ));
        assert!(!is_french("why is the connection so slow"));
        assert!(!is_french(""));
    }

    #[test]
    fn local_route_should_refuse_meaning_and_relevance_without_a_current_index() {
        let root = scratch("local-prose");
        let live = Live::new(&root, Route::Local);
        let deadline = Instant::now() + WINDOW;
        assert_eq!(
            live.meaning("how the daemon starts", 8, deadline)
                .unwrap_err(),
            "meaning needs a running daemon"
        );
        assert_eq!(
            live.relevance("the daemon starts", deadline).unwrap_err(),
            "the text index does not cover HEAD"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
