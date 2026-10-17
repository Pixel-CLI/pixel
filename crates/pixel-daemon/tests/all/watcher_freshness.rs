// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Integration: a file changed while a request is being served is visible to
//! the next request.
//!
//! Why it matters: the loop used to apply the debounced watcher batch only
//! after `handle_conn` returned, so a request that arrived while a slow
//! request held the loop (a `sync-branch` runs several git commands of up to
//! 120 s on this same thread) was answered from the index built before the
//! change: the caller that wrote the file and immediately asked about it got
//! a stale answer. The pending batch is now drained before the connection is
//! served. Since #1150 a read-plane request (`search`, `targets`, …) that
//! arrives while the writer is busy is answered by a read replica, unless a
//! watcher batch is queued ahead of it: the last two tests pin both halves.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use pixel_daemon::daemon::{Corpus, ReadReplica, WatchReport, run_corpus, socket_path};
use pixel_daemon::{Request, Response, Service};

use crate::support::git;

/// Time the watcher backend gets to deliver the file event before the probe
/// connects. FSEvents is created with zero latency and inotify is immediate,
/// so this is a margin, not a poll interval.
const WATCHER_SETTLE: Duration = Duration::from_millis(1500);

/// One committed source file defining `helper`.
fn fixture(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "gpx-watcher-freshness-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/util.ts"),
        "export function helper(x: number): number { return x + 1 }\n",
    )
    .unwrap();
    std::fs::write(dir.join(".gitignore"), ".pixel/\n").unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "baseline"]);
    dir
}

/// One client connection: writes requests, reads one NDJSON reply per request.
struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Client {
    fn connect(sock: &Path) -> Self {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match UnixStream::connect(sock) {
                Ok(stream) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(30)))
                        .unwrap();
                    let reader = BufReader::new(stream.try_clone().unwrap());
                    return Client { stream, reader };
                }
                Err(e) => {
                    assert!(
                        Instant::now() < deadline,
                        "daemon socket {} never accepted: {e}",
                        sock.display()
                    );
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }

    fn send(&mut self, req: &Request) {
        let mut line = serde_json::to_string(req).unwrap();
        line.push('\n');
        self.stream.write_all(line.as_bytes()).unwrap();
    }

    fn receive(&mut self) -> Response {
        let mut reply = String::new();
        self.reader
            .read_line(&mut reply)
            .expect("daemon answers within 30s");
        assert!(
            !reply.is_empty(),
            "daemon closed the connection without answering"
        );
        serde_json::from_str(&reply).expect("one response envelope")
    }

    fn request(&mut self, req: &Request) -> Response {
        self.send(req);
        self.receive()
    }
}

/// Wraps the repo `Service` and holds every `Status` request until the test
/// releases it, standing in for a long mutation on the daemon's single
/// thread: the loop is provably busy while the test writes the file and
/// queues the next connection.
struct GatedCorpus {
    service: Service,
    started: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
    ready: mpsc::Sender<Result<(), String>>,
}

impl Corpus for GatedCorpus {
    fn root(&self) -> &Path {
        self.service.root()
    }

    fn handle(&mut self, req: Request) -> Response {
        if matches!(req, Request::Status {}) {
            self.started.send(()).unwrap();
            self.release
                .recv_timeout(Duration::from_secs(30))
                .expect("test releases the held request");
        }
        self.service.handle(req)
    }

    fn apply_change(&mut self, abs: &Path, removed: bool) {
        self.service.apply_change(abs, removed);
    }

    fn pong(&self) -> Option<Response> {
        self.service.pong()
    }

    fn read_replica(&self) -> Option<Box<dyn ReadReplica>> {
        self.service.read_replica()
    }

    fn apply_changes(&mut self, changes: &[(PathBuf, bool)]) {
        self.service.apply_changes(changes);
    }

    // The watch registers on its own thread, so the edit below can land
    // before it is live; the wrapped `Service` catches up only if the
    // wrapper forwards the callback.
    fn watch_ready(&mut self, report: &WatchReport) {
        self.service.watch_ready(report);
        let _ = self.ready.send(Ok(()));
    }

    fn watcher_error(&mut self, error: &str) {
        self.service.watcher_error(error);
        let _ = self.ready.send(Err(error.to_owned()));
    }
}

