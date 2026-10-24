// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The `Service` (`specs/ops.md`): the single entry point that turns an
//! [`Op`] into an [`Envelope`](pixel_proto::Envelope). The socket server and
//! the CLI's in-process fallback both call [`Service::handle`].
//!
//! One thread owns the `Service`: it is the only writer. It refreshes the
//! text index and the graph from watcher batches and publishes the new
//! generation; reads are answered from the last published one, by the writer
//! itself or by a [`Replica`] while the writer is busy.

mod reader;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant};

use pixel_graph::store::Store;
use pixel_index::TrigramExtractor;
use pixel_index::history::FactsStore;
use pixel_index::indexset::IndexSet;
use pixel_io::GitRunner;
use pixel_proto::{EditPhase, Envelope, Op, PixelError, SnapshotInfo};
use serde_json::{Value, json};
use xxhash_rust::xxh3::xxh3_64;

use crate::api::{Response, ServeError, failure_response};
use crate::edit::{self, Answer};
use crate::plane::plane;
use crate::serve::{ReadReplica, WatchReport};
use reader::{Basis, Outcome, Published, Reader, history_result};

pub use reader::{
    HISTORY_DEFAULT_ROWS, MAP_FILE_CAP, PAGE_ROWS, SEARCH_DEFAULT_ROWS, SEARCH_MAX_ROWS,
    SYMBOL_ROWS,
};

/// The widest HEAD move the text index catches up by refreshing the paths
/// that changed; a wider one leaves the covered HEAD where it was and the
/// freshness gate says `index_stale`.
pub const HEAD_CATCH_UP_MAX_PATHS: usize = 2_000;

/// Git's empty tree (SHA-1): the base a first commit is diffed from.
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// How long one graph refresh may spend extracting before the rest is left
/// for the next batch.
const GRAPH_REFRESH_BUDGET: Duration = Duration::from_secs(10);

/// How long a refresh waits for another process holding the graph's writer
/// lock.
const GRAPH_LOCK_WAIT: Duration = Duration::from_secs(5);

/// How long the cached HEAD, branch and dirty count are trusted.
const GIT_STATE_TTL: Duration = Duration::from_secs(1);

/// The wall-clock budget of the first history query's catch-up.
const HISTORY_CATCH_UP: Duration = Duration::from_millis(3_000);

/// State the writer and its replicas share.
pub(crate) struct Shared {
    pub(crate) root: PathBuf,
    pub(crate) meaning: crate::meaning::Meaning,
    pub(crate) index: RwLock<IndexSet>,
    pub(crate) published: RwLock<Published>,
}

/// The cached HEAD, branch and dirty paths of the work tree.
#[derive(Default)]
struct GitState {
    cached: Option<(Instant, Option<String>, Option<String>, usize)>,
}

impl GitState {
    fn clear(&mut self) {
        self.cached = None;
    }

    fn read(&mut self, root: &Path) -> (Option<String>, Option<String>, usize) {
        if let Some((at, head, branch, dirty)) = &self.cached
            && at.elapsed() < GIT_STATE_TTL
        {
            return (head.clone(), branch.clone(), *dirty);
        }
        let git = GitRunner::new(root);
        let head = git.rev_parse_head();
        let branch = git.current_branch();
        let dirty = git.status_porcelain().len();
        self.cached = Some((Instant::now(), head.clone(), branch.clone(), dirty));
        (head, branch, dirty)
    }
}

pub struct Service {
    shared: Arc<Shared>,
    started: Instant,
    generation: u64,
    git: GitState,
    /// The graph may lag the working tree: a batch arrived since the last
    /// refresh, or none has run.
    graph_dirty: bool,
    /// A refresh has succeeded in this process: from then on every batch
    /// refreshes the graph at once, so replicas never serve an older one.
    graph_ready: bool,
    /// A refresh failed with this message; reads say so instead of lying.
    graph_error: Option<String>,
    watch: Option<WatchReport>,
    watcher_errors: u64,
    history_warmer: Arc<AtomicBool>,
}

impl Service {
    /// Opens (building when needed) the text index of the work tree that
    /// contains `root`. The graph and the history are built on first use.
    pub fn open(root: &Path) -> Result<Service, ServeError> {
        Self::open_with(root, crate::meaning::default_opener)
    }

    /// [`Service::open`] with another way to open the embedder, for tests of
    /// the semantic pool that must not need a downloaded model.
    pub fn open_with(root: &Path, opener: crate::meaning::Opener) -> Result<Service, ServeError> {
        let root = pixel_graph::tree::root(root).unwrap_or_else(|_| root.to_path_buf());
        let index = IndexSet::open_or_build(&root, Box::new(TrigramExtractor))
            .map_err(|err| ServeError::Msg(err.to_string()))?;
        let shared = Arc::new(Shared {
            root,
            meaning: crate::meaning::Meaning::new(opener),
            index: RwLock::new(index),
            published: RwLock::new(Published::default()),
        });
        let mut service = Service {
            shared,
            started: Instant::now(),
            generation: 0,
            git: GitState::default(),
            graph_dirty: true,
            graph_ready: false,
            graph_error: None,
            watch: None,
            watcher_errors: 0,
            history_warmer: Arc::new(AtomicBool::new(false)),
        };
        service.publish();
        Ok(service)
    }

    pub fn root(&self) -> &Path {
        &self.shared.root
    }

    /// The constant answer to `ping`: who is serving, and which protocol.
    pub fn pong_value(&self) -> Value {
        json!({
            "pong": true,
            "version": env!("CARGO_PKG_VERSION"),
            "protocol": pixel_proto::ENVELOPE_PROTOCOL_VERSION,
            "root": self.shared.root.display().to_string(),
        })
    }

