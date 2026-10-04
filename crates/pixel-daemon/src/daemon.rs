// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Unix-socket NDJSON daemon: one JSON `Request` per line, one JSON
//! `Response` line back. Single-threaded request handling: an accept thread
//! blocked on the listener and a notify watcher feed one mpsc channel, and
//! the event loop sleeps on that channel alone, so a long mutation (a
//! `sync-branch` is several git commands of up to 120 s each) delays every
//! other request on this root. The loop therefore drains the debounced
//! watcher batch before it serves a connection: a request following a
//! mutation never reads an index built before it. A worker thread owning the
//! `Service` behind a `Mutex` would keep the queue moving during a mutation;
//! until then the queue waits and the answers stay ordered and fresh.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use fs2::FileExt;
use notify::{RecursiveMode, Watcher};

use crate::api::{Request, Response, ServeError, Service, failure_response};

const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const DEBOUNCE: Duration = Duration::from_millis(500);
/// Longest the main loop sleeps before re-checking that the served root
/// still exists. A daemon whose root was deleted (a removed worktree, a
/// test fixture) has nothing left to serve and must not sit on the machine
/// for the rest of `IDLE_TIMEOUT`: auto-started daemons are detached from
/// their parent, so nothing else would ever reap them.
const ROOT_POLL: Duration = Duration::from_secs(5);
/// Idle poll interval for the facts ingest thread once fresh. A ref move
/// re-triggers ingest on the next poll without blocking queries.
const INGEST_IDLE_POLL: Duration = Duration::from_secs(5);
/// Backoff after a transient ingest error (e.g. a git lock held by another
/// process) before retrying.
const INGEST_ERROR_BACKOFF: Duration = Duration::from_secs(2);
const IGNORED_DIRS: &[&str] = &[
    ".pixel",
    ".git",
    "target",
    "node_modules",
    "bower_components",
    "Pods",
    "vendor",
    "_build",
    "DerivedData",
    "dist",
    "build",
    "out",
    ".gradle",
    "__pycache__",
    ".venv",
    "venv",
    ".tox",
    "site-packages",
    ".terraform",
    ".next",
    ".nuxt",
    ".turbo",
    ".cache",
    ".npm",
    ".yarn",
    ".pnpm-store",
];
/// Maximum length of a single NDJSON request line. A request larger than this
/// is rejected to prevent a malicious client from exhausting memory with a
/// multi-GB line. The largest legitimate request (a search pattern) is well
/// under 1 KiB.
const MAX_REQUEST_LINE: usize = 64 * 1024;
const CONNECTION_DEADLINE: Duration = Duration::from_secs(5);
/// Maximum number of requests served on a single connection before it is
/// closed. Prevents a single long-lived client from monopolizing the
/// single-threaded daemon indefinitely.
const MAX_REQUESTS_PER_CONN: u32 = 64;

/// $TMPDIR/pixel-<xxh3-of-canonical-root>.sock
///
/// On Linux, `TMPDIR` defaults to world-writable `/tmp`, which allows any
/// local user to predict the socket path and squat on it before the daemon
/// binds. We prefer `XDG_RUNTIME_DIR` (per-user, 0700, tmpfs) when available,
/// falling back to `TMPDIR` only on macOS (where `TMPDIR` is already per-user
/// 0700). On Linux without `XDG_RUNTIME_DIR`, we use `~/.cache/pixel/sockets/`
/// created with 0700 permissions.
///
/// Deliberately distinct from the legacy gitpixel tool's `gitpixel-*.sock`
/// prefix: the two daemons speak incompatible response envelopes (gitpixel's
/// `{ok,error,data}` vs pixel's `{op,protocol,...}`), so an old gitpixel
/// daemon and this one must bind to different socket paths and coexist
/// independently rather than collide on the same one.
pub fn socket_path(root: &Path) -> PathBuf {
    let canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let h = xxhash_rust::xxh3::xxh3_64(canon.to_string_lossy().as_bytes());
    let sock_name = format!("pixel-{h:016x}.sock");
    runtime_dir().join(sock_name)
}

/// Return a per-user directory for socket files. On macOS, `TMPDIR` is
/// already per-user with 0700 permissions. On Linux, prefer
/// `XDG_RUNTIME_DIR`; if unset, create `~/.cache/pixel/sockets/` with 0700.
fn runtime_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        std::env::temp_dir()
    }
    #[cfg(not(target_os = "macos"))]
    {
        if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR")
            && !dir.is_empty()
            && std::path::Path::new(&dir).exists()
        {
            return PathBuf::from(dir);
        }
        // Fallback: ~/.cache/pixel/sockets/ with 0700 perms.
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let dir = PathBuf::from(home).join(".cache/pixel/sockets");
        let _ = std::fs::create_dir_all(&dir);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        dir
    }
}

pub fn pid_path(root: &Path) -> PathBuf {
    socket_path(root).with_extension("pid")
}