#[test]
fn a_file_changed_during_a_held_request_is_visible_to_the_next_request() {
    let dir = fixture("held");
    let sock = socket_path(&dir);
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let served = dir.clone();
    let daemon = std::thread::spawn(move || {
        // Built on the daemon's own thread: `Service` is not `Send` (it
        // holds a `Box<dyn Embedder>`).
        let service = Service::open(&served).expect("service opens");
        run_corpus(GatedCorpus {
            service,
            started: started_tx,
            release: release_rx,
            ready: ready_tx,
        })
    });

    // The daemon accepts requests before its watcher thread registers. Wait
    // for registration so the write below is guaranteed to occur under watch.
    match ready_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => panic!("watcher failed before becoming ready: {error}"),
        Err(error) => panic!("watcher did not become ready: {error}"),
    }

    // Warm the daemon's cached graph handle: the stale answer this test
    // rules out is the *warm* store, which `ensure_graph` would otherwise
    // reload only because a watcher event dropped it.
    let mut warm = Client::connect(&sock);
    let warm = warm.request(&Request::Symbol {
        name: "helper".into(),
    });
    assert!(warm.ok, "{warm:?}");
    assert!(
        !warm.data()["symbols"].as_array().unwrap().is_empty(),
        "fixture symbol must be indexed before the held request: {warm:?}"
    );

    // Connection A: the daemon holds this request on the loop's only thread.
    let mut held = Client::connect(&sock);
    held.send(&Request::Status {});
    started_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("daemon takes the held request");

    // The edit happens while the loop is busy. Waiting before connecting B
    // is what puts the watcher event in the queue ahead of B's connection,
    // which is the order a real client produces (write, then ask).
    std::fs::write(
        dir.join("src/util.ts"),
        "export function helper(x: number): number { return x + 1 }\n\
         export function fresh_probe(): number { return 42 }\n",
    )
    .unwrap();
    std::thread::sleep(WATCHER_SETTLE);
    let mut probe = Client::connect(&sock);
    probe.send(&Request::Symbol {
        name: "fresh_probe".into(),
    });

    release_tx.send(()).unwrap();
    let held_reply = held.receive();
    assert!(held_reply.ok, "{held_reply:?}");

    let probe_reply = probe.receive();
    assert!(probe_reply.ok, "{probe_reply:?}");
    assert!(
        !probe_reply.data()["symbols"].as_array().unwrap().is_empty(),
        "the file changed during the held request must be visible to the \
         first request after it: {probe_reply:?}"
    );

    let mut bye = Client::connect(&sock);
    let bye_reply = bye.request(&Request::Shutdown);
    assert!(bye_reply.ok, "{bye_reply:?}");
    daemon.join().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A daemon over `fixture(tag)` serving a `GatedCorpus`, its watch live and
/// its graph built (a replica reads the graph, it never builds it).
struct Gated {
    dir: PathBuf,
    sock: PathBuf,
    started: mpsc::Receiver<()>,
    release: mpsc::Sender<()>,
    daemon: std::thread::JoinHandle<Result<(), pixel_daemon::ServeError>>,
}

impl Gated {
    fn start(tag: &str) -> Self {
        let dir = fixture(tag);
        let sock = socket_path(&dir);
        let (started_tx, started) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let served = dir.clone();
        let daemon = std::thread::spawn(move || {
            let service = Service::open(&served).expect("service opens");
            run_corpus(GatedCorpus {
                service,
                started: started_tx,
                release: release_rx,
                ready: ready_tx,
            })
        });
        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => panic!("watcher failed before becoming ready: {error}"),
            Err(error) => panic!("watcher did not become ready: {error}"),
        }
        let built = Client::connect(&sock).request(&Request::Symbol {
            name: "helper".into(),
        });
        assert!(built.ok, "{built:?}");
        Gated {
            dir,
            sock,
            started,
            release,
            daemon,
        }
    }

    /// Send a `status` the writer holds until [`Gated::finish`].
    fn hold(&self) -> Client {
        let mut held = Client::connect(&self.sock);
        held.send(&Request::Status {});
        self.started
            .recv_timeout(Duration::from_secs(30))
            .expect("the writer takes the held request");
        held
    }

    fn finish(self, mut held: Client) {
        self.release.send(()).unwrap();
        let held_reply = held.receive();
        assert!(held_reply.ok, "{held_reply:?}");
        let bye = Client::connect(&self.sock).request(&Request::Shutdown);
        assert!(bye.ok, "{bye:?}");
        self.daemon.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn search(pattern: &str) -> Request {
    Request::Search {
        pattern: pattern.into(),
        json: false,
        limit: None,
        offset: None,
        paths: None,
        scope: None,
        globs: Vec::new(),
        types: Vec::new(),
    }
}

/// A `ping` and a `search` sent while the writer holds a long request are
/// answered before it ends: the search by a read replica, from the
/// published index (#1150).
#[test]
fn reads_should_be_answered_while_the_writer_holds_a_request() {
    let gated = Gated::start("replica-read");
    let held = gated.hold();

    let pong = Client::connect(&gated.sock).request(&Request::Ping);
    assert!(pong.ok, "{pong:?}");
    let started = Instant::now();
    let found = Client::connect(&gated.sock).request(&search("helper"));
    assert!(found.ok, "{found:?}");
    assert!(
        found.data().to_string().contains("src/util.ts"),
        "the replica answers from the published index: {found:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the search waited for the held request"
    );
    gated.finish(held);
}

/// A file changed while the writer holds a request is visible to the next
/// read: the watcher batch is queued ahead of the read, so the read waits
/// for the writer instead of taking a replica's older index.
#[test]
fn a_read_after_an_edit_should_wait_for_the_batch_not_take_a_replica() {
    let gated = Gated::start("replica-batch");
    let held = gated.hold();
    std::fs::write(
        gated.dir.join("src/util.ts"),
        "export function helper(x: number): number { return x + 1 }\n\
         export function fresh_probe(): number { return 42 }\n",
    )
    .unwrap();
    std::thread::sleep(WATCHER_SETTLE);
    let mut probe = Client::connect(&gated.sock);
    probe.send(&search("fresh_probe"));
    std::thread::sleep(Duration::from_millis(300));
    gated.release.send(()).unwrap();
    let found = probe.receive();
    assert!(found.ok, "{found:?}");
    assert!(
        found.data().to_string().contains("fresh_probe"),
        "the edit made during the held request is visible: {found:?}"
    );
    // `finish` releases once more; the second `status` is never sent, so
    // hand it the held client after its answer is in.
    let mut held = held;
    let held_reply = held.receive();
    assert!(held_reply.ok, "{held_reply:?}");
    let bye = Client::connect(&gated.sock).request(&Request::Shutdown);
    assert!(bye.ok, "{bye:?}");
    gated.daemon.join().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&gated.dir);
}