    /// The state the writer and its replicas share, for tests of what reads it.
    #[cfg(test)]
    pub(crate) fn shared(&self) -> Arc<Shared> {
        Arc::clone(&self.shared)
    }

    /// A replica that answers read ops from what this service last published.
    pub fn read_replica(&self) -> Replica {
        Replica {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Answers one op.
    pub fn handle(&mut self, op: Op) -> Response {
        let plane = plane(&op);
        let response = match &op {
            Op::Ping => Envelope::success("ping", self.pong_value()),
            Op::Shutdown => Envelope::success("shutdown", json!({"stopping": true})),
            Op::Status {} => self.status(),
            Op::Reindex {} => self.reindex(),
            Op::EditCheck {
                path,
                proposed,
                phase,
                session,
            } => self.edit_check(path, proposed.as_deref(), *phase, session),
            Op::StopCheck { session } => self.stop_check(session),
            Op::History {
                query,
                facet,
                limit,
                read_only: false,
            } => self.history(query, facet.as_deref(), *limit),
            Op::Search { .. }
            | Op::Symbol { .. }
            | Op::Callers { .. }
            | Op::Impact { .. }
            | Op::Changes { .. }
            | Op::Map { .. }
            | Op::Meaning { .. }
            | Op::Brief { .. }
            | Op::History { .. } => self.read(&op),
        };
        if plane.mutates_repo() {
            self.git.clear();
            self.publish();
        }
        response
    }

    /// The edit hooks' check of one file (`specs/hooks.md`). It reads the
    /// generation as it stands (a hook never builds one), changes only the
    /// session's memory, and is silent when anything it needs is missing.
    fn edit_check(
        &mut self,
        path: &str,
        proposed: Option<&str>,
        phase: EditPhase,
        session: &str,
    ) -> Response {
        let root = self.shared.root.clone();
        let mut state = edit::session::load(&root, session);
        let held = state.clone();
        let name = match phase {
            EditPhase::Before => "before",
            EditPhase::After => "after",
        };
        let answer = match Store::open(&root) {
            Ok(store) => match phase {
                EditPhase::Before => edit::before(&mut state, &store, &root, path, proposed),
                EditPhase::After => edit::after(&mut state, &store, &root, path),
            }
            .unwrap_or(Answer::Silent("error")),
            Err(_) => Answer::Silent("index_stale"),
        };
        if state != held {
            let _ = edit::session::save(&root, session, &state);
        }
        Envelope::success("edit_check", answer.to_value(name))
    }

    /// Stop: whether stale callers are still pending for the session.
    fn stop_check(&mut self, session: &str) -> Response {
        let root = self.shared.root.clone();
        let mut state = edit::session::load(&root, session);
        let held = state.clone();
        let answer = edit::stop(&mut state, &root);
        if state != held {
            let _ = edit::session::save(&root, session, &state);
        }
        Envelope::success("stop_check", answer.to_value("stop"))
    }

    /// A read op, after bringing the graph up to date when the op needs it.
    fn read(&mut self, op: &Op) -> Response {
        if needs_graph(op) {
            self.ensure_graph();
        }
        let published = self
            .shared
            .published
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let reader = Reader {
            shared: &self.shared,
            root: &self.shared.root,
            index: &self.shared.index,
            published: &published,
            behind: None,
        };
        reader.answer(op).unwrap_or_else(|| {
            let why = self
                .graph_error
                .clone()
                .unwrap_or_else(|| "the graph is not built yet".to_owned());
            failure_response(op.op_name(), format!("INDEX_BUILDING: {why}"))
        })
    }

    /// Refreshes the graph when it lags the tree.
    fn ensure_graph(&mut self) {
        if !self.graph_dirty {
            return;
        }
        let deadline = Instant::now() + GRAPH_REFRESH_BUDGET;
        match pixel_graph::refresh(&self.shared.root, Some(GRAPH_LOCK_WAIT), Some(deadline)) {
            Ok(_) => {
                self.graph_error = None;
                self.graph_dirty = false;
                self.graph_ready = true;
            }
            Err(err) => {
                self.graph_error = Some(err.to_string());
            }
        }
        self.publish();
    }

    /// Publishes the repository's state for the replicas and the envelopes
    /// they build: the next generation.
    fn publish(&mut self) {
        self.generation += 1;
        let (head, branch, dirty) = self.git.read(&self.shared.root);
        let token = format!(
            "{:012x}",
            xxh3_64(format!("{head:?}|{}|{dirty}", self.generation).as_bytes()) & 0xffff_ffff_ffff
        );
        let snapshot = SnapshotInfo {
            token: Some(token),
            head,
            branch,
            dirty: Vec::new(),
            dirty_count: Some(dirty as u64),
        };
        *self
            .shared
            .published
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Published {
            snapshot,
            generation: self.generation,
            graph_fresh: !self.graph_dirty,
        };
    }

    fn status(&mut self) -> Response {
        let root = self.shared.root.clone();
        let index = self
            .shared
            .index
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .status();
        let graph = match Store::open(&root) {
            Ok(store) => {
                use pixel_model::Graph;
                match store.provenance() {
                    Ok(p) => json!({
                        "present": true,
                        "commit": p.commit,
                        "dirty": p.dirty,
                        "generation": p.generation,
                        "extractor": p.extractor,
                        "indexed_at": p.indexed_at,
                        "files": store.file_rows().map(|rows| rows.len()).ok(),
                        "stale": self.graph_dirty,
                    }),
                    Err(err) => json!({"present": false, "error": err.to_string()}),
                }
            }
            Err(err) => json!({"present": false, "error": err.to_string()}),
        };
        let history = match FactsStore::open_existing(&root) {
            Ok(Some(_)) => json!({"present": true}),
            Ok(None) => json!({"present": false}),
            Err(err) => json!({"present": false, "error": err.to_string()}),
        };
        let published = self
            .shared
            .published
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        Envelope::success(
            "status",
            json!({
                "version": env!("CARGO_PKG_VERSION"),
                "root": root.display().to_string(),
                "generation": published.generation,
                "uptime_s": self.started.elapsed().as_secs(),
                "index": {
                    "commit": index.commit_oid,
                    "covered_head": index.covered_head,
                    "base_files": index.base_files,
                    "delta_files": index.delta_files,
                    "overlay_files": index.overlay_files,
                    "tombstones": index.tombstones,
                },
                "graph": graph,
                "history": history,
                "watcher": {
                    "ready": self.watch.is_some(),
                    "watched_dirs": self.watch.map(|w| w.dirs),
                    "recursive": self.watch.map(|w| w.recursive),
                    "registration_ms": self.watch.map(|w| w.elapsed.as_millis() as u64),
                    "git_dir": self.watch.map(|w| w.git_dir),
                    "errors": self.watcher_errors,
                },
            }),
        )
        .with_snapshot(published.snapshot.compact())
    }

    /// Rebuilds the text index and the graph.
    fn reindex(&mut self) -> Response {
        match IndexSet::open_or_build_bypass_cache(&self.shared.root, Box::new(TrigramExtractor)) {
            Ok(fresh) => {
                *self
                    .shared
                    .index
                    .write()
                    .unwrap_or_else(PoisonError::into_inner) = fresh;
            }
            Err(err) => return failure_response("reindex", err.to_string()),
        }
        self.graph_dirty = true;
        self.ensure_graph();
        match &self.graph_error {
            Some(message) => failure_response("reindex", message.clone()),
            None => Envelope::success("reindex", json!({"reindexed": true})),
        }
    }

    /// A history query that may ingest: a bounded catch-up, then the keep-
    /// fresh loop in the background (spawned once).
    fn history(&mut self, query: &str, facet: Option<&str>, limit: Option<usize>) -> Response {
        let root = self.shared.root.clone();
        let mut store = match FactsStore::open(&root) {
            Ok(store) => store,
            Err(err) => return failure_response("history", err.to_string()),
        };
        let _ = pixel_index::history::ingest::ingest_until_fresh_bounded(
            &mut store,
            HISTORY_CATCH_UP.as_millis() as u64,
        );
        if !self.history_warmer.swap(true, Ordering::AcqRel) {
            spawn_history_warmer(root);
        }
        self.wrap(
            "history",
            match history_result(&store, query, facet, limit) {
                Outcome::Done(value, basis) => Ok((value, basis)),
                Outcome::Fail(message) => Err(message),
                Outcome::Ambiguous(message, _) => Err(message),
                Outcome::NoGraph => Err("history unavailable".to_owned()),
            },
        )
    }

    fn wrap(&self, op: &str, outcome: Result<(Value, Basis), String>) -> Response {
        match outcome {
            Ok((value, _basis)) => {
                let published = self
                    .shared
                    .published
                    .read()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone();
                Envelope::success(op, value).with_snapshot(published.snapshot.compact())
            }
            Err(message) => failure_response(op, message),
        }
    }

    // --- what the transport tells the service ---

    /// Re-reads `rel` into the text index; the graph catches up lazily.
    pub(crate) fn refresh_file(&mut self, rel: &str) {
        self.refresh_files(&[(rel, false)]);
    }

    /// Drops `rel` from the text index.
    pub(crate) fn remove_file(&mut self, rel: &str) {
        self.refresh_files(&[(rel, true)]);
    }

    /// A debounced watcher batch: `(path, removed)` pairs, relative to the
    /// root.
    pub(crate) fn refresh_files(&mut self, files: &[(&str, bool)]) {
        self.shared
            .index
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .refresh_files(files);
        self.graph_dirty = true;
        self.git.clear();
        if self.graph_ready {
            self.ensure_graph();
        } else {
            self.publish();
        }
    }

    /// The HEAD the text index was opened at and the paths its overlay
    /// holds: what a watch registered late may have missed.
    pub(crate) fn index_catch_up(&self) -> (Option<String>, std::collections::BTreeSet<String>) {
        let index = self
            .shared
            .index
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        (
            index.opened_head().map(str::to_owned),
            index.overlay_paths(),
        )
    }

    /// Brings the text index's content to HEAD after a ref move: refreshes
    /// every path that changed between the HEAD it covers and HEAD now.
    /// Whether the index covers HEAD afterwards; a failed diff or a move
    /// wider than [`HEAD_CATCH_UP_MAX_PATHS`] leaves it where it was.
    pub(crate) fn catch_up_head(&mut self) -> bool {
        self.catch_up_head_within(HEAD_CATCH_UP_MAX_PATHS)
    }

    pub(crate) fn catch_up_head_within(&mut self, max_paths: usize) -> bool {
        let root = self.shared.root.clone();
        let git = GitRunner::new(&root);
        let Some(head) = git.rev_parse_head() else {
            return false;
        };
        let covered = self
            .shared
            .index
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .covered_head()
            .map(str::to_owned);
        if covered.as_deref() == Some(head.as_str()) {
            return true;
        }
        let from = covered.unwrap_or_else(|| EMPTY_TREE.to_owned());
        let Ok(changed) = git.diff_name_status_or_err(&from, &head) else {
            return false;
        };
        if changed.len() > max_paths {
            return false;
        }
        let files: Vec<(&str, bool)> = changed
            .iter()
            .map(|(_status, path)| (path.as_str(), !root.join(path).exists()))
            .collect();
        self.refresh_files(&files);
        self.shared
            .index
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .note_caught_up(&head);
        true
    }

    /// The HEAD the text index covers, for the watch tests of `serve`.
    #[cfg(test)]
    pub(crate) fn index_covered_head(&self) -> Option<String> {
        self.shared
            .index
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .covered_head()
            .map(str::to_owned)
    }

    pub(crate) fn note_watch_ready(&mut self, report: WatchReport) {
        self.watch = Some(report);
    }

    /// Brings the graph up to the working tree now, without waiting for a
    /// graph read to ask. A daemon that only refreshed on a graph op answered
    /// every first brief from a graph it had not checked, and said so ("some
    /// daemon evidence predates the last edit"). From here on every batch
    /// refreshes it (`graph_ready`).
    pub(crate) fn warm_graph(&mut self) {
        self.graph_dirty = true;
        self.ensure_graph();
    }

    /// HEAD or a branch moved: the cached git state describes another commit.
    pub(crate) fn note_git_moved(&mut self) {
        self.git.clear();
        self.graph_dirty = true;
        if self.graph_ready {
            self.ensure_graph();
        } else {
            self.publish();
        }
    }

    pub(crate) fn note_watcher_error(&mut self, error: &str) {
        self.watcher_errors += 1;
        eprintln!("pixel daemon: watcher error: {error}");
    }
}

/// Whether the op reads the graph (as opposed to the text index or history).
fn needs_graph(op: &Op) -> bool {
    matches!(
        op,
        Op::Symbol { .. }
            | Op::Callers { .. }
            | Op::Impact { .. }
            | Op::Changes { .. }
            | Op::Map { .. }
    )
}

/// The detached background thread that keeps `history.db` fresh once a
/// history query has asked for it.
#[cfg_attr(test, mutants::skip)] // detached endless loop; ingest_tick is tested
fn spawn_history_warmer(root: PathBuf) {
    use pixel_index::history::ingest::{IngestOptions, ingest_tick};
    std::thread::spawn(move || {
        let Ok(mut store) = FactsStore::open(&root) else {
            return;
        };
        let options = IngestOptions::default();
        let mut ticks = 0u64;
        loop {
            match ingest_tick(&mut store, &options) {
                Ok(report) if report.fresh => {
                    let _ = store.wal_checkpoint();
                    std::thread::sleep(Duration::from_secs(5));
                }
                Ok(_) => {
                    ticks += 1;
                    if ticks.is_multiple_of(20) {
                        let _ = store.wal_checkpoint();
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
                Err(_) => std::thread::sleep(Duration::from_secs(2)),
            }
        }
    });
}

/// A copy of the service that answers read ops on its own thread from what
/// the writer last published.
pub struct Replica {
    shared: Arc<Shared>,
}

impl ReadReplica for Replica {
    fn read(&mut self, op: &Op, behind: &dyn Fn() -> Option<Duration>) -> Option<Response> {
        let published = self
            .shared
            .published
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let reader = Reader {
            shared: &self.shared,
            root: &self.shared.root,
            index: &self.shared.index,
            published: &published,
            behind: behind(),
        };
        reader.answer(op)
    }
}

/// A failure envelope for an op that cannot be answered.
pub fn unavailable(op: &str, message: impl Into<String>) -> Response {
    Envelope::failure(
        op,
        PixelError::new(pixel_proto::ErrorCode::UnsupportedState, message),
    )
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stdout(std::process::Stdio::null())
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?}");
    }

    /// A crate where `b::go` calls `a::run` and `tests/it.rs` calls `go`.
    fn project() -> (tempfile::TempDir, Service) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]);
        for (path, text) in [
            (
                "Cargo.toml",
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub mod a;\npub mod b;\n"),
            ("src/a.rs", "pub fn run(x: u32) -> u32 {\n    x\n}\n"),
            (
                "src/b.rs",
                "use crate::a::run;\n\npub fn go() -> u32 {\n    run(1)\n}\n",
            ),
            (
                "tests/it.rs",
                "use demo::b::go;\n\nfn t() {\n    go();\n}\n",
            ),
        ] {
            let full = root.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, text).unwrap();
        }
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "one"]);
        let service = Service::open(root).expect("service");
        (dir, service)
    }

    fn ask(service: &mut Service, json: &str) -> Response {
        service.handle(serde_json::from_str(json).expect("an op"))
    }

    fn data(response: &Response) -> &Value {
        assert!(response.ok, "{:?}", response.error);
        response.result.as_ref().expect("a result")
    }

    #[test]
    fn a_search_answers_from_the_text_index_with_a_snapshot_and_an_open_world() {
        let (_dir, mut service) = project();
        let response = ask(&mut service, r#"{"op":"search","pattern":"fn run"}"#);
        let rows = data(&response)["matches"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["path"], "src/a.rs");
        assert_eq!(rows[0]["line"], 1);
        let epistemics = response.epistemics.as_ref().unwrap();
        assert_eq!(
            (epistemics.closed_world, epistemics.basis.as_str()),
            (false, "index")
        );
        let snapshot = response.snapshot.as_ref().unwrap();
        assert_eq!(snapshot.token.as_deref().map(str::len), Some(12));
        assert_eq!(snapshot.dirty_count, Some(0));
    }

    #[test]
    fn the_graph_ops_build_the_graph_on_first_use_and_name_callers_with_tiers() {
        let (_dir, mut service) = project();
        let symbol = ask(&mut service, r#"{"op":"symbol","name":"run"}"#);
        assert_eq!(data(&symbol)["symbols"][0]["file"], "src/a.rs");
        let callers = ask(
            &mut service,
            r#"{"op":"callers","uid_or_name":"run","role":"callers"}"#,
        );
        let items = data(&callers)["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(
            (items[0]["name"].as_str(), items[0]["tier"].as_str()),
            (Some("go"), Some("proved"))
        );
        let callees = ask(
            &mut service,
            r#"{"op":"callers","uid_or_name":"go","role":"callees"}"#,
        );
        assert_eq!(data(&callees)["items"][0]["name"], "run");
        let impact = ask(
            &mut service,
            r#"{"op":"impact","uid_or_name":"run","direction":"upstream"}"#,
        );
        let impact = data(&impact);
        assert_eq!(impact["counts"], json!([1, 1, 0]));
        assert_eq!(impact["tests"], json!(["tests/it.rs"]));
        let epistemics = ask(&mut service, r#"{"op":"symbol","name":"run"}"#)
            .epistemics
            .unwrap();
        assert_eq!(epistemics.basis, "graph");
        assert!(epistemics.lower_bound && !epistemics.closed_world);
        assert!(!epistemics.extraction_limits.is_empty());
    }

    #[test]
    fn a_name_that_is_nobody_is_not_found_and_a_name_twice_is_ambiguous() {
        let (dir, mut service) = project();
        let missing = ask(&mut service, r#"{"op":"symbol","name":"nope"}"#);
        assert!(!missing.ok);
        assert_eq!(
            missing.error.unwrap().code,
            pixel_proto::ErrorCode::NotFound
        );
        std::fs::write(dir.path().join("src/c.rs"), "pub fn run() {}\n").unwrap();
        service.refresh_files(&[("src/c.rs", false)]);
        let twice = ask(
            &mut service,
            r#"{"op":"callers","uid_or_name":"run","role":"callers"}"#,
        );
        assert!(!twice.ok);
        let error = twice.error.unwrap();
        assert_eq!(error.code, pixel_proto::ErrorCode::Ambiguous);
        assert_eq!(
            error.ambiguity.unwrap()["candidates"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn changes_map_the_working_tree_onto_symbols() {
        let (dir, mut service) = project();
        std::fs::write(
            dir.path().join("src/a.rs"),
            "pub fn run(x: u32, y: u32) -> u32 {\n    x + y\n}\n",
        )
        .unwrap();
        service.refresh_files(&[("src/a.rs", false)]);
        let response = ask(&mut service, r#"{"op":"changes","include_tests":true}"#);
        let symbols = data(&response)["symbols"].as_array().unwrap();
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0]["name"], "run");
        assert_eq!(symbols[0]["signature_changed"], true);
        assert_eq!(symbols[0]["tests"], json!(["tests/it.rs"]));
        let map = ask(&mut service, r#"{"op":"map","markdown":true}"#);
        assert!(
            data(&map)["markdown"]
                .as_str()
                .unwrap()
                .contains("## src/a.rs")
        );
    }

    #[test]
    fn two_batches_in_a_row_both_publish_a_generation_and_a_snapshot() {
        let (dir, mut service) = project();
        let _ = ask(&mut service, r#"{"op":"symbol","name":"run"}"#);
        let before = service.shared.published.read().unwrap().clone();
        std::fs::write(dir.path().join("src/d.rs"), "pub fn d() {}\n").unwrap();
        service.refresh_files(&[("src/d.rs", false)]);
        let first = service.shared.published.read().unwrap().clone();
        std::fs::write(dir.path().join("src/e.rs"), "pub fn e() {}\n").unwrap();
        service.refresh_files(&[("src/e.rs", false)]);
        let second = service.shared.published.read().unwrap().clone();
        assert!(first.generation > before.generation);
        assert!(second.generation > first.generation);
        assert_eq!(first.snapshot.dirty_count, Some(1));
        assert_eq!(second.snapshot.dirty_count, Some(2));
        assert_ne!(first.snapshot.token, second.snapshot.token);
        assert!(
            first.graph_fresh && second.graph_fresh,
            "the graph follows each batch"
        );
        let found = ask(&mut service, r#"{"op":"symbol","name":"e"}"#);
        assert_eq!(data(&found)["symbols"][0]["file"], "src/e.rs");
    }

    #[test]
    fn warming_the_graph_publishes_a_fresh_one_before_any_graph_read_asks() {
        let (_dir, mut service) = project();
        assert!(!service.shared().published.read().unwrap().graph_fresh);
        let before = ask(&mut service, r#"{"op":"status"}"#);
        assert_eq!(data(&before)["graph"]["present"], false);
        service.warm_graph();
        let after = ask(&mut service, r#"{"op":"status"}"#);
        assert_eq!(data(&after)["graph"]["present"], true);
        assert_eq!(data(&after)["graph"]["stale"], false);
        assert!(service.shared().published.read().unwrap().graph_fresh);
        assert!(
            service.graph_ready,
            "every later batch refreshes the graph at once"
        );
    }

    #[test]
    fn a_replica_serves_only_a_fresh_graph_or_a_stale_ok_read() {
        let (_dir, mut service) = project();
        let mut replica = service.read_replica();
        let symbol = serde_json::from_str::<Op>(r#"{"op":"symbol","name":"run"}"#).unwrap();
        let stale =
            serde_json::from_str::<Op>(r#"{"op":"symbol","name":"run","stale_ok":true}"#).unwrap();
        // Nothing is built yet: the writer is to build and answer.
        assert!(replica.read(&symbol, &|| None).is_none());
        let _ = service.handle(symbol.clone());
        let fresh = replica
            .read(&symbol, &|| None)
            .expect("a fresh graph is served");
        assert!(fresh.ok && fresh.warnings.is_empty());
        // A batch the writer has not finished: only `stale_ok` is answered,
        // and it says how far behind.
        service.graph_dirty = true;
        service.publish();
        assert!(replica.read(&symbol, &|| None).is_none());
        let behind = replica
            .read(&stale, &|| Some(Duration::from_millis(40)))
            .expect("a stale_ok read is served");
        assert_eq!(behind.epistemics.unwrap().staleness_ms, Some(40));
        assert_eq!(behind.warnings[0].code, "STALE_GENERATION");
    }

    #[test]
    fn history_is_built_by_the_first_query_and_never_by_a_read_only_one() {
        let (dir, mut service) = project();
        let read_only = ask(
            &mut service,
            r#"{"op":"history","query":"one","read_only":true}"#,
        );
        assert_eq!(data(&read_only)["available"], false);
        assert!(!dir.path().join(".pixel/history.db").exists());
        let first = ask(&mut service, r#"{"op":"history","query":"one"}"#);
        assert_eq!(data(&first)["available"], true);
        assert!(dir.path().join(".pixel/history.db").exists());
        let again = ask(
            &mut service,
            r#"{"op":"history","query":"one","read_only":true}"#,
        );
        assert_eq!(data(&again)["available"], true);
    }

    #[test]
    fn meaning_without_a_model_is_unavailable_with_its_reason() {
        let (_dir, mut service) = project();
        let meaning = ask(&mut service, r#"{"op":"meaning","query":"q"}"#);
        let answer = data(&meaning);
        assert_eq!(answer["status"], "unavailable");
        if !cfg!(feature = "semantic") {
            assert_eq!(answer["reason"], "failed");
            assert!(
                answer["detail"]
                    .as_str()
                    .unwrap()
                    .contains("no semantic search")
            );
        }
    }

    #[test]
    fn the_edit_ops_should_be_silent_until_a_graph_exists() {
        let (_dir, mut service) = project();
        let response = ask(
            &mut service,
            r#"{"op":"edit_check","path":"src/a.rs","proposed":"pub fn run() {}\n","session":"s"}"#,
        );
        let value = data(&response);
        assert_eq!(value["text"], Value::Null);
        assert_eq!(value["silent"], "index_stale");
        let stop = ask(&mut service, r#"{"op":"stop_check","session":"s"}"#);
        assert_eq!(data(&stop)["silent"], "empty");
    }

    #[test]
    fn an_edit_should_card_then_go_stale_then_block_stop_once() {
        let (dir, mut service) = project();
        let _ = ask(&mut service, r#"{"op":"symbol","name":"run"}"#);
        let card = ask(
            &mut service,
            r#"{"op":"edit_check","path":"src/a.rs","proposed":"pub fn run(x: u32, y: u32) -> u32 {\n    x + y\n}\n","session":"s"}"#,
        );
        let card = data(&card);
        assert!(
            card["text"]
                .as_str()
                .unwrap()
                .starts_with("[PIXEL:EDIT] `run`"),
            "{card}"
        );
        assert_eq!(card["phase"], "before");
        assert_eq!(card["count"], 1);

        std::fs::write(
            dir.path().join("src/a.rs"),
            "pub fn run(x: u32, y: u32) -> u32 {\n    x + y\n}\n",
        )
        .unwrap();
        let stale = ask(
            &mut service,
            r#"{"op":"edit_check","path":"src/a.rs","phase":"after","session":"s"}"#,
        );
        let stale = data(&stale);
        assert!(
            stale["text"]
                .as_str()
                .unwrap()
                .starts_with("[PIXEL:STALE-CALLERS]"),
            "{stale}"
        );
        assert_eq!(stale["phase"], "after");

        let block = ask(&mut service, r#"{"op":"stop_check","session":"s"}"#);
        let block = data(&block);
        assert!(
            block["text"].as_str().unwrap().contains("src/b.rs:4"),
            "{block}"
        );
        let again = ask(&mut service, r#"{"op":"stop_check","session":"s"}"#);
        assert_eq!(data(&again)["silent"], "already_blocked");
        let other = ask(&mut service, r#"{"op":"stop_check","session":"another"}"#);
        assert_eq!(data(&other)["silent"], "empty");
    }

    #[test]
    fn a_brief_over_a_built_graph_names_the_definition_and_its_callers() {
        let (dir, mut service) = project();
        let _ = ask(&mut service, r#"{"op":"symbol","name":"run"}"#);
        let response = ask(
            &mut service,
            r#"{"op":"brief","prompt":"who calls `run` and what breaks if I rename it","session":"s1","host":"claude"}"#,
        );
        assert!(response.ok, "{response:?}");
        let value = data(&response);
        let text = value["text"].as_str().expect("a block");
        assert!(text.starts_with("[PIXEL:BRIEF]\n"), "{text}");
        assert!(
            text.contains("### src/a.rs:1-3 — run definition (function)\n1| pub fn run"),
            "{text}"
        );
        assert!(
            text.contains("### src/b.rs:1-5 — run usage in go"),
            "{text}"
        );
        // The callers' own callers are shown too: the test that calls `go`.
        assert!(
            text.contains("### tests/it.rs:3-5 — t calls go\n3| fn t() {"),
            "{text}"
        );
        assert_eq!(
            value["paths"],
            json!(["src/a.rs", "src/b.rs", "tests/it.rs"])
        );
        assert_eq!(value["declined"], Value::Null);
        assert_eq!(value["record"]["t"], "brief");
        assert_eq!(value["record"]["route"], "daemon");
        drop(dir);
    }

    /// `handle` -> `start_session` -> `open_session`, which returns a `Session`.
    fn flow_project() -> (tempfile::TempDir, Service) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]);
        for (path, text) in [
            (
                "Cargo.toml",
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
            ),
            (
                "src/lib.rs",
                "pub mod auth;\npub mod server;\npub mod session;\n",
            ),
            (
                "src/session.rs",
                "pub struct Session {\n    pub id: u32,\n}\n\npub fn open_session(id: u32) -> Session {\n    Session { id }\n}\n",
            ),
            (
                "src/auth.rs",
                "use crate::session::{Session, open_session};\n\npub fn start_session(user: u32) -> Session {\n    open_session(user)\n}\n",
            ),
            (
                "src/server.rs",
                "use crate::auth::start_session;\n\npub fn handle(user: u32) -> u32 {\n    let session = start_session(user);\n    session.id\n}\n",
            ),
        ] {
            let full = root.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, text).unwrap();
        }
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "fixture"]);
        let service = Service::open(root).expect("the service opens");
        (dir, service)
    }

    #[test]
    fn a_trace_question_gets_the_call_chain_to_the_constructor_of_the_type() {
        let (_dir, mut service) = flow_project();
        let _ = ask(&mut service, r#"{"op":"symbol","name":"handle"}"#);
        let prompt =
            "trace how a request becomes a Session, from handle through to the constructor";
        let on = ask(
            &mut service,
            &json!({"op": "brief", "prompt": prompt, "session": "s", "host": "claude"}).to_string(),
        );
        let on = data(&on);
        let text = on["text"]
            .as_str()
            .unwrap_or_else(|| panic!("a block: {on}"));
        assert!(
            text.contains(
                "### call trace, entry to target\nchain handle (src/server.rs:3) -> start_session (src/auth.rs:3) -> open_session (src/session.rs:5)"
            ),
            "{text}"
        );
        assert!(
            text.contains("### src/auth.rs:3-5 — start_session calls open_session"),
            "{text}"
        );
        let off = ask(
            &mut service,
            &json!({
                "op": "brief", "prompt": prompt, "session": "t", "host": "claude",
                "without": ["cand-j-chain"],
            })
            .to_string(),
        );
        let off = data(&off);
        let text = off["text"].as_str().unwrap_or_default();
        assert!(!text.contains("call trace, entry to target"), "{text}");
    }

    #[test]
    fn a_history_question_gets_the_commit_it_quotes_with_its_files_and_patch() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/client.rs"), "pub fn send() {}\n").unwrap();
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "initial import"]);
        std::fs::write(
            root.join("src/client.rs"),
            "pub fn send() {}\n\npub fn backoff() {}\n",
        )
        .unwrap();
        git(root, &["add", "-A"]);
        git(
            root,
            &[
                "commit",
                "-qm",
                "add retry backoff to the client\n\nRetries wait longer each time.",
            ],
        );
        let mut service = Service::open(root).expect("the service opens");
        let _ = ask(&mut service, r#"{"op":"symbol","name":"send"}"#);
        let prompt =
            r#"which commit "add retry backoff to the client" and which files did it change"#;
        let on = ask(
            &mut service,
            &json!({"op": "brief", "prompt": prompt, "session": "s", "host": "claude"}).to_string(),
        );
        let on = data(&on);
        let text = on["text"]
            .as_str()
            .unwrap_or_else(|| panic!("a block: {on}"));
        assert!(text.contains("— message, files and patch\n"), "{text}");
        assert!(text.contains("add retry backoff to the client"), "{text}");
        assert!(
            text.contains("files in the commit:\n src/client.rs | 2 ++"),
            "{text}"
        );
        assert!(text.contains("@@ src/client.rs\n"), "{text}");
        assert!(text.contains("+pub fn backoff() {}"), "{text}");
        let off = ask(
            &mut service,
            &json!({
                "op": "brief", "prompt": prompt, "session": "t", "host": "claude",
                "without": ["cand-k-commit-patch", "history-fallback"],
            })
            .to_string(),
        );
        let off = data(&off);
        let text = off["text"].as_str().unwrap_or_default();
        assert!(!text.contains("message, files and patch"), "{text}");
    }

    /// A two-member Cargo workspace.
    fn workspace() -> (tempfile::TempDir, Service) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]);
        for (path, text) in [
            ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
            (
                "README.md",
                "# Demo\n\nDemo parses and formats the crates of a workspace.\n",
            ),
            (
                "crates/alpha/Cargo.toml",
                "[package]\nname = \"alpha\"\ndescription = \"Alpha parses\"\n",
            ),
            ("crates/alpha/src/lib.rs", "pub fn parse() {}\n"),
            (
                "crates/beta/Cargo.toml",
                "[package]\nname = \"beta\"\ndescription = \"Beta formats\"\n",
            ),
            ("crates/beta/src/lib.rs", "pub fn format() {}\n"),
        ] {
            let full = root.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, text).unwrap();
        }
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "fixture"]);
        let service = Service::open(root).expect("the service opens");
        (dir, service)
    }

    #[test]
    fn a_workspace_question_gets_the_member_lines_unless_the_step_is_switched_off() {
        let (_dir, mut service) = workspace();
        let _ = ask(&mut service, r#"{"op":"symbol","name":"parse"}"#);
        let prompt = "which crates make up the workspace and what does each one do";
        let on = ask(
            &mut service,
            &json!({"op": "brief", "prompt": prompt, "session": "s", "host": "claude"}).to_string(),
        );
        let on = data(&on);
        let text = on["text"]
            .as_str()
            .unwrap_or_else(|| panic!("a block: {on}"));
        assert!(
            text.contains(
                "### workspace — 2 members (cargo)\ncrates/alpha — Alpha parses\ncrates/beta — Beta formats"
            ),
            "{text}"
        );
        assert!(
            text.contains("### README — lead\nDemo parses and formats"),
            "{text}"
        );

        let off = ask(
            &mut service,
            &json!({
                "op": "brief", "prompt": prompt, "session": "t", "host": "claude",
                "without": ["workspace-overview"],
            })
            .to_string(),
        );
        let off = data(&off);
        let text = off["text"].as_str().unwrap_or_default();
        assert!(!text.contains("workspace — 2 members"), "{text}");
    }

    /// A crate with a matcher module, its importer and a guide.
    fn guided() -> (tempfile::TempDir, Service) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]);
        for (path, text) in [
            (
                "Cargo.toml",
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub mod smart_matcher;\n"),
            (
                "src/smart_matcher.rs",
                "//! Matching.\n\npub struct SmartCase {\n    pub sensitive: bool,\n}\n\nimpl SmartCase {\n    pub fn decide(&self, pattern: &str) -> bool {\n        pattern.chars().any(char::is_uppercase)\n    }\n}\n",
            ),
            (
                "src/main.rs",
                "use demo::smart_matcher::SmartCase;\n\nfn main() {\n    let _ = SmartCase { sensitive: true };\n}\n",
            ),
            (
                "docs/guide.md",
                "# Guide\n\n## Smart case matching\n\nSmart case matching makes a search case sensitive only when the pattern holds an uppercase letter.\nIt is on by default.\n\n## Colors\n\nColors are set with flags.\n",
            ),
        ] {
            let full = root.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, text).unwrap();
        }
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "fixture"]);
        let service = Service::open(root).expect("the service opens");
        (dir, service)
    }

    fn brief_text(service: &mut Service, prompt: &str, without: &[&str], session: &str) -> String {
        let response = ask(
            service,
            &json!({
                "op": "brief", "prompt": prompt, "session": session, "host": "claude",
                "without": without,
            })
            .to_string(),
        );
        data(&response)["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn a_question_about_a_module_gets_its_source_and_the_lines_that_import_it_unless_the_step_is_off()
     {
        let (_dir, mut service) = guided();
        let _ = ask(&mut service, r#"{"op":"symbol","name":"decide"}"#);
        let prompt = "which module part handles smart matcher decisions and what depends on it";
        let on = brief_text(&mut service, prompt, &[], "s1");
        // The importer main.rs is already shown as a usage block, so only the
        // declaration in lib.rs is added.
        assert!(
            on.contains(
                "### src/lib.rs:1-1 — source reference to smart_matcher\n1| pub mod smart_matcher;"
            ),
            "{on}"
        );
        assert!(!on.contains("src/main.rs:1-1 — source reference"), "{on}");
        let off = brief_text(&mut service, prompt, &["name-files"], "s2");
        assert!(!off.contains("source reference to"), "{off}");
    }

    #[test]
    fn an_explanatory_question_gets_the_documentation_section_unless_the_step_is_off() {
        let (_dir, mut service) = guided();
        let _ = ask(&mut service, r#"{"op":"symbol","name":"decide"}"#);
        let prompt = "why does smart case matching stay case sensitive by default";
        let on = brief_text(&mut service, prompt, &[], "s1");
        assert!(
            on.contains("### docs/guide.md:3-7 — Smart case matching (documentation section)"),
            "{on}"
        );
        let off = brief_text(&mut service, prompt, &["doc-sections"], "s2");
        assert!(!off.contains("documentation section"), "{off}");
    }

    #[test]
    fn a_brief_for_a_prompt_that_asks_nothing_about_code_is_declined_with_a_reason() {
        let (_dir, mut service) = project();
        let response = ask(
            &mut service,
            r#"{"op":"brief","prompt":"thanks, that works","session":"s1","host":"claude"}"#,
        );
        assert!(response.ok);
        let value = data(&response);
        assert_eq!(value["text"], Value::Null);
        assert_eq!(value["declined"], "not_about_code");
        assert_eq!(value["record"]["gate"], "declined");
    }

    #[test]
    fn status_reports_the_index_the_graph_and_the_generation() {
        let (_dir, mut service) = project();
        let before = ask(&mut service, r#"{"op":"status"}"#);
        assert_eq!(data(&before)["graph"]["present"], false);
        assert_eq!(data(&before)["history"]["present"], false);
        let _ = ask(&mut service, r#"{"op":"symbol","name":"run"}"#);
        let after = ask(&mut service, r#"{"op":"status"}"#);
        let after = data(&after);
        assert_eq!(after["graph"]["present"], true);
        assert_eq!(after["graph"]["files"], 5, "four sources and the manifest");
        assert_eq!(after["index"]["base_files"], 5);
        assert!(after["generation"].as_u64().unwrap() >= 2);
    }

    #[test]
    fn a_reindex_rebuilds_and_ping_answers_who_is_serving() {
        let (_dir, mut service) = project();
        assert!(
            data(&ask(&mut service, r#"{"op":"reindex"}"#))["reindexed"]
                .as_bool()
                .unwrap()
        );
        let pong = ask(&mut service, r#"{"op":"ping"}"#);
        assert_eq!(data(&pong)["pong"], true);
        assert_eq!(data(&pong)["version"], env!("CARGO_PKG_VERSION"));
    }
}