/// How long the [`ping_only`] probe waits for a connect and a reply. A warm
/// daemon answers a `Ping` immediately; a socket that stays silent past this
/// is not the fast path the caller was looking for.
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// Probe a daemon without ever starting one: connect to [`socket_path`] and
/// send one `Ping`, `true` only when the daemon answers `ok`.
///
/// Deliberately not the CLI's auto-starting `try_daemon`: any root it is
/// given gets a repository `Service` spawned for it, which is wrong for the
/// machine-wide corpus (`recall_dir()`), where only the recall daemon may
/// serve and `Service::open` leaves `.pixel/` artifacts inside the corpus.
/// A caller that wants a daemon running starts it explicitly.
pub fn ping_only(root: &Path) -> bool {
    let Ok(mut stream) = UnixStream::connect(socket_path(root)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(PROBE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(PROBE_TIMEOUT));
    probe_ping(&mut stream)
}

/// One `Ping` round trip on an already-connected stream: `true` only for an
/// `ok` reply. Split from [`ping_only`] so tests drive the framing over a
/// socket pair instead of a real daemon.
fn probe_ping(stream: &mut UnixStream) -> bool {
    // A unit variant: serialization cannot fail.
    let mut line = serde_json::to_string(&Request::Ping).expect("Request::Ping serializes");
    line.push('\n');
    if stream.write_all(line.as_bytes()).is_err() {
        return false;
    }
    let mut reply = String::new();
    if BufReader::new(stream).read_line(&mut reply).is_err() {
        return false;
    }
    serde_json::from_str::<Response>(&reply).is_ok_and(|r| r.ok)
}

enum Msg {
    /// A connection the accept thread took off the listener.
    Conn(UnixStream),
    /// The listener failed; the loop exits as it did on an accept error.
    AcceptFailed,
    Fs(notify::Event),
    /// A `notify` callback error, forwarded to the loop (the single owner of
    /// the corpus) so a watch that stopped reporting is counted and logged
    /// instead of dropped on the callback thread.
    WatcherError(String),
}

/// A corpus a daemon can serve: the repo `Service`, or the machine-wide
/// transcript recall service. The transport (socket, watcher, debounce,
/// framing) is identical for every corpus.
pub trait Corpus {
    /// Root that keys the socket path and is watched by default.
    fn root(&self) -> &Path;
    fn handle(&mut self, req: Request) -> Response;
    /// Debounced watcher callback with the absolute changed path.
    fn apply_change(&mut self, abs: &Path, removed: bool);
    /// Debounced watcher callback with a batch of changed paths (default: loops apply_change).
    fn apply_changes(&mut self, changes: &[(PathBuf, bool)]) {
        for (abs, removed) in changes {
            self.apply_change(abs, *removed);
        }
    }
    /// Directories the watcher observes (default: the root).
    fn watch_paths(&self) -> Vec<PathBuf> {
        vec![self.root().to_path_buf()]
    }
    /// How often the loop calls `sweep` regardless of watcher events
    /// (default: never). A corpus whose writers hold files open for
    /// minutes (streamed transcripts) sets this, because FSEvents on macOS
    /// defers the modify event until the writer closes the file.
    fn sweep_interval(&self) -> Option<Duration> {
        None
    }
    /// Periodic maintenance, called every `sweep_interval` from the loop
    /// thread (default: nothing).
    fn sweep(&mut self) {}
    /// The watcher backend reported an error: changes may have been missed,
    /// so answers can be stale until the next event. The default logs it; a
    /// corpus that reports health counts it too.
    #[cfg_attr(test, mutants::skip)] // stderr diagnostics only
    fn watcher_error(&mut self, error: &str) {
        eprintln!("pixel daemon: watcher error: {error}");
    }
}

impl Corpus for Service {
    fn root(&self) -> &Path {
        Service::root(self)
    }

    fn handle(&mut self, req: Request) -> Response {
        Service::handle(self, req)
    }

    fn apply_change(&mut self, abs: &Path, removed: bool) {
        let root = Service::root(self).to_path_buf();
        let Ok(rel) = abs.strip_prefix(&root) else {
            return;
        };
        let rel = rel.to_string_lossy().into_owned();
        if rel.is_empty() {
            return;
        }
        if removed {
            self.remove_file(&rel);
        } else {
            self.refresh_file(&rel);
        }
    }

    fn watcher_error(&mut self, error: &str) {
        self.note_watcher_error(error);
    }

    fn apply_changes(&mut self, changes: &[(PathBuf, bool)]) {
        let root = Service::root(self).to_path_buf();
        let rel_changes: Vec<(String, bool)> = changes
            .iter()
            .filter_map(|(abs, removed)| {
                let rel = abs.strip_prefix(&root).ok()?.to_string_lossy().into_owned();
                if rel.is_empty() {
                    None
                } else {
                    Some((rel, *removed))
                }
            })
            .collect();
        let slice: Vec<(&str, bool)> = rel_changes
            .iter()
            .map(|(r, rem)| (r.as_str(), *rem))
            .collect();
        self.refresh_files(&slice);
    }
}

/// Run the repo daemon in the foreground until Shutdown, idle timeout, or
/// error.
#[cfg_attr(test, mutants::skip)] // thin adapter: open + run_corpus, both tested
pub fn run(root: &Path) -> Result<(), ServeError> {
    let service = Service::open(root)?;
    // The facts/history index is demand-driven: no ingest thread is spawned
    // here. `facts_open_and_catch_up` serves the first history query with a
    // bounded lazy ingest and spawns `spawn_facts_ingest` to keep it fresh.
    run_corpus(service)
}

/// Spawn a low-priority background thread that periodically ticks the facts
/// ingest (history.db) until fresh, then idle-polls so a ref move re-triggers
/// ingest. Queries never block on it: the ingest shares the WAL-mode
/// connection and yields every tick budget. Spawned on first facts use, not
/// at daemon start.
pub(crate) fn spawn_facts_ingest(root: &Path) {
    let root = root.to_path_buf();
    std::thread::spawn(move || {
        let mut store = match pixel_facts::FactsStore::open(&root) {
            Ok(s) => s,
            Err(_) => return,
        };
        let opts = pixel_facts::ingest::IngestOptions::default();
        // Periodic tick loop: keep ingesting until fresh, then idle-poll so a
        // ref move re-triggers ingest. Each tick is budget-bounded, so queries
        // on the same WAL-mode connection are never starved.
        let mut tick_count = 0u64;
        loop {
            match pixel_facts::ingest::ingest_tick(&mut store, &opts) {
                Ok(report) if report.fresh => {
                    let _ = store.wal_checkpoint();
                    std::thread::sleep(INGEST_IDLE_POLL);
                }
                Ok(_) => {
                    tick_count += 1;
                    if tick_count.is_multiple_of(20) {
                        let _ = store.wal_checkpoint();
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
                Err(_) => {
                    // Transient error (e.g. git lock): back off and retry.
                    std::thread::sleep(INGEST_ERROR_BACKOFF);
                }
            }
        }
    });
}

/// Run any corpus daemon in the foreground.
pub fn run_corpus(mut service: impl Corpus) -> Result<(), ServeError> {
    let root = service.root().to_path_buf();
    let sock = socket_path(&root);

    // Advisory lock on pid_path to prevent concurrent startup race and duplicate running daemons.
    let lock_path = pid_path(&root).with_extension("lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    if lock_file.try_lock_exclusive().is_err() {
        return Err(ServeError::Msg(format!(
            "daemon lock already held by another process for {}",
            root.display()
        )));
    }

    // A live socket means another daemon owns this root.
    if UnixStream::connect(&sock).is_ok() {
        return Err(ServeError::Msg(format!(
            "daemon already running for {} ({})",
            root.display(),
            sock.display()
        )));
    }
    let _ = std::fs::remove_file(&sock); // stale leftover

    let listener = UnixListener::bind(&sock)?;
    let bound = socket_identity(&sock);
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
    std::fs::write(pid_path(&root), std::process::id().to_string())?;

    let (tx, rx) = mpsc::channel::<Msg>();
    let stop = Arc::new(AtomicBool::new(false));
    let acceptor = spawn_acceptor(listener, tx.clone(), Arc::clone(&stop));

    // Watcher: raw notify events into the channel; debounced below. A
    // backend error goes through the same channel: a watch that stopped
    // reporting is exactly the failure that leaves the index stale, so it
    // must not be dropped here.
    let watch_paths = service.watch_paths();
    let _watcher = if watch_paths.is_empty() {
        None
    } else {
        let tx_fs = tx.clone();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let msg = match res {
                Ok(ev) => Msg::Fs(ev),
                Err(error) => Msg::WatcherError(error.to_string()),
            };
            // The only send failure is a dropped receiver: the loop is exiting.
            let _ = tx_fs.send(msg);
        })
        .map_err(|e| ServeError::Msg(format!("watcher init: {e}")))?;
        for wp in &watch_paths {
            watcher
                .watch(wp, RecursiveMode::Recursive)
                .map_err(|e| ServeError::Msg(format!("watch {}: {e}", wp.display())))?;
        }
        Some(watcher)
    };

    eprintln!(
        "pixel daemon: root={} socket={}",
        root.display(),
        sock.display()
    );

    // absolute path -> removed?
    let mut pending: BTreeMap<PathBuf, bool> = BTreeMap::new();
    let mut flush_at: Option<Instant> = None;
    let mut last_activity = Instant::now();
    let mut shutdown = false;
    let sweep_every = service.sweep_interval();
    let mut next_sweep = sweep_every.map(|every| Instant::now() + every);

    while !shutdown {
        let now = Instant::now();
        let idle_left = IDLE_TIMEOUT
            .checked_sub(now.duration_since(last_activity))
            .unwrap_or(Duration::ZERO);
        let timeout = match flush_at {
            Some(at) => at.saturating_duration_since(now).min(idle_left),
            None => idle_left,
        }
        .min(ROOT_POLL);
        let timeout = match next_sweep {
            Some(at) => at.saturating_duration_since(now).min(timeout),
            None => timeout,
        };

        match rx.recv_timeout(timeout.max(Duration::from_millis(1))) {
            Ok(Msg::Conn(stream)) => {
                last_activity = Instant::now();
                // The watcher may have queued a mutation just before this
                // connection. Drain those events before the request so the
                // first post-mutation read never sees the preceding
                // publication; connections drained meanwhile wait their turn.
                let mut streams = vec![stream];
                let mut failed = false;
                while let Ok(message) = rx.try_recv() {
                    match message {
                        Msg::Conn(stream) => streams.push(stream),
                        Msg::AcceptFailed => failed = true,
                        Msg::Fs(ev) => note_event(&root, &ev, &mut pending, &mut flush_at),
                        Msg::WatcherError(error) => service.watcher_error(&error),
                    }
                }
                // Apply the debounced batch before serving the connections:
                // the debounce coalesces bursts between requests, it must
                // not let a request read the index from before a mutation.
                flush_pending(&mut service, &mut pending, &mut flush_at);
                for stream in streams {
                    if !shutdown {
                        handle_conn(&mut service, stream, &mut shutdown);
                    }
                }
                if failed {
                    break;
                }
                continue;
            }
            Ok(Msg::AcceptFailed) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(Msg::Fs(ev)) => note_event(&root, &ev, &mut pending, &mut flush_at),
            Ok(Msg::WatcherError(error)) => service.watcher_error(&error),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        if let Some(at) = flush_at
            && Instant::now() >= at
        {
            flush_pending(&mut service, &mut pending, &mut flush_at);
        }

        if let (Some(at), Some(every)) = (next_sweep, sweep_every)
            && Instant::now() >= at
        {
            service.sweep();
            next_sweep = Some(Instant::now() + every);
        }

        if last_activity.elapsed() >= IDLE_TIMEOUT {
            eprintln!("pixel daemon: idle timeout, exiting");
            break;
        }
        if root_removed(&root) {
            eprintln!(
                "pixel daemon: root {} no longer exists, exiting",
                root.display()
            );
            break;
        }
    }

    let _ = stop_acceptor(&sock, bound, &stop, acceptor);
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(pid_path(&root));
    let _ = std::fs::remove_file(&lock_path);
    Ok(())
}

/// Accept connections on a blocking listener and hand them to the loop, so
/// an idle daemon sleeps instead of polling the listener. The thread ends on
/// `stop` (checked after each accept), on an accept error, or once the loop
/// has dropped its receiver.
fn spawn_acceptor(
    listener: UnixListener,
    tx: mpsc::Sender<Msg>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if stop.load(Ordering::Acquire) {
                return;
            }
            let Ok(stream) = stream else {
                let _ = tx.send(Msg::AcceptFailed);
                return;
            };
            if tx.send(Msg::Conn(stream)).is_err() {
                return;
            }
        }
    })
}

