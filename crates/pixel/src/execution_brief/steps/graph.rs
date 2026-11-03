// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The code graph, as a step reads it.
//!
//! A probe script asks the graph by running `pixel find-symbol`, `who-calls`
//! and `call-path` once per question; a step asks the repository's daemon the
//! same three ops on a socket, in this process, with the brief's own request
//! path (`evidence::connect`, `crate::roundtrip_by`). Every request carries
//! the step's deadline both as the socket timeout and as the request deadline
//! (`pixel_proto::REQUEST_DEADLINE_FIELD`): the step's deadline is already
//! the earlier of its budget and the brief's answer deadline
//! (`probes::run_step`), so a graph call cannot outlive either. Each op
//! opts into the published generation (`stale_ok`), as every graph read of
//! the brief does: a hook never waits for a rebuild.
//!
//! Without a daemon that answers in time, every read here is empty and the
//! step prints nothing, as the script did without a `pixel` to run. The
//! staleness notice the daemon attaches to a stale answer is not forwarded
//! (the brief's own blocks carry it).

use pixel_daemon::Request;
use serde::Deserialize;
use serde_json::Value;

use super::Ctx;
use crate::execution_brief::evidence;

/// A symbol as the daemon's `symbol` and `uses` ops print it.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub(super) struct Sym {
    pub(super) uid: String,
    pub(super) name: String,
    pub(super) qualified: String,
    pub(super) kind: String,
    pub(super) path: String,
    /// 1-based, inclusive; signed so a step can do the arithmetic of a window
    /// around it (`line - 2`) without underflow.
    pub(super) start_line: i64,
    pub(super) end_line: i64,
    pub(super) sig: String,
}

impl Sym {
    /// The qualified name, else the plain one (`c.get('qualified', c['name'])`).
    pub(super) fn qualified_or_name(&self) -> &str {
        if self.qualified.is_empty() {
            &self.name
        } else {
            &self.qualified
        }
    }
}

/// One edge of `uses`: the symbol at the other end and the call site's line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Edge {
    pub(super) symbol: Sym,
    pub(super) tier: String,
    /// `None` when the graph recorded no site.
    pub(super) site_line: Option<i64>,
}

/// One symbol of a `call-path` chain.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub(super) struct Hop {
    pub(super) uid: String,
    pub(super) name: String,
    pub(super) path: String,
    pub(super) line: i64,
}

/// Which side of a call `uses` lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Role {
    Callers,
    Callees,
}

impl Role {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Callers => "callers",
            Self::Callees => "callees",
        }
    }
}

/// Edges per page when the daemon does not say (`data.get('edge_limit', 20)`).
const DEFAULT_EDGE_LIMIT: usize = 20;

impl Ctx<'_> {
    /// The data of one daemon request, within the step's deadline; `None`
    /// when nothing listens, the time is up, or the daemon refuses.
    pub(super) fn graph(&self, request: &Request) -> Option<Value> {
        let mut stream = evidence::connect(self.root, self.deadline)?;
        let response = crate::roundtrip_by(
            &mut stream,
            request,
            Some(evidence::unix_ms_at(self.deadline)),
        )?;
        crate::unwrap_response(response).ok()
    }

    /// The symbols of the graph named `name` (`pixel find-symbol`).
    pub(super) fn symbols_named(&self, name: &str) -> Vec<Sym> {
        self.graph(&Request::Symbol {
            name: name.to_owned(),
            stale_ok: true,
        })
        .and_then(|data| rows(&data, "symbols"))
        .unwrap_or_default()
    }

    /// Every call edge of `uid` on one side, page after page while a page is
    /// full and the offset has not passed `max_offset` (`pixel who-calls
    /// --role`): the script's loop, so a symbol with hundreds of edges is
    /// read as far as it was.
    pub(super) fn call_edges(&self, uid: &str, role: Role, max_offset: usize) -> Vec<Edge> {
        let mut out = Vec::new();
        let mut offset = 0;
        while offset <= max_offset && !self.timed_out() {
            let Some(data) = self.graph(&Request::Uses {
                uid_or_name: uid.to_owned(),
                role: role.as_str().to_owned(),
                offset: Some(offset),
                stale_ok: true,
            }) else {
                break;
            };
            let page = edges_of(&data);
            // The page's own size, whether or not each edge's far symbol was named.
            let got = data
                .get("edges")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let limit = data
                .get("edge_limit")
                .and_then(Value::as_u64)
                .and_then(|limit| usize::try_from(limit).ok())
                .unwrap_or(DEFAULT_EDGE_LIMIT);
            out.extend(page);
            if got < limit || got == 0 {
                break;
            }
            offset += got;
        }
        out
    }

    /// The shortest call chain from `from` to `to`, both uids
    /// (`pixel call-path`); `None` when the graph finds none.
    pub(super) fn call_path(&self, from: &str, to: &str) -> Option<Vec<Hop>> {
        let data = self.graph(&Request::Trace {
            from: from.to_owned(),
            to: to.to_owned(),
            stale_ok: true,
        })?;
        if data.get("found") != Some(&Value::Bool(true)) {
            return None;
        }
        rows(&data, "hops")
    }
}

