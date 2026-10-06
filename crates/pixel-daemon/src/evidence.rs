// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Bounded JSONL evidence bridge for interactive harnesses.
//!
//! The normal daemon protocol remains request/response. This bridge is a
//! long-lived stdio adapter that correlates concurrent bundle requests, emits
//! useful lexical/structural evidence before completion, and never permits a
//! slow client to consume more than its small in-flight allotment.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::Service;

pub const EVIDENCE_PROTOCOL_VERSION: u64 = 1;
const MAX_IN_FLIGHT: usize = 4;
const MAX_QUERIES_PER_BUNDLE: usize = 8;
const MAX_QUERY_LIMIT: usize = 32;
/// Longest request line accepted on stdin.
const MAX_REQUEST_LINE: usize = 65_536; // 64 KiB

type ActiveRequests = Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>;

/// One admitted bundle's claim on the bounded queue. Dropping it gives the
/// slot and the requestId back, so a worker that panics cannot shrink the
/// queue for the rest of the bridge's life.
struct QueueSlot {
    in_flight: Arc<AtomicUsize>,
    active: ActiveRequests,
    request_id: String,
}

impl Drop for QueueSlot {
    fn drop(&mut self) {
        if let Ok(mut active) = self.active.lock() {
            active.remove(&self.request_id);
        }
        self.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Status and stat of one dirty path: an edit to an already-dirty file
/// changes the stat, not the status.
type DirtyEntry = (String, Option<(u64, u128)>);

/// What the bridge last indexed of the working tree. The bridge has no
/// watcher, so comparing this before each bundle is how an edit made since
/// the previous bundle reaches the shared index.
#[derive(Debug, Default, PartialEq)]
struct TreeState {
    head: Option<String>,
    dirty: BTreeMap<String, DirtyEntry>,
}

impl TreeState {
    fn capture(root: &Path) -> Self {
        let dirty = pixel_index::gitsync::status_porcelain(root)
            .into_iter()
            .map(|(xy, path)| {
                let stat = std::fs::symlink_metadata(root.join(&path))
                    .ok()
                    .map(|meta| (meta.len(), modified_nanos(&meta)));
                (path, (xy, stat))
            })
            .collect();
        TreeState {
            head: pixel_index::gitsync::rev_parse_head(root),
            dirty,
        }
    }
}

fn modified_nanos(meta: &std::fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |elapsed| elapsed.as_nanos())
}

/// Paths whose indexed content may lag the tree between `before` and
/// `after`: every path of the commit range (`committed`) plus every dirty
/// path that appeared, disappeared, or changed status or stat.
fn stale_paths(before: &TreeState, after: &TreeState, committed: &[(char, String)]) -> Vec<String> {
    let mut stale: BTreeSet<String> = committed.iter().map(|(_, path)| path.clone()).collect();
    for (path, entry) in &after.dirty {
        if before.dirty.get(path) != Some(entry) {
            stale.insert(path.clone());
        }
    }
    for path in before.dirty.keys() {
        if !after.dirty.contains_key(path) {
            stale.insert(path.clone());
        }
    }
    stale.into_iter().collect()
}

/// The writable service every bundle's read replica is cut from, and the
/// tree it last caught up with.
struct Master {
    service: Arc<Mutex<Service>>,
    seen: TreeState,
}

impl Master {
    fn open(root: &Path) -> Result<Self, String> {
        // Captured before the open: an edit racing the open is refreshed
        // again by the next bundle, never lost.
        let seen = TreeState::capture(root);
        let service = Service::open(root).map_err(|error| error.to_string())?;
        Ok(Master {
            service: Arc::new(Mutex::new(service)),
            seen,
        })
    }

