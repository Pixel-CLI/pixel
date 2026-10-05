// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Contract tests for the daemon transport: what one connection answers
//! for each kind of line a client can send (empty, malformed, not UTF-8,
//! too long, past the request limit, a shutdown), the default `Corpus`
//! callbacks, and what the repository `Service` does with a watcher change.

use super::*;

use std::sync::Mutex;

/// A corpus that answers every request with a success naming its op, and
/// records the changes the default `apply_changes` hands to `apply_change`.
struct EchoCorpus {
    root: PathBuf,
    applied: Arc<Mutex<Vec<(PathBuf, bool)>>>,
}

impl EchoCorpus {
    fn new() -> Self {
        EchoCorpus {
            root: PathBuf::from("/nonexistent-echo-root"),
            applied: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl Corpus for EchoCorpus {
    fn root(&self) -> &Path {
        &self.root
    }
    fn handle(&mut self, req: Request) -> Response {
        Response::success(req.op_name(), serde_json::json!({"echo": req.op_name()}))
    }
    fn apply_change(&mut self, abs: &Path, removed: bool) {
        self.applied
            .lock()
            .unwrap()
            .push((abs.to_path_buf(), removed));
    }
}

fn ping_line() -> String {
    serde_json::to_string(&Request::Ping).unwrap()
}

/// Run `handle_conn` over a socket pair: the client writes `input`, closes
/// its write half, and reads every reply line until the daemon closes.
/// Returns the parsed replies and the shutdown flag the connection set.
fn converse(input: Vec<u8>) -> (Vec<serde_json::Value>, bool) {
    let (server, client) = UnixStream::pair().unwrap();
    let reader = std::thread::spawn(move || {
        let mut client = client;
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        // The daemon may close mid-input (an oversized line ends the
        // connection at byte 65 537): a failed write is then the expected
        // peer close, and the replies sent before it are still read below.
        let _ = client.write_all(&input);
        let _ = client.shutdown(std::net::Shutdown::Write);
        BufReader::new(&client)
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(&line.unwrap()).unwrap())
            .collect::<Vec<_>>()
    });
    let mut corpus = EchoCorpus::new();
    let mut shutdown = false;
    handle_conn(&mut corpus, server, &mut shutdown);
    (reader.join().unwrap(), shutdown)
}

fn error_message(reply: &serde_json::Value) -> &str {
    reply["error"]["message"].as_str().unwrap_or_default()
}

#[test]
fn handle_conn_should_answer_each_request_line_and_skip_empty_ones() {
    let input = format!("\n{}\n   \n{}\n", ping_line(), ping_line());
    let (replies, shutdown) = converse(input.into_bytes());
    assert_eq!(
        replies.len(),
        2,
        "one reply per request, none for blank lines"
    );
    for reply in &replies {
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["result"]["echo"], "ping");
    }
    assert!(!shutdown, "a closed connection is not a shutdown");
}

#[test]
fn handle_conn_should_answer_a_malformed_request_and_keep_serving_the_connection() {
    let input = format!("{{\"op\":\"no_such_op\"}}\n{}\n", ping_line());
    let (replies, _) = converse(input.into_bytes());
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0]["ok"], false);
    assert_eq!(replies[0]["op"], "error");
    assert!(
        error_message(&replies[0]).starts_with("bad request: "),
        "{}",
        replies[0]
    );
    assert_eq!(
        replies[1]["result"]["echo"], "ping",
        "the next line is served"
    );
}

#[test]
fn handle_conn_should_reject_a_non_utf8_line_and_keep_serving_the_connection() {
    let mut input = vec![0xff, 0xfe, b'\n'];
    input.extend(format!("{}\n", ping_line()).into_bytes());
    let (replies, _) = converse(input);
    assert_eq!(replies.len(), 2);
    assert_eq!(error_message(&replies[0]), "request is not valid UTF-8");
    assert_eq!(replies[1]["result"]["echo"], "ping");
}

#[test]
fn handle_conn_should_refuse_an_oversized_line_and_close_the_connection() {
    let mut input = vec![b'a'; MAX_REQUEST_LINE + 1];
    input.push(b'\n');
    input.extend(format!("{}\n", ping_line()).into_bytes());
    let (replies, shutdown) = converse(input);
    assert_eq!(replies.len(), 1, "nothing after the oversized line is read");
    assert_eq!(error_message(&replies[0]), "request line too long");
    assert!(!shutdown);
}

