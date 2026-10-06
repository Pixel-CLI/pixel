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
use pixel_graph::GraphStore;
use pixel_graph::build::{
    EXTRACTOR_VERSION, EXTRACTOR_VERSION_KEY, FRESHNESS_KEY, freshness_signature_trusting_stat,
};
use pixel_index::delta::{DeltaState, delta_shard_path};
use pixel_index::indexset::IndexSet;
use pixel_index::shard::Shard;
use pixel_index::{GramExtractor, TrigramExtractor, gitsync};
use serde_json::Value;

use super::chain::{
    CONCEPT_ROWS, CallerHit, Evidence, FileHit, Found, SEARCH_ROWS, SYMBOL_ROWS, SymbolHit,
};

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

    fn local_files(&self, anchor: &str) -> Result<Found, String> {
        if !index_current(&self.root) {
            return Err("the text index does not cover HEAD".into());
        }
        let set = IndexSet::open_or_build(&self.root, Box::new(TrigramExtractor))
            .map_err(|error| error.to_string())?;
        let (rows, stats) = set
            .search_page_filtered(&regex::escape(anchor), 0, Some(SEARCH_ROWS), None, None)
            .map_err(|error| error.to_string())?;
        Ok(Found {
            hits: rows
                .into_iter()
                .filter(|row| !pixel_index::index::credential_path(Path::new(&row.path)))
                .map(|row| FileHit {
                    path: row.path,
                    line: row.line_number,
                })
                .collect(),
            capped: stats.truncated,
        })
    }
}

impl Evidence for Live {
    fn files_with(&self, anchor: &str, deadline: Instant) -> Result<Found, String> {
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
                Ok(search_files(&self.ask(&request, deadline)?))
            }
            Route::Local => self.local_files(anchor),
        }
    }

    fn concept(&self, phrase: &str, deadline: Instant) -> Result<Found, String> {
        match self.route {
            Route::Daemon => {
                let request = Request::Resolve {
                    phrase: phrase.to_string(),
                    limit: Some(CONCEPT_ROWS),
                };
                Ok(resolve_files(&self.ask(&request, deadline)?))
            }
            Route::Local => Err("a concept search needs a running daemon".into()),
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

/// `search` rows (`{path, line, text}`) as file hits.
fn search_files(data: &Value) -> Found {
    Found {
        hits: file_rows(data, "matches", "line"),
        capped: data.get("truncated").and_then(Value::as_bool) == Some(true),
    }
}

/// `resolve` matches (`{path, start_line, kind, score, raw}`) as file hits.
fn resolve_files(data: &Value) -> Found {
    Found {
        hits: file_rows(data, "matches", "start_line"),
        capped: false,
    }
}

fn file_rows(data: &Value, key: &str, line_key: &str) -> Vec<FileHit> {
    data.get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|row| {
            Some(FileHit {
                path: row.get("path")?.as_str()?.to_string(),
                line: row.get(line_key).and_then(Value::as_u64).unwrap_or(0),
            })
        })
        .collect()
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
    fn search_files_should_read_rows_and_the_truncation_flag() {
        let data = json!({"matches": [
            {"path": "src/a.ts", "line": 4, "text": "x"},
            {"line": 5},
            {"path": "src/b.ts"}
        ], "truncated": true});
        assert_eq!(
            search_files(&data),
            Found {
                hits: vec![
                    FileHit {
                        path: "src/a.ts".into(),
                        line: 4
                    },
                    FileHit {
                        path: "src/b.ts".into(),
                        line: 0
                    }
                ],
                capped: true
            }
        );
        assert_eq!(search_files(&json!({})), Found::default());
        assert!(!search_files(&json!({"matches": [], "truncated": false})).capped);
    }

    #[test]
    fn resolve_files_should_read_the_start_line_of_each_match() {
        let data = json!({"matches": [
            {"path": "app/routes.rb", "start_line": 12, "kind": "route", "score": 0.9, "raw": "x"}
        ]});
        assert_eq!(
            resolve_files(&data).hits,
            [FileHit {
                path: "app/routes.rb".into(),
                line: 12
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
        assert_eq!(
            live.concept("anything", deadline).unwrap_err(),
            "a concept search needs a running daemon"
        );
        assert_eq!(
            live.files_with("anything", deadline).unwrap_err(),
            "the text index does not cover HEAD"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