/// The `(device, inode)` of the socket file at `path`, `None` when it is gone.
fn socket_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path)
        .ok()
        .map(|meta| (meta.dev(), meta.ino()))
}

/// Wake the accept thread with a connection of our own, after `stop` is set,
/// and wait for it to exit; returns whether it did. The wake-up goes out only
/// while `sock` is still the socket this daemon bound (`bound`): once the
/// file was removed, or replaced by another daemon's socket for the same
/// root, a connection would never reach this thread, so it is left to end
/// with the process instead of `join` waiting forever.
fn stop_acceptor(
    sock: &Path,
    bound: Option<(u64, u64)>,
    stop: &AtomicBool,
    acceptor: std::thread::JoinHandle<()>,
) -> bool {
    stop.store(true, Ordering::Release);
    if bound.is_none() || socket_identity(sock) != bound {
        return false;
    }
    if UnixStream::connect(sock).is_err() {
        return false;
    }
    acceptor.join().is_ok()
}

/// The served root has been deleted (or replaced by a non-directory): every
/// answer the daemon could give from here on would describe a tree that no
/// longer exists, so the loop exits and releases the socket, pid and lock.
fn root_removed(root: &Path) -> bool {
    !root.is_dir()
}

/// Record one watcher event as a pending change, when it is one.
///
/// A read is not: `notify`'s inotify backend (the Linux one) reports an
/// `Access` event for every open (IN_OPEN) and read-only close
/// (IN_CLOSE_NOWRITE) of a watched file, so the daemon's own extraction pass
/// — or any `cat`, editor or grep — would mark every file it read as
/// changed, and the batch the loop applies before the next request would
/// move the index counters for a repository nobody edited. Metadata-only
/// events (a `chmod`, a `touch`) cannot change an answer either. Every event
/// that can change content (create, modify data or name, remove) is still
/// recorded.
fn record_event(root: &Path, ev: &notify::Event, pending: &mut BTreeMap<PathBuf, bool>) {
    if matches!(
        ev.kind,
        notify::EventKind::Access(_)
            | notify::EventKind::Modify(notify::event::ModifyKind::Metadata(_))
    ) {
        return;
    }
    for path in &ev.paths {
        let root_git_exclude = path
            .strip_prefix(root)
            .is_ok_and(|relative| relative == Path::new(".git/info/exclude"));
        if !root_git_exclude
            && path.components().any(|c| match c {
                Component::Normal(s) => IGNORED_DIRS.iter().any(|d| s == *d),
                _ => false,
            })
        {
            continue;
        }
        if path.is_dir() {
            continue;
        }
        let removed = matches!(ev.kind, notify::EventKind::Remove(_)) || !path.exists();
        // A later create/modify wins over an earlier remove and vice versa.
        pending.insert(path.clone(), removed);
    }
}