    /// Bring the shared index up to the tree before a bundle reads it.
    fn catch_up(&mut self, root: &Path) -> Result<(), String> {
        let now = TreeState::capture(root);
        if now == self.seen {
            return Ok(());
        }
        let committed = match (&self.seen.head, &now.head) {
            (Some(before), Some(after)) if before != after => {
                pixel_index::gitsync::diff_name_status(root, before, after)
            }
            _ => Vec::new(),
        };
        let stale = stale_paths(&self.seen, &now, &committed);
        if !stale.is_empty() {
            let files: Vec<(&str, bool)> = stale
                .iter()
                .map(|path| (path.as_str(), !root.join(path).exists()))
                .collect();
            self.service
                .lock()
                .map_err(|_| "evidence service lock poisoned".to_string())?
                .refresh_files(&files);
        }
        self.seen = now;
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct Request {
    op: String,
    version: u64,
    #[serde(rename = "requestId")]
    request_id: String,
    generation: Option<u64>,
    #[serde(rename = "deadlineMs")]
    deadline_ms: Option<u64>,
    queries: Option<Vec<Query>>,
    #[serde(rename = "targetRequestId")]
    target_request_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Query {
    id: String,
    kind: String,
    query: String,
    limit: Option<usize>,
}

fn frame(request_id: &str, kind: &str, body: Value) -> Value {
    let mut frame = json!({
        "version": EVIDENCE_PROTOCOL_VERSION,
        "requestId": request_id,
        "type": kind,
    });
    if let Some(map) = frame.as_object_mut()
        && let Some(body) = body.as_object()
    {
        map.extend(body.clone());
    }
    frame
}

fn write_frame<W: Write>(writer: &Arc<Mutex<W>>, value: Value) -> Result<(), String> {
    let encoded = serde_json::to_string(&value).map_err(|error| error.to_string())?;
    let mut writer = writer
        .lock()
        .map_err(|_| "evidence stdout lock poisoned".to_string())?;
    writer
        .write_all(encoded.as_bytes())
        .and_then(|_| writer.write_all(b"\n"))
        .and_then(|_| writer.flush())
        .map_err(|error| format!("evidence stdout: {error}"))
}

fn capabilities(request_id: &str) -> Value {
    frame(
        request_id,
        "capabilities",
        json!({
            "protocolVersions": [EVIDENCE_PROTOCOL_VERSION],
            "features": ["bundle", "partial_results", "cancellation", "deadlines", "bounded_queue"],
            "maxInFlight": MAX_IN_FLIGHT,
        }),
    )
}

fn complete(request_id: &str, generation: Option<u64>, status: &str, error: Option<&str>) -> Value {
    let mut body = json!({ "status": status });
    if let Some(generation) = generation {
        body["generation"] = json!(generation);
    }
    if let Some(error) = error {
        body["error"] = json!(error);
    }
    frame(request_id, "complete", body)
}

fn interrupted_status(
    cancelled: &AtomicBool,
    started: Instant,
    deadline_ms: Option<u64>,
) -> Option<&'static str> {
    if cancelled.load(Ordering::Acquire) {
        return Some("cancelled");
    }
    if deadline_ms.is_some_and(|deadline| started.elapsed().as_millis() >= u128::from(deadline)) {
        return Some("deadline");
    }
    None
}

fn register_active(
    active: &ActiveRequests,
    request_id: &str,
    cancelled: Arc<AtomicBool>,
) -> Result<bool, String> {
    let mut active = active
        .lock()
        .map_err(|_| "evidence active lock poisoned".to_string())?;
    if active.contains_key(request_id) {
        return Ok(false);
    }
    active.insert(request_id.to_string(), cancelled);
    Ok(true)
}

fn reap_finished_workers(workers: &mut Vec<JoinHandle<()>>) {
    let mut pending = Vec::with_capacity(workers.len());
    for worker in std::mem::take(workers) {
        if worker.is_finished() {
            let _ = worker.join();
        } else {
            pending.push(worker);
        }
    }
    *workers = pending;
}

fn run_bundle<W: Write + Send + 'static>(
    master: &Mutex<Option<Master>>,
    root: &Path,
    writer: &Arc<Mutex<W>>,
    slot: QueueSlot,
    request: Request,
    cancelled: &AtomicBool,
    started: Instant,
) {
    let request_id = request.request_id.clone();
    let generation = request.generation;
    let work = std::panic::AssertUnwindSafe(|| {
        bundle_frames(master, root, writer, request, cancelled, started)
    });
    let (status, error) = match std::panic::catch_unwind(work) {
        Ok(Ok(status)) => (status, None),
        Ok(Err(error)) => ("error", Some(error)),
        Err(_) => ("error", Some("evidence worker panicked".to_string())),
    };
    // Release before the terminal frame: a client that sends its next
    // bundle (or reuses the requestId) on seeing `complete` must find the
    // slot free, not a spurious `backpressure` or duplicate.
    drop(slot);
    let _ = write_frame(
        writer,
        complete(&request_id, generation, status, error.as_deref()),
    );
}

/// Validate a bundle, catch the index up with the tree, and write one
/// `partial` frame per query; the terminal status is the caller's to write.
fn bundle_frames<W: Write>(
    master: &Mutex<Option<Master>>,
    root: &Path,
    writer: &Arc<Mutex<W>>,
    request: Request,
    cancelled: &AtomicBool,
    started: Instant,
) -> Result<&'static str, String> {
    let request_id = request.request_id;
    {
        let queries = request
            .queries
            .ok_or_else(|| "bundle requires queries".to_string())?;
        if queries.is_empty() || queries.len() > MAX_QUERIES_PER_BUNDLE {
            return Err(format!(
                "bundle requires 1..={MAX_QUERIES_PER_BUNDLE} queries"
            ));
        }
        if let Some(status) = interrupted_status(cancelled, started, request.deadline_ms) {
            return Ok(status);
        }
        // The clone is quick and shares the indexed text generation, but owns
        // its SQLite connection: readers never hold the master service lock.
        let service = {
            let mut slot = master
                .lock()
                .map_err(|_| "evidence service slot lock poisoned".to_string())?;
            match slot.as_mut() {
                Some(master) => master.catch_up(root)?,
                None => *slot = Some(Master::open(root)?),
            }
            Arc::clone(&slot.as_ref().expect("master opened above").service)
        };
        let mut reader = service
            .lock()
            .map_err(|_| "evidence service lock poisoned".to_string())?
            .read_replica();
        for query in queries {
            if let Some(status) = interrupted_status(cancelled, started, request.deadline_ms) {
                return Ok(status);
            }
            let limit = query.limit.unwrap_or(8).clamp(1, MAX_QUERY_LIMIT);
            let (publication, result) = reader.read_evidence(&query.kind, &query.query, limit);
            if let Some(status) = interrupted_status(cancelled, started, request.deadline_ms) {
                return Ok(status);
            }
            let mut body = json!({
                "generation": request.generation,
                "indexGeneration": publication.generation,
                "queryId": query.id,
            });
            match result {
                Ok(result) => body["result"] = result,
                Err(error) => body["error"] = json!(error),
            }
            write_frame(writer, frame(&request_id, "partial", body))?;
        }
        Ok("complete")
    }
}

/// One line of `reader` without its `\n` or `\r\n`, or `None` at EOF. The
/// cap is enforced while reading: `take` stops two bytes past `max`, room
/// for a `\r\n` after a line of exactly `max` bytes, so a line that never
/// ends costs `max + 2` bytes, not the whole stream.
fn read_bounded_line<R: BufRead>(reader: &mut R, max: usize) -> Result<Option<String>, String> {
    let mut line = Vec::new();
    let read = std::io::Read::take(&mut *reader, max as u64 + 2)
        .read_until(b'\n', &mut line)
        .map_err(|error| format!("evidence stdin: {error}"))?;
    if read == 0 {
        return Ok(None);
    }
    if line.last() == Some(&b'\n') {
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
    }
    if line.len() > max {
        return Err("evidence request exceeds 64KiB".into());
    }
    String::from_utf8(line)
        .map(Some)
        .map_err(|_| "evidence stdin: stream did not contain valid UTF-8".into())
}

/// Serve the evidence JSONL protocol. EOF waits for accepted work to produce
/// its terminal frames, so a short-lived caller does not silently lose output.
pub fn serve<R: BufRead, W: Write + Send + 'static>(
    root: &Path,
    reader: R,
    writer: W,
) -> Result<(), String> {
    serve_bounded(root, reader, writer, usize::MAX)
}