#[test]
fn handle_conn_should_accept_a_line_exactly_at_the_cap() {
    // A JSON string padded with spaces to exactly MAX_REQUEST_LINE bytes:
    // the cap is on lines *over* it.
    let ping = ping_line();
    let mut line = ping.clone();
    line.push_str(&" ".repeat(MAX_REQUEST_LINE - ping.len()));
    assert_eq!(line.len(), MAX_REQUEST_LINE);
    let (replies, _) = converse(format!("{line}\n").into_bytes());
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["result"]["echo"], "ping");
}

#[test]
fn handle_conn_should_stop_after_the_request_limit_with_a_named_error() {
    let lines: String = (0..=MAX_REQUESTS_PER_CONN)
        .map(|_| format!("{}\n", ping_line()))
        .collect();
    let (replies, _) = converse(lines.into_bytes());
    let limit = MAX_REQUESTS_PER_CONN as usize;
    assert_eq!(replies.len(), limit + 1);
    assert!(
        replies[..limit]
            .iter()
            .all(|r| r["result"]["echo"] == "ping"),
        "every request up to the limit is answered"
    );
    assert_eq!(error_message(&replies[limit]), "request limit exceeded");
}

#[test]
fn handle_conn_should_count_an_invalid_utf8_line_against_the_request_limit() {
    let mut input = vec![0xff, b'\n'];
    for _ in 0..MAX_REQUESTS_PER_CONN {
        input.extend(format!("{}\n", ping_line()).into_bytes());
    }
    let (replies, _) = converse(input);
    let limit = MAX_REQUESTS_PER_CONN as usize;
    assert_eq!(replies.len(), limit + 1);
    assert_eq!(
        error_message(&replies[limit]),
        "request limit exceeded",
        "the rejected line used one of the {limit} requests"
    );
}

#[test]
fn handle_conn_should_set_the_shutdown_flag_and_stop_reading_after_a_shutdown() {
    let input = format!(
        "{}\n{}\n",
        serde_json::to_string(&Request::Shutdown).unwrap(),
        ping_line()
    );
    let (replies, shutdown) = converse(input.into_bytes());
    assert!(shutdown, "the loop must learn it has to exit");
    assert_eq!(
        replies.len(),
        1,
        "the ping after the shutdown is not served"
    );
    assert_eq!(replies[0]["result"]["echo"], "shutdown");
}

#[test]
fn handle_conn_should_return_without_a_reply_when_the_client_sends_nothing() {
    let (replies, shutdown) = converse(Vec::new());
    assert!(replies.is_empty());
    assert!(!shutdown);
}

#[test]
fn probe_ping_should_be_false_when_the_peer_is_gone() {
    let (server, mut client) = UnixStream::pair().unwrap();
    drop(server);
    assert!(
        !probe_ping(&mut client),
        "a write to a closed socket is no daemon"
    );
}

#[test]
fn probe_ping_should_be_false_when_the_peer_closes_without_answering() {
    let (server, mut client) = UnixStream::pair().unwrap();
    let reader = std::thread::spawn(move || {
        // Bounded: a probe that never writes must fail the assertion below,
        // not hang the suite on this read.
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut line = String::new();
        let _ = BufReader::new(&server).read_line(&mut line);
        // Read the ping, then hang up without a reply.
        line
    });
    assert!(!probe_ping(&mut client));
    assert!(reader.join().unwrap().contains("ping"));
}

#[test]
fn default_apply_changes_should_hand_each_change_to_apply_change_in_order() {
    let mut corpus = EchoCorpus::new();
    let changes = vec![
        (PathBuf::from("/r/a.rs"), false),
        (PathBuf::from("/r/b.rs"), true),
    ];
    corpus.apply_changes(&changes);
    assert_eq!(*corpus.applied.lock().unwrap(), changes);
}

#[test]
fn default_corpus_should_watch_its_root_and_do_nothing_on_sweep_or_ready() {
    let mut corpus = EchoCorpus::new();
    assert_eq!(corpus.watch_paths(), vec![corpus.root.clone()]);
    corpus.sweep();
    corpus.watch_ready();
    assert!(corpus.applied.lock().unwrap().is_empty());
}

#[test]
fn root_removed_should_be_true_only_once_the_root_is_no_directory() {
    let root = tempfile_root("root-removed");
    assert!(!root_removed(&root));
    let file = root.join("plain");
    std::fs::write(&file, "x").unwrap();
    assert!(root_removed(&file), "a file is not a servable root");
    std::fs::remove_dir_all(&root).unwrap();
    assert!(root_removed(&root));
}