/// Record one watcher event and, when a change is pending afterwards, push
/// the debounce deadline back to `DEBOUNCE` from now.
fn note_event(
    root: &Path,
    ev: &notify::Event,
    pending: &mut BTreeMap<PathBuf, bool>,
    flush_at: &mut Option<Instant>,
) {
    record_event(root, ev, pending);
    if !pending.is_empty() {
        *flush_at = Some(Instant::now() + DEBOUNCE);
    }
}

/// Apply the debounced watcher batch now, when there is one. The loop calls
/// this before every connection so a request following a mutation cannot
/// read an index built before it, and again on the debounce timer.
fn flush_pending(
    service: &mut dyn Corpus,
    pending: &mut BTreeMap<PathBuf, bool>,
    flush_at: &mut Option<Instant>,
) {
    if pending.is_empty() {
        return;
    }
    let batch: Vec<(PathBuf, bool)> = std::mem::take(pending).into_iter().collect();
    service.apply_changes(&batch);
    *flush_at = None;
}

fn handle_conn(service: &mut dyn Corpus, stream: UnixStream, shutdown: &mut bool) {
    // On macOS/BSD a socket accepted from a non-blocking listener inherits
    // `O_NONBLOCK`. The listener blocks now, but the reset keeps the reads
    // below independent of how the stream was accepted: a non-blocking one
    // reads a request line that has not arrived yet as `WouldBlock` (the
    // connection closes unanswered) and cuts short a reply larger than the
    // send buffer.
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut writer = stream;
    let mut line = String::new();
    let mut request_count: u32 = 0;
    let deadline = Instant::now() + CONNECTION_DEADLINE;
    loop {
        // Cap requests per connection to prevent starvation.
        if request_count >= MAX_REQUESTS_PER_CONN {
            let resp = failure_response("error", "request limit exceeded");
            let _ = writer.write_all(serde_json::to_vec(&resp).unwrap_or_default().as_slice());
            let _ = writer.write_all(b"\n");
            break;
        }
        line.clear();
        // Cap line length: read in chunks and abort if the line exceeds the
        // limit, so a multi-GB line cannot exhaust memory.
        match read_capped_line(&mut reader, &mut line, MAX_REQUEST_LINE, deadline) {
            ReadResult::Ok => {}
            ReadResult::Eof => break,
            ReadResult::TooLong => {
                let resp = failure_response("error", "request line too long");
                let _ = writer.write_all(serde_json::to_vec(&resp).unwrap_or_default().as_slice());
                let _ = writer.write_all(b"\n");
                break;
            }
            ReadResult::InvalidUtf8 => {
                request_count += 1;
                let resp = failure_response("error", "request is not valid UTF-8");
                let _ = writer.write_all(serde_json::to_vec(&resp).unwrap_or_default().as_slice());
                let _ = writer.write_all(b"\n");
                continue;
            }
            ReadResult::TimedOut | ReadResult::Err => break,
        }
        request_count += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (resp, is_shutdown) = match serde_json::from_str::<Request>(trimmed) {
            Ok(req) => {
                let is_shutdown = matches!(req, Request::Shutdown);
                (service.handle(req), is_shutdown)
            }
            Err(e) => (
                failure_response("error", format!("bad request: {e}")),
                false,
            ),
        };
        let out = serde_json::to_string(&resp).unwrap_or_else(|_| {
            // Fallback: a minimal failure envelope if serialization itself
            // fails (should never happen for a Value-typed envelope).
            r#"{"ok":false,"op":"error","protocol":1,"error":{"code":"INVARIANT_VIOLATION","message":"serialize failure"}}"#
                .to_string()
        });
        let mut out = out;
        out.push('\n');
        if writer.write_all(out.as_bytes()).is_err() || writer.flush().is_err() {
            break;
        }
        if is_shutdown {
            *shutdown = true;
            break;
        }
    }
}