/// [`serve`] for at most `max_requests` request lines. Tests pass a small
/// bound, so a reader that never reports EOF ends the loop instead of the
/// test's time budget.
fn serve_bounded<R: BufRead, W: Write + Send + 'static>(
    root: &Path,
    mut reader: R,
    writer: W,
    max_requests: usize,
) -> Result<(), String> {
    // Capability negotiation must be independent of index construction: Pi
    // gives this bridge only 500ms to answer before it takes the legacy path.
    let master: Arc<Mutex<Option<Master>>> = Arc::new(Mutex::new(None));
    let root: PathBuf = root.to_path_buf();
    let writer = Arc::new(Mutex::new(writer));
    let active: ActiveRequests = Arc::new(Mutex::new(HashMap::new()));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let mut workers: Vec<JoinHandle<()>> = Vec::new();

    for _ in 0..max_requests {
        let Some(line) = read_bounded_line(&mut reader, MAX_REQUEST_LINE)? else {
            break;
        };
        reap_finished_workers(&mut workers);
        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                let _ = write_frame(
                    &writer,
                    frame(
                        "unknown",
                        "complete",
                        json!({"status":"error", "error": format!("invalid request: {error}")}),
                    ),
                );
                continue;
            }
        };
        if request.version != EVIDENCE_PROTOCOL_VERSION {
            let _ = write_frame(
                &writer,
                complete(
                    &request.request_id,
                    request.generation,
                    "error",
                    Some("unsupported protocol version"),
                ),
            );
            continue;
        }
        match request.op.as_str() {
            "capabilities" => write_frame(&writer, capabilities(&request.request_id))?,
            "cancel" => {
                if let Some(target) = request.target_request_id.as_deref()
                    && let Ok(active) = active.lock()
                    && let Some(cancelled) = active.get(target)
                {
                    cancelled.store(true, Ordering::Release);
                }
                write_frame(
                    &writer,
                    complete(&request.request_id, request.generation, "complete", None),
                )?;
            }
            "bundle" => {
                if in_flight.fetch_add(1, Ordering::AcqRel) >= MAX_IN_FLIGHT {
                    in_flight.fetch_sub(1, Ordering::AcqRel);
                    write_frame(
                        &writer,
                        complete(
                            &request.request_id,
                            request.generation,
                            "backpressure",
                            Some("evidence queue is full"),
                        ),
                    )?;
                    continue;
                }
                let cancelled = Arc::new(AtomicBool::new(false));
                if !register_active(&active, &request.request_id, Arc::clone(&cancelled))? {
                    in_flight.fetch_sub(1, Ordering::AcqRel);
                    write_frame(
                        &writer,
                        complete(
                            &request.request_id,
                            request.generation,
                            "error",
                            Some("duplicate active evidence requestId"),
                        ),
                    )?;
                    continue;
                }
                let slot = QueueSlot {
                    in_flight: Arc::clone(&in_flight),
                    active: Arc::clone(&active),
                    request_id: request.request_id.clone(),
                };
                let started = Instant::now();
                workers.push(std::thread::spawn({
                    let master = Arc::clone(&master);
                    let root = root.clone();
                    let writer = Arc::clone(&writer);
                    move || run_bundle(&master, &root, &writer, slot, request, &cancelled, started)
                }));
            }
            _ => write_frame(
                &writer,
                complete(
                    &request.request_id,
                    request.generation,
                    "error",
                    Some("unsupported evidence operation"),
                ),
            )?,
        }
    }
    for worker in workers {
        let _ = worker.join();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Request lines a test bridge serves at most (see `serve_bounded`).
    const TEST_MAX_REQUESTS: usize = 256;
    use std::io::Write;

    #[derive(Clone)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);
    impl Write for SharedBuffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn capability_frame_advertises_the_client_required_features() {
        let value = capabilities("cap-1");
        assert_eq!(value["version"], EVIDENCE_PROTOCOL_VERSION);
        assert_eq!(value["requestId"], "cap-1");
        assert_eq!(value["type"], "capabilities");
        assert!(
            value["features"]
                .as_array()
                .unwrap()
                .iter()
                .any(|feature| feature == "bundle")
        );
        assert!(
            value["features"]
                .as_array()
                .unwrap()
                .iter()
                .any(|feature| feature == "partial_results")
        );
        assert!(
            value["features"]
                .as_array()
                .unwrap()
                .iter()
                .any(|feature| feature == "cancellation")
        );
        assert!(
            value["features"]
                .as_array()
                .unwrap()
                .iter()
                .any(|feature| feature == "deadlines")
        );
        assert!(
            value["features"]
                .as_array()
                .unwrap()
                .iter()
                .any(|feature| feature == "bounded_queue")
        );
    }

    #[test]
    fn capability_handshake_does_not_open_the_repository_service() {
        let input = b"{\"op\":\"capabilities\",\"version\":1,\"requestId\":\"cap-1\"}\n";
        let output = Arc::new(Mutex::new(Vec::new()));
        serve_bounded(
            Path::new("/definitely-not-an-indexed-repository"),
            std::io::Cursor::new(input),
            SharedBuffer(Arc::clone(&output)),
            TEST_MAX_REQUESTS,
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&output.lock().unwrap()).unwrap();
        assert_eq!(value["type"], "capabilities");
    }

    #[test]
    fn interruption_checks_cancel_and_deadline_before_read_work() {
        let cancelled = AtomicBool::new(false);
        let started = Instant::now() - std::time::Duration::from_millis(1);
        assert_eq!(
            interrupted_status(&cancelled, started, Some(0)),
            Some("deadline")
        );

        cancelled.store(true, Ordering::Release);
        assert_eq!(
            interrupted_status(&cancelled, Instant::now(), None),
            Some("cancelled")
        );
    }

    #[test]
    fn active_request_ids_cannot_replace_a_cancellation_handle() {
        let active = Arc::new(Mutex::new(HashMap::new()));
        let first = Arc::new(AtomicBool::new(false));
        assert!(register_active(&active, "bundle-1", Arc::clone(&first)).unwrap());
        assert!(!register_active(&active, "bundle-1", Arc::new(AtomicBool::new(false))).unwrap());

        active.lock().unwrap()["bundle-1"].store(true, Ordering::Release);
        assert!(first.load(Ordering::Acquire));
    }

    fn frames(output: &Arc<Mutex<Vec<u8>>>) -> Vec<Value> {
        String::from_utf8_lossy(&output.lock().unwrap())
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn serve_lines(root: &Path, lines: &[String]) -> (Result<(), String>, Vec<Value>) {
        let input = lines
            .iter()
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        let output = Arc::new(Mutex::new(Vec::new()));
        let result = serve_bounded(
            root,
            std::io::Cursor::new(input.into_bytes()),
            SharedBuffer(Arc::clone(&output)),
            TEST_MAX_REQUESTS,
        );
        (result, frames(&output))
    }

    fn bundle(request_id: &str, queries: usize) -> String {
        let queries: Vec<Value> = (0..queries)
            .map(|n| json!({"id": format!("q{n}"), "kind": "search", "query": "x"}))
            .collect();
        json!({"op": "bundle", "version": 1, "requestId": request_id, "queries": queries})
            .to_string()
    }

    fn terminal<'a>(frames: &'a [Value], request_id: &str) -> &'a Value {
        frames
            .iter()
            .find(|frame| frame["requestId"] == request_id && frame["type"] == "complete")
            .unwrap_or_else(|| panic!("no complete frame for {request_id}: {frames:?}"))
    }

    const NO_REPO: &str = "/definitely-not-an-indexed-repository";

    #[test]
    fn a_bundle_outside_one_to_eight_queries_is_refused_before_touching_the_index() {
        let (result, frames) =
            serve_lines(Path::new(NO_REPO), &[bundle("empty", 0), bundle("nine", 9)]);
        result.unwrap();
        for id in ["empty", "nine"] {
            let done = terminal(&frames, id);
            assert_eq!(done["status"], "error", "{done}");
            assert_eq!(done["error"], "bundle requires 1..=8 queries", "{done}");
        }
        // Eight is inside the bound: it gets past validation and fails on
        // the (missing) repository instead.
        let (_, frames) = serve_lines(Path::new(NO_REPO), &[bundle("eight", 8)]);
        let done = terminal(&frames, "eight");
        assert_ne!(done["error"], "bundle requires 1..=8 queries", "{done}");
    }

    #[test]
    fn unknown_versions_operations_and_malformed_lines_get_an_error_frame_each() {
        let (result, frames) = serve_lines(
            Path::new(NO_REPO),
            &[
                json!({"op": "capabilities", "version": 2, "requestId": "v2"}).to_string(),
                json!({"op": "explode", "version": 1, "requestId": "op"}).to_string(),
                "not json".to_string(),
                json!({"op": "cancel", "version": 1, "requestId": "c", "targetRequestId": "none"})
                    .to_string(),
            ],
        );
        result.unwrap();
        assert_eq!(
            terminal(&frames, "v2")["error"],
            "unsupported protocol version"
        );
        assert_eq!(
            terminal(&frames, "op")["error"],
            "unsupported evidence operation"
        );
        assert_eq!(terminal(&frames, "unknown")["status"], "error");
        // Cancelling a request that is not running is acknowledged, not an error.
        assert_eq!(terminal(&frames, "c")["status"], "complete");
    }

    /// A reader that counts the bytes the bridge pulls from it.
    struct Counting<R> {
        inner: R,
        read: Arc<AtomicUsize>,
    }

    impl<R: std::io::Read> std::io::Read for Counting<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read.fetch_add(n, Ordering::Relaxed);
            Ok(n)
        }
    }

    /// A client that never sends a newline cannot make the bridge hold the
    /// whole line: the cap is enforced while reading, so 1 MiB without a
    /// newline is refused after the first 64 KiB + 1 byte (plus one buffer).
    #[test]
    fn a_line_without_newline_should_be_refused_after_the_cap_not_after_the_line() {
        let read = Arc::new(AtomicUsize::new(0));
        let reader = std::io::BufReader::with_capacity(
            4096,
            Counting {
                inner: std::io::Read::take(std::io::repeat(b'x'), 1_048_576),
                read: Arc::clone(&read),
            },
        );
        let result = serve_bounded(
            Path::new(NO_REPO),
            reader,
            SharedBuffer(Arc::default()),
            TEST_MAX_REQUESTS,
        );
        assert_eq!(result, Err("evidence request exceeds 64KiB".to_string()));
        let consumed = read.load(Ordering::Relaxed);
        assert!(
            consumed <= MAX_REQUEST_LINE + 2 + 4096,
            "read {consumed} bytes"
        );
    }

    /// The cap is on the request, not on its line ending: exactly `max`
    /// bytes pass with `\n` or `\r\n`, one more byte does not.
    #[test]
    fn read_bounded_line_should_accept_a_line_of_exactly_the_cap_with_either_ending() {
        for ending in ["\n", "\r\n"] {
            let mut reader =
                std::io::Cursor::new(format!("abcd{ending}abcde{ending}").into_bytes());
            assert_eq!(
                read_bounded_line(&mut reader, 4),
                Ok(Some("abcd".into())),
                "{ending:?}"
            );
            assert_eq!(
                read_bounded_line(&mut reader, 4),
                Err("evidence request exceeds 64KiB".into()),
                "{ending:?}"
            );
        }
    }

    /// A writer that keeps the first 1 MiB and drops the rest, so a test
    /// bridge that spins cannot exhaust memory before its deadline.
    struct CappedBuffer(Arc<Mutex<Vec<u8>>>);
    impl Write for CappedBuffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let mut kept = self.0.lock().unwrap();
            let room = 1_048_576_usize.saturating_sub(kept.len());
            kept.extend_from_slice(&bytes[..bytes.len().min(room)]);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The public `serve` has no request bound: it answers more requests than
    /// the tests' `serve_bounded` limit. It runs on a thread with a
    /// deadline, so a reader that never reports EOF fails the test instead
    /// of hanging it.
    #[test]
    fn serve_should_answer_more_requests_than_the_test_bound() {
        let count = TEST_MAX_REQUESTS + 44;
        let input: String = (0..count)
            .map(|n| format!("{{\"op\":\"capabilities\",\"version\":1,\"requestId\":\"c{n}\"}}\n"))
            .collect();
        let output = Arc::new(Mutex::new(Vec::new()));
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let writer = CappedBuffer(Arc::clone(&output));
        std::thread::spawn(move || {
            let result = serve(
                Path::new(NO_REPO),
                std::io::Cursor::new(input.into_bytes()),
                writer,
            );
            let _ = done_tx.send(result);
        });
        assert_eq!(
            done_rx.recv_timeout(std::time::Duration::from_secs(10)),
            Ok(Ok(()))
        );
        let capabilities = frames(&output)
            .iter()
            .filter(|frame| frame["type"] == "capabilities")
            .count();
        assert_eq!(capabilities, count);
    }

    #[test]
    fn read_bounded_line_should_strip_the_line_ending_and_stop_at_eof() {
        let mut reader = std::io::Cursor::new(b"one\r\ntwo\nthree".to_vec());
        assert_eq!(read_bounded_line(&mut reader, 8), Ok(Some("one".into())));
        assert_eq!(read_bounded_line(&mut reader, 8), Ok(Some("two".into())));
        assert_eq!(read_bounded_line(&mut reader, 8), Ok(Some("three".into())));
        assert_eq!(read_bounded_line(&mut reader, 8), Ok(None));

        let mut reader = std::io::Cursor::new(b"\xff\n".to_vec());
        assert_eq!(
            read_bounded_line(&mut reader, 8),
            Err("evidence stdin: stream did not contain valid UTF-8".into())
        );
    }

    #[test]
    fn a_request_line_over_64_kib_stops_the_bridge_and_one_at_the_cap_does_not() {
        let at_cap = "x".repeat(MAX_REQUEST_LINE);
        let (result, frames) = serve_lines(Path::new(NO_REPO), &[at_cap]);
        result.unwrap();
        assert_eq!(terminal(&frames, "unknown")["status"], "error");

        let over = "x".repeat(MAX_REQUEST_LINE + 1);
        let (result, _) = serve_lines(Path::new(NO_REPO), &[over]);
        assert_eq!(result, Err("evidence request exceeds 64KiB".to_string()));
    }

    /// Records the queue state at the moment the terminal frame is written.
    struct QueueProbe {
        in_flight: Arc<AtomicUsize>,
        active: ActiveRequests,
        seen: Arc<Mutex<Option<(usize, bool)>>>,
    }

    impl Write for QueueProbe {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if String::from_utf8_lossy(bytes).contains("\"complete\"") {
                let busy = self.active.lock().unwrap().contains_key("bundle-1");
                *self.seen.lock().unwrap() = Some((self.in_flight.load(Ordering::Acquire), busy));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_queue_slot_is_free_before_the_client_can_see_complete() {
        let in_flight = Arc::new(AtomicUsize::new(1));
        let active: ActiveRequests = Arc::new(Mutex::new(HashMap::new()));
        let cancelled = Arc::new(AtomicBool::new(false));
        assert!(register_active(&active, "bundle-1", Arc::clone(&cancelled)).unwrap());
        let seen = Arc::new(Mutex::new(None));
        let writer = Arc::new(Mutex::new(QueueProbe {
            in_flight: Arc::clone(&in_flight),
            active: Arc::clone(&active),
            seen: Arc::clone(&seen),
        }));
        let request: Request = serde_json::from_str(&bundle("bundle-1", 0)).unwrap();
        let slot = QueueSlot {
            in_flight: Arc::clone(&in_flight),
            active: Arc::clone(&active),
            request_id: "bundle-1".into(),
        };
        run_bundle(
            &Mutex::new(None),
            Path::new(NO_REPO),
            &writer,
            slot,
            request,
            &cancelled,
            Instant::now(),
        );
        assert_eq!(
            *seen.lock().unwrap(),
            Some((0, false)),
            "slot and requestId released before `complete` is written"
        );
    }

    #[test]
    fn a_panicking_worker_gives_its_queue_slot_back() {
        let in_flight = Arc::new(AtomicUsize::new(1));
        let active: ActiveRequests = Arc::new(Mutex::new(HashMap::new()));
        assert!(register_active(&active, "bundle-1", Arc::new(AtomicBool::new(false))).unwrap());
        let slot = QueueSlot {
            in_flight: Arc::clone(&in_flight),
            active: Arc::clone(&active),
            request_id: "bundle-1".into(),
        };
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _slot = slot;
            panic!("worker bug");
        }));
        assert!(unwound.is_err());
        assert_eq!(in_flight.load(Ordering::Acquire), 0);
        assert!(active.lock().unwrap().is_empty());
    }

    fn dirty(entries: &[(&str, &str, u64)]) -> TreeState {
        TreeState {
            head: Some("h1".into()),
            dirty: entries
                .iter()
                .map(|(path, xy, len)| ((*path).to_string(), ((*xy).to_string(), Some((*len, 7)))))
                .collect(),
        }
    }

    #[test]
    fn stale_paths_are_the_commit_range_plus_every_dirty_entry_that_moved() {
        let before = dirty(&[
            ("kept.rs", " M", 1),
            ("edited.rs", " M", 1),
            ("reverted.rs", " M", 1),
        ]);
        let after = dirty(&[
            ("kept.rs", " M", 1),
            ("edited.rs", " M", 2),
            ("new.rs", "??", 1),
        ]);
        assert_eq!(
            stale_paths(&before, &after, &[('M', "committed.rs".into())]),
            ["committed.rs", "edited.rs", "new.rs", "reverted.rs"],
            "an unchanged dirty entry is not refreshed again"
        );
        assert!(stale_paths(&after, &after, &[]).is_empty());
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// Wait (bounded) until `request_id`'s terminal frame is in `output`.
    fn await_complete(output: &Arc<Mutex<Vec<u8>>>, request_id: &str) -> Vec<Value> {
        let deadline = Instant::now() + std::time::Duration::from_secs(15);
        while Instant::now() < deadline {
            let frames = frames(output);
            if frames
                .iter()
                .any(|frame| frame["requestId"] == request_id && frame["type"] == "complete")
            {
                return frames;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("{request_id} never completed: {:?}", frames(output));
    }

    #[test]
    fn a_bundle_sees_a_file_written_after_the_previous_bundle() {
        let root =
            std::env::temp_dir().join(format!("pixel-evidence-fresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        std::fs::write(root.join("a.rs"), "fn early() {}\n").unwrap();
        git(&root, &["add", "a.rs"]);
        git(&root, &["commit", "-qm", "init"]);

        let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let bridge = std::thread::spawn({
            let root = root.clone();
            let output = Arc::clone(&output);
            move || {
                serve_bounded(
                    &root,
                    std::io::BufReader::new(server),
                    SharedBuffer(output),
                    TEST_MAX_REQUESTS,
                )
            }
        });
        let mut client = client;
        let search = |id: &str| {
            json!({"op": "bundle", "version": 1, "requestId": id,
                   "queries": [{"id": "q", "kind": "search", "query": "late_marker"}]})
            .to_string()
        };
        writeln!(client, "{}", search("before")).unwrap();
        let first = await_complete(&output, "before");
        std::fs::write(root.join("late.rs"), "fn late_marker() {}\n").unwrap();
        writeln!(client, "{}", search("after")).unwrap();
        let second = await_complete(&output, "after");
        drop(client);
        bridge.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&root);

        let partial = |frames: &[Value], id: &str| {
            frames
                .iter()
                .find(|frame| frame["requestId"] == id && frame["type"] == "partial")
                .cloned()
                .unwrap_or_else(|| panic!("no partial for {id}: {frames:?}"))
        };
        assert!(!partial(&first, "before").to_string().contains("late.rs"));
        assert!(
            partial(&second, "after").to_string().contains("late.rs"),
            "the edit made between two bundles is searchable: {second:?}"
        );
    }

    #[test]
    fn completed_workers_are_reaped_before_accepting_more_requests() {
        let mut workers = vec![std::thread::spawn(|| {})];
        while !workers[0].is_finished() {
            std::thread::yield_now();
        }
        reap_finished_workers(&mut workers);
        assert!(workers.is_empty());
    }

    #[test]
    fn modified_nanos_reads_the_file_mtime_since_the_epoch() {
        let path =
            std::env::temp_dir().join(format!("pixel-evidence-mtime-{}", std::process::id()));
        std::fs::write(&path, b"x").unwrap();
        let when = std::time::Duration::new(1_000_000, 5_000);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + when)
            .unwrap();
        let meta = std::fs::symlink_metadata(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(modified_nanos(&meta), when.as_nanos());
    }
}
