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
//! served. The single thread remains the known limit for *latency* (a long
//! mutation still delays every other request, see `daemon.rs`'s module
//! comment); this test pins the ordering, not a concurrency guarantee.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use pixel_daemon::daemon::{Corpus, run_corpus, socket_path};
use pixel_daemon::{Request, Response, Service};

/// Time the watcher backend gets to deliver the file event before the probe
/// connects. FSEvents is created with zero latency and inotify is immediate,
/// so this is a margin, not a poll interval.
const WATCHER_SETTLE: Duration = Duration::from_millis(1500);

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

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

    fn apply_changes(&mut self, changes: &[(PathBuf, bool)]) {
        self.service.apply_changes(changes);
    }

    // The watch registers on its own thread, so the edit below can land
    // before it is live; the wrapped `Service` catches up only if the
    // wrapper forwards the callback.
    fn watch_ready(&mut self) {
        self.service.watch_ready();
    }

    fn watcher_error(&mut self, error: &str) {
        self.service.watcher_error(error);
    }
}

#[test]
fn a_file_changed_during_a_held_request_is_visible_to_the_next_request() {
    let dir = fixture("held");
    let sock = socket_path(&dir);
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let served = dir.clone();
    let daemon = std::thread::spawn(move || {
        // Built on the daemon's own thread: `Service` is not `Send` (it
        // holds a `Box<dyn Embedder>`).
        let service = Service::open(&served).expect("service opens");
        run_corpus(GatedCorpus {
            service,
            started: started_tx,
            release: release_rx,
        })
    });

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