enum ReadResult {
    Ok,
    Eof,
    TooLong,
    InvalidUtf8,
    TimedOut,
    Err,
}

/// Read one line into `buf`, returning `TooLong` if it exceeds `max_bytes`
/// before a newline is found. The trailing newline is consumed but not
/// included in `buf` (same semantics as `read_line` minus the newline).
fn read_capped_line(
    reader: &mut BufReader<std::os::unix::net::UnixStream>,
    buf: &mut String,
    max_bytes: usize,
    deadline: Instant,
) -> ReadResult {
    use std::io::Read;
    let mut bytes = Vec::with_capacity(max_bytes.min(4096));
    let mut byte = [0u8; 1];
    loop {
        if Instant::now() >= deadline {
            return ReadResult::TimedOut;
        }
        match reader.read(&mut byte) {
            Ok(0) => {
                if bytes.is_empty() {
                    return ReadResult::Eof;
                }
                break;
            }
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                bytes.push(byte[0]);
                if bytes.len() > max_bytes {
                    return ReadResult::TooLong;
                }
            }
            Err(_) => return ReadResult::Err,
        }
    }
    match String::from_utf8(bytes) {
        Ok(line) => {
            *buf = line;
            ReadResult::Ok
        }
        Err(_) => ReadResult::InvalidUtf8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A corpus with no index behind it: enough to drive `run_corpus`'s
    /// transport loop from a test.
    struct StubCorpus(PathBuf);

    impl Corpus for StubCorpus {
        fn root(&self) -> &Path {
            &self.0
        }
        fn handle(&mut self, _req: Request) -> Response {
            failure_response("stub", "stub corpus")
        }
        fn apply_change(&mut self, _abs: &Path, _removed: bool) {}
    }

    /// A corpus that asks for a periodic sweep and counts the calls.
    struct SweptCorpus {
        root: PathBuf,
        every: Option<Duration>,
        sweeps: Arc<AtomicUsize>,
    }

    /// A corpus that records the batches the loop hands it, so a test can
    /// assert *when* the debounced watcher events are applied.
    struct RecordingCorpus {
        root: PathBuf,
        batches: Arc<std::sync::Mutex<Vec<Vec<PathBuf>>>>,
    }

    impl Corpus for RecordingCorpus {
        fn root(&self) -> &Path {
            &self.root
        }
        fn handle(&mut self, _req: Request) -> Response {
            failure_response("stub", "stub corpus")
        }
        fn apply_change(&mut self, _abs: &Path, _removed: bool) {}
        fn apply_changes(&mut self, changes: &[(PathBuf, bool)]) {
            let paths: Vec<PathBuf> = changes.iter().map(|(path, _)| path.clone()).collect();
            self.batches.lock().unwrap().push(paths);
        }
    }

    impl Corpus for SweptCorpus {
        fn root(&self) -> &Path {
            &self.root
        }
        fn handle(&mut self, _req: Request) -> Response {
            failure_response("stub", "stub corpus")
        }
        fn apply_change(&mut self, _abs: &Path, _removed: bool) {}
        fn watch_paths(&self) -> Vec<PathBuf> {
            Vec::new()
        }
        fn sweep_interval(&self) -> Option<Duration> {
            self.every
        }
        fn sweep(&mut self) {
            self.sweeps.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A corpus whose every answer is larger than a unix socket's default
    /// send buffer (8 KiB on macOS), so a short write is observable.
    struct BulkyCorpus(PathBuf);

    const BULKY_PAYLOAD: usize = 262_144; // 256 KiB

    impl Corpus for BulkyCorpus {
        fn root(&self) -> &Path {
            &self.0
        }
        fn handle(&mut self, _req: Request) -> Response {
            Response::success(
                "bulky",
                serde_json::json!({"blob": "a".repeat(BULKY_PAYLOAD)}),
            )
        }
        fn apply_change(&mut self, _abs: &Path, _removed: bool) {}
    }

    #[test]
    fn a_connection_inheriting_nonblocking_still_waits_for_a_late_request_and_writes_it_all() {
        let (server, client) = UnixStream::pair().unwrap();
        // What `accept` on the non-blocking listener hands back on macOS/BSD.
        server.set_nonblocking(true).unwrap();
        let client_thread = std::thread::spawn(move || {
            let mut client = client;
            client
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            // The request arrives after the daemon starts reading.
            std::thread::sleep(Duration::from_millis(100));
            writeln!(client, "{}", serde_json::to_string(&Request::Ping).unwrap()).unwrap();
            let mut line = String::new();
            BufReader::new(&client).read_line(&mut line).unwrap();
            line
        });
        let mut corpus = BulkyCorpus(scratch_root("bulky"));
        let mut shutdown = false;
        handle_conn(&mut corpus, server, &mut shutdown);
        let line = client_thread.join().unwrap();
        let reply: serde_json::Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("one complete JSON reply ({e}), got {} bytes", line.len()));
        assert_eq!(
            reply["result"]["blob"].as_str().map(str::len),
            Some(BULKY_PAYLOAD),
            "the whole reply reaches the client"
        );
    }

    fn scratch_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("pixel-daemon-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root.canonicalize().unwrap()
    }

    /// A socket pair carrying a canned reply: the probe's contract is the
    /// reply's `ok` field, not the transport.
    #[test]
    fn probe_ping_is_true_only_for_an_ok_reply() {
        let (mut server, mut client) = UnixStream::pair().unwrap();
        let ok = Response::success("ping", serde_json::json!({"pong": true}));
        writeln!(server, "{}", serde_json::to_string(&ok).unwrap()).unwrap();
        assert!(probe_ping(&mut client), "an ok reply is a live daemon");

        let (mut server, mut client) = UnixStream::pair().unwrap();
        let failed = failure_response("ping", "not serving");
        writeln!(server, "{}", serde_json::to_string(&failed).unwrap()).unwrap();
        assert!(!probe_ping(&mut client), "a failure envelope is not");

        let (mut server, mut client) = UnixStream::pair().unwrap();
        writeln!(server, "not json").unwrap();
        assert!(!probe_ping(&mut client), "garbage is not a live daemon");
    }

    /// The probe must be false for a root nobody serves, and true for one a
    /// daemon listens on: the two halves a constant-returning mutant breaks.
    #[test]
    fn ping_only_sees_a_listening_daemon_and_nothing_else() {
        let bare = scratch_root("probe-none");
        assert!(!ping_only(&bare));

        let root = scratch_root("probe-live");
        let listener = UnixListener::bind(socket_path(&root)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            // Poll with a deadline: a probe that never connects must fail the
            // assertion, not hang the suite.
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // A non-blocking listener hands back a non-blocking
                        // socket on BSD: reset it, or the read races the
                        // client's write instead of waiting for it.
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                        let mut line = String::new();
                        let Ok(n) = BufReader::new(&stream).read_line(&mut line) else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        assert_eq!(
                            serde_json::from_str::<Request>(&line).unwrap(),
                            Request::Ping,
                            "the probe sends a Ping"
                        );
                        let reply = Response::success("ping", serde_json::json!({"pong": true}));
                        writeln!(stream, "{}", serde_json::to_string(&reply).unwrap()).unwrap();
                        return;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => return,
                }
            }
        });
        assert!(ping_only(&root));
        server.join().unwrap();
        let _ = std::fs::remove_file(socket_path(&root));
    }

    fn shutdown(sock: &Path) -> Result<Response, String> {
        let mut stream = UnixStream::connect(sock).map_err(|error| error.to_string())?;
        stream
            .set_read_timeout(Some(PROBE_TIMEOUT))
            .map_err(|error| error.to_string())?;
        stream
            .set_write_timeout(Some(PROBE_TIMEOUT))
            .map_err(|error| error.to_string())?;
        let mut line = serde_json::to_string(&Request::Shutdown).unwrap();
        line.push('\n');
        stream
            .write_all(line.as_bytes())
            .map_err(|error| error.to_string())?;
        let mut reader = BufReader::new(stream);
        let mut reply = String::new();
        std::io::BufRead::read_line(&mut reader, &mut reply).map_err(|error| error.to_string())?;
        serde_json::from_str(&reply).map_err(|error| error.to_string())
    }

    /// The watcher alone misses the transcript an agent is streaming (macOS
    /// FSEvents defers the modify event while the file stays open), so a
    /// corpus that asks for a sweep must get it on its interval with no
    /// filesystem event and no request in between.
    #[test]
    fn corpus_sweep_runs_on_its_interval_without_events() {
        let root = scratch_root("sweep");
        let sock = socket_path(&root);
        let sweeps = Arc::new(AtomicUsize::new(0));
        let corpus = SweptCorpus {
            root: root.clone(),
            every: Some(Duration::from_millis(100)),
            sweeps: Arc::clone(&sweeps),
        };
        let started = Instant::now();
        let daemon = std::thread::spawn(move || run_corpus(corpus));
        wait_until("socket to answer", Duration::from_secs(10), || ping(&sock));

        wait_until("three sweeps", Duration::from_secs(10), || {
            sweeps.load(Ordering::SeqCst) >= 3
        });
        // The first sweep waits one full interval (nothing to sweep at
        // start-up) and each later one is rescheduled from the interval, so
        // three sweeps at 100 ms cannot land before 300 ms.
        assert!(
            started.elapsed() >= Duration::from_millis(300),
            "sweeps ran early or back to back instead of on the interval"
        );
        // The count keeps rising: sweeps are periodic, not a one-off after
        // the first request.
        let seen = sweeps.load(Ordering::SeqCst);
        wait_until("a further sweep", Duration::from_secs(10), || {
            sweeps.load(Ordering::SeqCst) > seen
        });

        let _response = shutdown(&sock).expect("shutdown request must receive a response");
        wait_until("daemon to exit", Duration::from_secs(10), || {
            daemon.is_finished()
        });
        daemon.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The accept thread hands each connection to the loop's channel and
    /// stops when told to: the daemon's own wake-up connection ends it, so
    /// `run_corpus` returns without a thread still blocked in `accept`.
    #[test]
    fn acceptor_should_forward_connections_and_stop_when_woken() {
        let root = scratch_root("acceptor");
        let sock = root.join("a.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let acceptor = spawn_acceptor(listener, tx, Arc::clone(&stop));

        let _client = UnixStream::connect(&sock).unwrap();
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)),
            Ok(Msg::Conn(_))
        ));

        assert!(stop_acceptor(
            &sock,
            socket_identity(&sock),
            &stop,
            acceptor
        ));
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "the wake-up connection must not reach the loop"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A change starts the debounce timer a full `DEBOUNCE` from now; an
    /// event that leaves nothing pending (a read) does not start it.
    #[test]
    fn note_event_should_start_the_debounce_only_for_a_pending_change() {
        use notify::event::{AccessKind, CreateKind, EventKind};
        let root = scratch_root("note-event");
        let file = root.join("a.rs");
        std::fs::write(&file, "fn a() {}\n").unwrap();
        let mut pending = BTreeMap::new();
        let mut flush_at = None;

        let read = notify::Event::new(EventKind::Access(AccessKind::Any)).add_path(file.clone());
        note_event(&root, &read, &mut pending, &mut flush_at);
        assert_eq!(flush_at, None);

        let before = Instant::now();
        let create = notify::Event::new(EventKind::Create(CreateKind::File)).add_path(file.clone());
        note_event(&root, &create, &mut pending, &mut flush_at);
        assert_eq!(pending.get(&file), Some(&false));
        let at = flush_at.expect("a pending change starts the debounce");
        assert!(
            at >= before + DEBOUNCE,
            "the debounce must wait DEBOUNCE from now"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// With its socket gone nothing can wake the accept thread, so stopping
    /// reports that it was left running instead of blocking on `join`.
    #[test]
    fn stop_acceptor_should_not_wait_on_a_removed_socket() {
        let root = scratch_root("acceptor-gone");
        let sock = root.join("a.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let (tx, _rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let acceptor = spawn_acceptor(listener, tx, Arc::clone(&stop));
        let bound = socket_identity(&sock);
        std::fs::remove_file(&sock).unwrap();

        assert!(!stop_acceptor(&sock, bound, &stop, acceptor));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The socket file was removed and another daemon for the same root bound
    /// the same path: a wake-up connection would reach that daemon, never
    /// this thread, and `join` would wait forever. Stopping must notice the
    /// replaced socket and return. The call runs on a thread with a deadline,
    /// so a regression fails the test instead of hanging it.
    #[test]
    fn stop_acceptor_should_not_wait_on_a_socket_another_daemon_bound() {
        let root = scratch_root("acceptor-replaced");
        let sock = root.join("a.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let (tx, _rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let acceptor = spawn_acceptor(listener, tx, Arc::clone(&stop));
        let bound = socket_identity(&sock);
        std::fs::remove_file(&sock).unwrap();
        let _other_daemon = UnixListener::bind(&sock).unwrap();
        assert_ne!(socket_identity(&sock), bound);

        let (done_tx, done_rx) = mpsc::channel();
        let sock_for_stop = sock.clone();
        std::thread::spawn(move || {
            let _ = done_tx.send(stop_acceptor(&sock_for_stop, bound, &stop, acceptor));
        });
        assert_eq!(done_rx.recv_timeout(Duration::from_secs(5)), Ok(false));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn socket_identity_should_follow_the_file_not_the_path() {
        let root = scratch_root("socket-identity");
        let sock = root.join("a.sock");
        assert_eq!(socket_identity(&sock), None);
        let first = UnixListener::bind(&sock).unwrap();
        let identity = socket_identity(&sock);
        assert!(identity.is_some());
        drop(first);
        std::fs::remove_file(&sock).unwrap();
        let _second = UnixListener::bind(&sock).unwrap();
        assert!(socket_identity(&sock).is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The trait default is "no sweep": the repo corpus relies on it, and a
    /// default interval would make every repo daemon run periodic work.
    #[test]
    fn default_corpus_has_no_sweep_interval() {
        let stub = StubCorpus(PathBuf::from("/nonexistent"));
        assert_eq!(stub.sweep_interval(), None);
    }

    /// The loop applies the pending watcher batch before it serves a
    /// connection, not after: a request that follows a mutation must not
    /// read the index from before it. Nothing is applied when no event
    /// arrived, and the batch is consumed exactly once.
    #[test]
    fn pending_batch_is_applied_when_a_connection_arrives() {
        let changed = PathBuf::from("/repo/src/edited.ts");
        let batches = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut corpus = RecordingCorpus {
            root: PathBuf::from("/repo"),
            batches: Arc::clone(&batches),
        };
        let mut pending = BTreeMap::new();
        let mut flush_at = None;

        flush_pending(&mut corpus, &mut pending, &mut flush_at);
        assert!(
            batches.lock().unwrap().is_empty(),
            "an empty batch must not reach the corpus"
        );

        pending.insert(changed.clone(), false);
        flush_at = Some(Instant::now() + Duration::from_secs(60));
        flush_pending(&mut corpus, &mut pending, &mut flush_at);
        assert_eq!(
            batches.lock().unwrap().as_slice(),
            [vec![changed]],
            "the pending event must be applied before the request is served"
        );
        assert!(
            pending.is_empty(),
            "the batch must be consumed, never applied twice"
        );
        assert_eq!(flush_at, None, "the debounce timer must not fire again");
    }

    /// A read is not a change. `notify`'s inotify backend reports an
    /// `Access` event for every open and read-only close of a watched file,
    /// so the daemon's own extraction pass would mark every file it read as
    /// changed and the batch the loop applies before the next request would
    /// move the index counters for a repository nobody edited. Metadata-only
    /// events cannot change an answer either, while every content event
    /// still reaches the corpus.
    #[test]
    fn watcher_records_only_events_that_change_content() {
        use notify::event::{
            AccessKind, AccessMode, CreateKind, DataChange, MetadataKind, ModifyKind,
        };

        let root = scratch_root("content-events");
        let file = root.join("login.rs");
        std::fs::write(&file, "pub fn login() -> bool { true }\n").unwrap();
        std::fs::create_dir_all(root.join(".pixel")).unwrap();
        let ignored = root.join(".pixel/actions.jsonl");
        std::fs::write(&ignored, "{}\n").unwrap();

        for kind in [
            notify::EventKind::Access(AccessKind::Open(AccessMode::Read)),
            notify::EventKind::Access(AccessKind::Close(AccessMode::Read)),
            notify::EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)),
        ] {
            let mut pending = BTreeMap::new();
            record_event(
                &root,
                &notify::Event::new(kind).add_path(file.clone()),
                &mut pending,
            );
            assert!(pending.is_empty(), "{kind:?} must not look like a change");
        }

        let mut pending = BTreeMap::new();
        for kind in [
            notify::EventKind::Create(CreateKind::File),
            notify::EventKind::Modify(ModifyKind::Data(DataChange::Any)),
            notify::EventKind::Modify(ModifyKind::Name(notify::event::RenameMode::Any)),
        ] {
            record_event(
                &root,
                &notify::Event::new(kind).add_path(file.clone()),
                &mut pending,
            );
            assert_eq!(pending.get(&file), Some(&false), "{kind:?} is a change");
        }
        record_event(
            &root,
            &notify::Event::new(notify::EventKind::Remove(notify::event::RemoveKind::File))
                .add_path(file.clone()),
            &mut pending,
        );
        assert_eq!(pending.get(&file), Some(&true), "a removal is a removal");
        // The ignored-tree rule survives the content filter: `.pixel` state
        // is the daemon's own churn, never a source change.
        record_event(
            &root,
            &notify::Event::new(notify::EventKind::Modify(ModifyKind::Data(DataChange::Any)))
                .add_path(ignored.clone()),
            &mut pending,
        );
        assert!(
            !pending.contains_key(&ignored),
            ".pixel state is not a source change"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn watcher_admits_only_the_root_git_info_exclude_control() {
        use notify::event::{DataChange, ModifyKind};

        let root = scratch_root("git-exclude-event");
        let root_exclude = root.join(".git/info/exclude");
        let object = root.join(".git/objects/ab/object");
        let nested_exclude = root.join("nested/.git/info/exclude");
        for path in [&root_exclude, &object, &nested_exclude] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "changed\n").unwrap();
        }
        let event =
            notify::Event::new(notify::EventKind::Modify(ModifyKind::Data(DataChange::Any)))
                .add_path(root_exclude.clone())
                .add_path(object.clone())
                .add_path(nested_exclude.clone());
        let mut pending = BTreeMap::new();
        record_event(&root, &event, &mut pending);

        assert_eq!(pending.get(&root_exclude), Some(&false));
        assert!(!pending.contains_key(&object), ".git objects stay excluded");
        assert!(
            !pending.contains_key(&nested_exclude),
            "only the served root's exclude file is a control"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A corpus without an interval is never swept, so a corpus without
    /// periodic work pays nothing.
    #[test]
    fn corpus_without_interval_is_never_swept() {
        let root = scratch_root("nosweep");
        let sock = socket_path(&root);
        let sweeps = Arc::new(AtomicUsize::new(0));
        let corpus = SweptCorpus {
            root: root.clone(),
            every: None,
            sweeps: Arc::clone(&sweeps),
        };
        let daemon = std::thread::spawn(move || run_corpus(corpus));
        wait_until("socket to answer", Duration::from_secs(10), || ping(&sock));
        std::thread::sleep(Duration::from_millis(400));
        assert!(ping(&sock));
        assert_eq!(sweeps.load(Ordering::SeqCst), 0);

        let _response = shutdown(&sock).expect("shutdown request must receive a response");
        wait_until("daemon to exit", Duration::from_secs(10), || {
            daemon.is_finished()
        });
        daemon.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    fn wait_until(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + limit;
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// One round trip on the daemon socket: forces a loop iteration and
    /// proves the daemon is serving.
    fn ping(sock: &Path) -> bool {
        let Ok(mut stream) = UnixStream::connect(sock) else {
            return false;
        };
        let mut line = serde_json::to_string(&Request::Ping).unwrap();
        line.push('\n');
        if stream.write_all(line.as_bytes()).is_err() {
            return false;
        }
        let mut reader = BufReader::new(stream);
        let mut reply = String::new();
        std::io::BufRead::read_line(&mut reader, &mut reply).is_ok() && !reply.is_empty()
    }

    /// An auto-started daemon is detached from its parent and would otherwise
    /// live the full idle timeout after its root is deleted: a test suite
    /// that runs the CLI against throwaway fixtures left one daemon per
    /// fixture behind (18 per run, load average past 70 after a few runs).
    #[test]
    fn daemon_exits_once_its_root_is_deleted() {
        let root = std::env::temp_dir().join(format!(
            "pixel-daemon-root-gone-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let sock = socket_path(&root);
        let served = root.clone();
        let daemon = std::thread::spawn(move || run_corpus(StubCorpus(served)));

        wait_until("socket to answer", Duration::from_secs(10), || ping(&sock));
        // The first request ran one loop iteration with the root present; a
        // daemon that exits on that iteration has released its socket by the
        // time of the second request. The exit below is therefore tied to
        // the removal, not to the check firing unconditionally.
        std::thread::sleep(Duration::from_millis(300));
        assert!(ping(&sock), "daemon stopped serving while its root existed");
        assert!(
            !daemon.is_finished(),
            "daemon exited while its root existed"
        );

        std::fs::remove_dir_all(&root).unwrap();
        wait_until(
            "daemon to exit after root removal",
            ROOT_POLL + Duration::from_secs(10),
            || daemon.is_finished(),
        );
        daemon.join().unwrap().unwrap();
        assert!(!sock.exists(), "socket must be released on exit");
        assert!(
            !pid_path(&root).exists(),
            "pid file must be released on exit"
        );
    }

    #[test]
    fn capped_line_preserves_utf8() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        writer.write_all("📋\n".as_bytes()).unwrap();
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        assert!(matches!(
            read_capped_line(
                &mut reader,
                &mut line,
                16,
                Instant::now() + Duration::from_secs(1)
            ),
            ReadResult::Ok
        ));
        assert_eq!(line, "📋");
    }

    #[test]
    fn oversized_line_is_rejected_without_unbounded_drain() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        writer.write_all(b"0123456789\nnext\n").unwrap();
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        assert!(matches!(
            read_capped_line(
                &mut reader,
                &mut line,
                4,
                Instant::now() + Duration::from_secs(1)
            ),
            ReadResult::TooLong
        ));
    }

    #[test]
    fn expired_connection_deadline_stops_frame_read() {
        let (_writer, reader) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        assert!(matches!(
            read_capped_line(&mut reader, &mut line, 4, Instant::now()),
            ReadResult::TimedOut
        ));
    }
}