/// The array `key` of `data` read as `T`, `None` when it is not one.
fn rows<T: for<'de> Deserialize<'de>>(data: &Value, key: &str) -> Option<Vec<T>> {
    serde_json::from_value(data.get(key)?.clone()).ok()
}

/// The edges of one `uses` page; an edge whose far symbol the graph could not
/// name is left out.
fn edges_of(data: &Value) -> Vec<Edge> {
    let Some(rows) = data.get("edges").and_then(Value::as_array) else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let symbol: Sym = serde_json::from_value(row.get("symbol")?.clone()).ok()?;
            Some(Edge {
                symbol,
                tier: row
                    .get("tier")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                site_line: row.get("site_line").and_then(Value::as_i64),
            })
        })
        .collect()
}

#[cfg(test)]
pub(super) mod testing {
    //! A daemon that answers the graph ops of a step with canned data, and
    //! remembers what it was asked.

    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};

    /// Serves `root`'s socket until dropped; `answer` maps a request to the
    /// `result` of its response.
    pub(crate) struct FakeGraph {
        seen: Arc<Mutex<Vec<Value>>>,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl FakeGraph {
        pub(crate) fn serve(
            root: &Path,
            answer: impl Fn(&Value) -> Value + Send + 'static,
        ) -> Self {
            let socket = pixel_daemon::socket_path(root);
            let _ = std::fs::remove_file(&socket);
            let listener = UnixListener::bind(&socket).unwrap();
            listener.set_nonblocking(true).unwrap();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let (log, halt) = (Arc::clone(&seen), Arc::clone(&stop));
            let thread = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(15);
                while !halt.load(Ordering::SeqCst) && Instant::now() < deadline {
                    let Ok((mut stream, _)) = listener.accept() else {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    };
                    // BSD hands back a non-blocking socket from a non-blocking listener.
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let mut line = String::new();
                    let read = stream
                        .try_clone()
                        .map(BufReader::new)
                        .map(|mut reader| reader.read_line(&mut line));
                    if !matches!(read, Ok(Ok(1..))) {
                        continue;
                    }
                    let request: Value = serde_json::from_str(&line).unwrap();
                    log.lock().unwrap().push(request.clone());
                    let reply = pixel_daemon::Response::success("test", answer(&request));
                    let _ = writeln!(stream, "{}", serde_json::to_string(&reply).unwrap());
                }
            });
            Self {
                seen,
                stop,
                thread: Some(thread),
            }
        }

        /// The requests so far, as they arrived.
        pub(crate) fn requests(&self) -> Vec<Value> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl Drop for FakeGraph {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// A `symbol` row the way the daemon prints it.
    pub(crate) fn sym(uid: &str, name: &str, kind: &str, path: &str, lines: (u32, u32)) -> Value {
        json!({"uid": uid, "name": name, "qualified": name, "kind": kind,
               "path": path, "start_line": lines.0, "end_line": lines.1, "sig": ""})
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{FakeGraph, sym};
    use super::*;
    use crate::execution_brief::steps::testing::repo;
    use serde_json::json;
    use std::time::{Duration, Instant};

    fn ctx(root: &std::path::Path) -> Ctx<'_> {
        Ctx::new(root, "p", "b", Instant::now() + Duration::from_secs(10))
    }

    #[test]
    fn symbols_named_reads_the_symbol_op_and_nothing_else() {
        let dir = repo(&[("a.rs", "x\n")]);
        let daemon = FakeGraph::serve(dir.path(), |request| {
            assert_eq!(request["op"], "symbol");
            json!({"symbols": [sym("a.rs#go#function", "go", "function", "a.rs", (3, 9))]})
        });
        let found = ctx(dir.path()).symbols_named("go");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].uid, "a.rs#go#function");
        assert_eq!((found[0].start_line, found[0].end_line), (3, 9));
        assert_eq!(found[0].qualified_or_name(), "go");
        let asked = daemon.requests();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0]["name"], "go");
        assert_eq!(
            asked[0]["stale_ok"], true,
            "a hook never waits for a rebuild"
        );
        assert!(
            asked[0].get(pixel_proto::REQUEST_DEADLINE_FIELD).is_some(),
            "the request carries the step's deadline"
        );
    }

    #[test]
    fn without_a_daemon_every_read_is_empty() {
        let dir = repo(&[("a.rs", "x\n")]);
        let ctx = ctx(dir.path());
        assert!(ctx.symbols_named("go").is_empty());
        assert!(ctx.call_edges("u", Role::Callers, 100).is_empty());
        assert_eq!(ctx.call_path("a", "b"), None);
    }

    #[test]
    fn call_edges_page_while_a_page_is_full() {
        let dir = repo(&[("a.rs", "x\n")]);
        let daemon = FakeGraph::serve(dir.path(), |request| {
            assert_eq!(request["role"], "callees");
            let offset = request["offset"].as_u64().unwrap();
            let count = if offset == 0 { 2 } else { 1 };
            let edges: Vec<_> = (0..count)
                .map(|n| {
                    json!({"symbol": sym(&format!("u{offset}{n}"), "f", "function", "a.rs", (1, 2)),
                           "tier": "exact", "site_line": 7})
                })
                .collect();
            json!({"edges": edges, "edge_limit": 2})
        });
        let edges = ctx(dir.path()).call_edges("root", Role::Callees, 100);
        let uids: Vec<_> = edges.iter().map(|e| e.symbol.uid.as_str()).collect();
        assert_eq!(uids, ["u00", "u01", "u20"]);
        assert_eq!(edges[0].site_line, Some(7));
        assert_eq!(edges[0].tier, "exact");
        let offsets: Vec<_> = daemon
            .requests()
            .iter()
            .map(|r| r["offset"].clone())
            .collect();
        assert_eq!(offsets, [json!(0), json!(2)]);
    }

    #[test]
    fn call_edges_stop_at_the_offset_bound() {
        let dir = repo(&[("a.rs", "x\n")]);
        let daemon = FakeGraph::serve(dir.path(), |_| {
            json!({"edges": [
                {"symbol": sym("u", "f", "function", "a.rs", (1, 2)), "tier": "exact", "site_line": null}
            ], "edge_limit": 1})
        });
        // Every page is full; the bound, not the data, ends the loop.
        let edges = ctx(dir.path()).call_edges("root", Role::Callers, 3);
        assert_eq!(edges.len(), 4);
        assert_eq!(edges[0].site_line, None);
        let offsets: Vec<_> = daemon
            .requests()
            .iter()
            .map(|r| r["offset"].clone())
            .collect();
        assert_eq!(offsets, [json!(0), json!(1), json!(2), json!(3)]);
        assert_eq!(daemon.requests()[0]["role"], "callers");
    }

    #[test]
    fn call_edges_page_by_the_edges_the_daemon_sent_not_the_ones_it_named() {
        let dir = repo(&[("a.rs", "x\n")]);
        let daemon = FakeGraph::serve(dir.path(), |request| {
            if request["offset"] == 0 {
                json!({"edges": [
                    {"symbol": sym("u", "f", "function", "a.rs", (1, 2)), "tier": "exact"},
                    {"symbol": null, "tier": "exact"}
                ], "edge_limit": 2})
            } else {
                json!({"edges": [], "edge_limit": 2})
            }
        });
        let edges = ctx(dir.path()).call_edges("root", Role::Callers, 100);
        assert_eq!(edges.len(), 1, "the unnamed far symbol is left out");
        let offsets: Vec<_> = daemon
            .requests()
            .iter()
            .map(|r| r["offset"].clone())
            .collect();
        assert_eq!(offsets, [json!(0), json!(2)]);
    }

    #[test]
    fn call_path_is_the_hops_of_a_found_chain() {
        let dir = repo(&[("a.rs", "x\n")]);
        let _daemon = FakeGraph::serve(dir.path(), |request| {
            assert_eq!(request["op"], "trace");
            if request["to"] == "none" {
                return json!({"found": false, "hops": [{"uid": "x"}]});
            }
            json!({"found": true, "hops": [
                {"uid": "a", "name": "go", "path": "a.rs", "line": 3},
                {"uid": "b", "name": "stop", "path": "b.rs", "line": 9}
            ]})
        });
        let ctx = ctx(dir.path());
        let hops = ctx.call_path("a", "b").unwrap();
        assert_eq!(hops.len(), 2);
        assert_eq!(hops[1].name, "stop");
        assert_eq!(hops[1].line, 9);
        assert_eq!(ctx.call_path("a", "none"), None, "found: false is no chain");
    }
}