#[test]
fn record_event_should_skip_directories_and_mark_a_vanished_path_removed() {
    use notify::event::{DataChange, ModifyKind};
    let root = tempfile_root("record-dir");
    let dir = root.join("src");
    std::fs::create_dir_all(&dir).unwrap();
    let gone = root.join("gone.rs");
    let event = notify::Event::new(notify::EventKind::Modify(ModifyKind::Data(DataChange::Any)))
        .add_path(dir.clone())
        .add_path(gone.clone());
    let mut pending = BTreeMap::new();
    record_event(&root, &event, &mut pending);
    assert!(
        !pending.contains_key(&dir),
        "a directory has no content of its own"
    );
    assert_eq!(
        pending.get(&gone),
        Some(&true),
        "a modify for a path that no longer exists is a removal"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn flush_pending_should_apply_nothing_and_keep_the_deadline_when_nothing_is_pending() {
    let mut corpus = EchoCorpus::new();
    let mut pending = BTreeMap::new();
    let deadline = Some(Instant::now());
    let mut flush_at = deadline;
    flush_pending(&mut corpus, &mut pending, &mut flush_at);
    assert!(corpus.applied.lock().unwrap().is_empty());
    assert_eq!(flush_at, deadline);
}

fn tempfile_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "pixel-daemon-contract-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    root.canonicalize().unwrap()
}

// -- the repository Service as a corpus -------------------------------------------

fn search_hits(service: &mut Service, pattern: &str) -> Vec<String> {
    let resp = Corpus::handle(
        service,
        Request::Search {
            pattern: pattern.into(),
            json: true,
            limit: None,
            offset: None,
            paths: None,
            scope: None,
            globs: Vec::new(),
            types: Vec::new(),
        },
    );
    let value = serde_json::to_value(&resp).unwrap();
    assert_eq!(value["ok"], true, "{value}");
    let mut paths: Vec<String> = value["result"]["matches"]
        .as_array()
        .unwrap_or_else(|| panic!("matches in {value}"))
        .iter()
        .filter_map(|m| m["path"].as_str().map(String::from))
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

#[test]
fn service_apply_change_should_index_an_added_file_and_drop_a_removed_one() {
    let root = tempfile_root("service-apply");
    std::fs::write(root.join("a.rs"), "fn zorblax_one() {}\n").unwrap();
    let mut service = Service::open(&root).unwrap();
    assert_eq!(search_hits(&mut service, "zorblax"), vec!["a.rs"]);

    std::fs::write(root.join("b.rs"), "fn zorblax_two() {}\n").unwrap();
    service.apply_change(&root.join("b.rs"), false);
    assert_eq!(search_hits(&mut service, "zorblax"), vec!["a.rs", "b.rs"]);

    std::fs::remove_file(root.join("a.rs")).unwrap();
    service.apply_change(&root.join("a.rs"), true);
    assert_eq!(search_hits(&mut service, "zorblax"), vec!["b.rs"]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn service_apply_change_should_ignore_paths_outside_the_root_and_the_root_itself() {
    let root = tempfile_root("service-outside");
    std::fs::write(root.join("a.rs"), "fn quixel_one() {}\n").unwrap();
    let outside = tempfile_root("service-outside-other");
    std::fs::write(outside.join("o.rs"), "fn quixel_two() {}\n").unwrap();
    let mut service = Service::open(&root).unwrap();

    service.apply_change(&outside.join("o.rs"), false);
    service.apply_change(&root, true);
    service.apply_changes(&[(outside.join("o.rs"), false), (root.clone(), true)]);

    assert_eq!(
        search_hits(&mut service, "quixel"),
        vec!["a.rs"],
        "neither a foreign file nor the root itself changes the index"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}

#[test]
fn service_apply_changes_should_apply_a_batch_of_adds_and_removals() {
    let root = tempfile_root("service-batch");
    std::fs::write(root.join("a.rs"), "fn vexlor_one() {}\n").unwrap();
    let mut service = Service::open(&root).unwrap();
    std::fs::remove_file(root.join("a.rs")).unwrap();
    std::fs::write(root.join("b.rs"), "fn vexlor_two() {}\n").unwrap();

    service.apply_changes(&[(root.join("a.rs"), true), (root.join("b.rs"), false)]);

    assert_eq!(search_hits(&mut service, "vexlor"), vec!["b.rs"]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn service_watcher_error_should_be_counted_in_status() {
    let root = tempfile_root("service-watcher-error");
    let mut service = Service::open(&root).unwrap();
    service.watcher_error("queue overflow");
    service.watcher_error("queue overflow");
    let resp = Corpus::handle(&mut service, Request::Status {});
    let value = serde_json::to_value(&resp).unwrap();
    assert_eq!(value["result"]["watcher"]["notify_errors"], 2, "{value}");
    let _ = std::fs::remove_dir_all(&root);
}
