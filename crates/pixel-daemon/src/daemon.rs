// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Unix-socket NDJSON daemon: one JSON `Request` per line, one JSON
//! `Response` line back. Two threads share the work. The writer, the thread
//! that called [`run_corpus`], owns the corpus (`Service` is not `Send`) and
//! runs one [`Job`] at a time. The loop, on its own thread, owns the socket,
//! the watcher, the debounce and the idle and root checks; an accept thread
//! hands each connection to a connection thread that reads its request lines
//! and passes each one to the loop. The loop sends the writer, in arrival
//! order, the debounced watcher batch and then the requests behind it, so a
//! request following a mutation never reads an index built before it. The
//! writer still runs everything in order: a long mutation (a `sync-branch`
//! is several git commands of up to 120 s each) delays every request queued
//! behind it, until reads move to replicas (#1087).

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use fs2::FileExt;
use notify::{RecursiveMode, Watcher};

use crate::api::{Request, Response, ServeError, Service, failure_response};

mod readers;

pub use readers::ReadReplica;
use readers::{Busy, READERS, Readers};

const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const DEBOUNCE: Duration = Duration::from_millis(500);
/// Four debounce windows retain burst batching while bounding queue wait during
/// continuous edits to two seconds; applying the batch can take additional time.
const MAX_PENDING_WAIT: Duration = Duration::from_secs(2);
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
/// writer's queue indefinitely.
const MAX_REQUESTS_PER_CONN: u32 = 64;
/// Connection threads alive at once. Past it the accept thread waits for one
/// to end, and further connections wait in the listener's backlog, as they
/// did when the loop served one connection at a time.
const MAX_CONN_THREADS: usize = 32;

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
    /// One request a connection thread read, and where its answer goes.
    Request(Request, mpsc::Sender<Response>),
    /// The listener failed; the loop exits as it did on an accept error.
    AcceptFailed,
    Fs(notify::Event),
    /// A `notify` callback error, forwarded to the loop (which queues it for
    /// the corpus) so a watch that stopped reporting is counted and logged
    /// instead of dropped on the callback thread.
    WatcherError(String),
    /// The background watch registration finished: the guard that keeps the
    /// watch alive and what it covers, or why it could not be set up.
    WatchReady(Result<(WatchGuard, WatchReport), String>),
    /// The writer stopped (a panic outside any request): nothing will
    /// answer, so the loop exits.
    WriterGone,
    /// A read a replica could not answer: the writer serves it.
    Requeue(Request, mpsc::Sender<Response>),
}

/// What the loop hands the writer, in the order it decided it. The writer
/// runs one job at a time, so the order the loop sees (a watcher batch, then
/// the request that followed it) is the order the corpus sees.
enum Job {
    /// A watcher batch.
    Apply(Vec<(PathBuf, bool)>),
    Serve(Request, mpsc::Sender<Response>),
    GitMoved,
    WatcherError(String),
    WatchReady(WatchReport),
    Sweep,
}

impl Job {
    /// The job changes what a read answers (the index, or HEAD and the
    /// branch a snapshot names): no read may overtake it.
    fn refreshes(&self) -> bool {
        matches!(self, Job::Apply(_) | Job::GitMoved | Job::WatchReady(_))
    }
}

/// Git's empty tree (SHA-1): the base a first commit is diffed from.
pub(crate) const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// A registered watch, alive for as long as the loop holds it.
trait Watch: Send {
    /// Start watching `dir`, a directory that appeared after registration,
    /// and the directories under it that are not ignored; return the files
    /// already inside, whose creation no watch could see.
    ///
    /// # Errors
    ///
    /// The backend refused a watch (an exhausted inotify `max_user_watches`).
    fn extend(&mut self, dir: &Path) -> Result<Vec<PathBuf>, String>;
}

/// Keeps a registered watch alive for as long as the loop holds it.
type WatchGuard = Box<dyn Watch>;

/// A watch as its registrar hands it over, before the loop times it.
struct Registered {
    guard: WatchGuard,
    /// Directories holding a watch: one per root for a recursive watch.
    dirs: usize,
    recursive: bool,
    git_dir: bool,
}

/// What the watch registration covered and how long it took, shown by
/// `status` so a watch that grows or slows down again is visible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WatchReport {
    /// Directories holding a watch: one per root for a recursive watch.
    pub dirs: usize,
    /// One recursive watch per root (FSEvents) rather than one watch per
    /// directory that is not ignored (inotify).
    pub recursive: bool,
    /// From the start of the registration to the watch being live.
    pub elapsed: Duration,
    /// The root's `.git` directory is under the watch, so a commit, a
    /// checkout or a reset reaches `Corpus::git_moved`.
    pub git_dir: bool,
}

/// Registers the watch on `paths`, forwarding its events into the loop's
/// channel. Runs off the serving thread, so a slow registration never delays
/// an answer (11 515 `node_modules` directories once made a repository wait
/// 11 to 28 s for its first one).
type Registrar =
    Box<dyn FnOnce(Vec<PathBuf>, mpsc::Sender<Msg>) -> Result<Registered, String> + Send>;

/// A corpus a daemon can serve: the repo `Service`, or the machine-wide
/// transcript recall service. The transport (socket, watcher, debounce,
/// framing) is identical for every corpus.
pub trait Corpus {
    /// Root that keys the socket path and is watched by default.
    fn root(&self) -> &Path;
    fn handle(&mut self, req: Request) -> Response;
    /// The answer to `ping`, when it is the same for the whole life of the
    /// corpus: the loop then answers `ping` itself, ahead of the writer's
    /// queue, so a liveness probe never waits behind a long request
    /// (default: `None`, the writer answers it).
    fn pong(&self) -> Option<Response> {
        None
    }
    /// A copy that answers read-plane requests on its own thread from what
    /// the writer last published, used while the writer is busy (default:
    /// `None`, the writer answers everything).
    fn read_replica(&self) -> Option<Box<dyn ReadReplica>> {
        None
    }
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
    /// The watch is registered and reporting, as `report` describes. Changes
    /// made while it was being set up were not observed, so a corpus that can
    /// list its changed files re-reads them here (default: nothing).
    fn watch_ready(&mut self, _report: &WatchReport) {}
    /// HEAD or a branch moved under the root's `.git` (a commit, a checkout,
    /// a reset): drop what was derived from them (default: nothing).
    fn git_moved(&mut self) {}
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

    fn pong(&self) -> Option<Response> {
        Some(Response::success("ping", self.pong_value()))
    }

    fn read_replica(&self) -> Option<Box<dyn ReadReplica>> {
        Some(Box::new(Service::read_replica(self)))
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

    fn git_moved(&mut self) {
        self.note_git_moved();
        // What the move rewrote is read now, whatever events the watcher
        // delivers for it, so the index answers for the new HEAD.
        self.catch_up_head();
    }

    /// Re-read every path that may have changed while the watch was still
    /// being registered, since none of it raised an event: what `git status`
    /// lists now, what the overlay held (an edit discarded by `git checkout
    /// -- f` or `git stash` leaves `git status` clean), and what a HEAD move
    /// (`git checkout <branch>`, `git pull`) rewrote. A path is removed when
    /// it is gone from disk, as for a watcher event.
    fn watch_ready(&mut self, report: &WatchReport) {
        self.note_watch_ready(*report);
        let root = Service::root(self).to_path_buf();
        let (_opened_head, mut paths) = self.index_catch_up();
        paths.extend(
            pixel_index::gitsync::status_porcelain(&root)
                .into_iter()
                .map(|(_xy, path)| path),
        );
        let changes: Vec<(PathBuf, bool)> = paths
            .into_iter()
            .map(|path| {
                let abs = root.join(path);
                let removed = !abs.exists();
                (abs, removed)
            })
            .collect();
        self.apply_changes(&changes);
        // A HEAD move made meanwhile (a repository with no commit at open is
        // diffed from the empty tree).
        self.catch_up_head();
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
// Detached endless ingest thread; the tick counter only paces WAL checkpoints.
#[cfg_attr(test, mutants::skip)]
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
pub fn run_corpus(service: impl Corpus) -> Result<(), ServeError> {
    // FSEvents follows a whole tree from one stream without walking it;
    // inotify needs one watch per directory, so it only gets the ones whose
    // events the loop keeps.
    let register: Registrar = if cfg!(target_os = "macos") {
        Box::new(register_recursive_watch)
    } else {
        Box::new(register_pruned_watch)
    };
    run_corpus_with(service, register)
}

/// The platform watcher, forwarding events and backend errors into the
/// loop's channel.
#[cfg_attr(test, mutants::skip)] // adapter over the notify backend; the loop's handling is tested
fn forwarding_watcher(tx: mpsc::Sender<Msg>) -> Result<notify::RecommendedWatcher, String> {
    notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let msg = match res {
            Ok(ev) => Msg::Fs(ev),
            Err(error) => Msg::WatcherError(error.to_string()),
        };
        // The only send failure is a dropped receiver: the loop is exiting.
        let _ = tx.send(msg);
    })
    .map_err(|e| format!("watcher init: {e}"))
}

/// One recursive watch per root, which already covers every directory
/// created under it.
struct RecursiveWatch(
    #[expect(dead_code, reason = "held for its Drop")] notify::RecommendedWatcher,
);

impl Watch for RecursiveWatch {
    fn extend(&mut self, _dir: &Path) -> Result<Vec<PathBuf>, String> {
        Ok(Vec::new())
    }
}

/// Watch every path recursively: the macOS registration, where FSEvents
/// walks nothing and holds no descriptor per directory.
#[cfg_attr(test, mutants::skip)] // adapter over the notify backend; the loop's handling is tested
fn register_recursive_watch(
    paths: Vec<PathBuf>,
    tx: mpsc::Sender<Msg>,
) -> Result<Registered, String> {
    let mut watcher = forwarding_watcher(tx)?;
    for path in &paths {
        watcher
            .watch(path, RecursiveMode::Recursive)
            .map_err(|e| format!("watch {}: {e}", path.display()))?;
    }
    let git_dir = paths.iter().any(|root| root.join(".git").is_dir());
    Ok(Registered {
        guard: Box::new(RecursiveWatch(watcher)),
        dirs: paths.len(),
        recursive: true,
        git_dir,
    })
}

/// One non-recursive watch per directory that is not ignored, under the
/// roots it was registered for.
struct PrunedWatch {
    watcher: notify::RecommendedWatcher,
    roots: Vec<PathBuf>,
}

impl Watch for PrunedWatch {
    fn extend(&mut self, dir: &Path) -> Result<Vec<PathBuf>, String> {
        let Some(root) = self.roots.iter().find(|root| dir.starts_with(root)) else {
            return Ok(Vec::new());
        };
        if ignored_below(root, dir) || !dir.is_dir() {
            return Ok(Vec::new());
        }
        let tree = unignored_tree(dir, true);
        watch_each(&mut self.watcher, &tree.dirs)?;
        Ok(tree.files)
    }
}

/// Watch each directory under `paths` that is not ignored, one
/// non-recursive watch each: the registration everywhere but macOS. A
/// recursive inotify watch walks and watches every ignored directory too
/// (15 825 watches where 2 216 suffice), and past `max_user_watches` it
/// fails, leaving the daemon without live updates. The `.git` directories
/// of `GIT_WATCHED_DIRS` are watched as well.
#[cfg_attr(test, mutants::skip)] // the walk and the extension are tested; this joins them to the backend
fn register_pruned_watch(paths: Vec<PathBuf>, tx: mpsc::Sender<Msg>) -> Result<Registered, String> {
    let mut watcher = forwarding_watcher(tx)?;
    let dirs = pruned_watch_dirs(&paths);
    let watched = watch_each(&mut watcher, &dirs)?;
    let git_dir = paths.iter().any(|root| dirs.contains(&root.join(".git")));
    Ok(Registered {
        guard: Box::new(PrunedWatch {
            watcher,
            roots: paths,
        }),
        dirs: watched,
        recursive: false,
        git_dir,
    })
}

/// The directories a pruned watch registers for `roots`.
fn pruned_watch_dirs(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for root in roots {
        dirs.extend(unignored_tree(root, false).dirs);
        for sub in GIT_WATCHED_DIRS {
            let dir = root.join(".git").join(sub);
            if dir.is_dir() {
                dirs.push(dir);
            }
        }
    }
    dirs
}

/// The directories of `<root>/.git` a pruned watch keeps: `info` for its
/// `exclude` file, the directory itself for `HEAD` and `packed-refs`, and
/// `logs` for the `HEAD` reflog every commit, checkout and reset appends to.
const GIT_WATCHED_DIRS: [&str; 3] = ["", "info", "logs"];

/// Add one non-recursive watch per directory in a single backend commit;
/// return how many hold one. A directory removed since the walk is skipped.
fn watch_each(watcher: &mut impl Watcher, dirs: &[PathBuf]) -> Result<usize, String> {
    let mut paths = watcher.paths_mut();
    let mut watched = 0;
    for dir in dirs {
        match paths.add(dir, RecursiveMode::NonRecursive) {
            Ok(()) => watched += 1,
            Err(error) if vanished(&error) => {}
            Err(error) => return Err(format!("watch {}: {error}", dir.display())),
        }
    }
    paths.commit().map_err(|e| format!("watch commit: {e}"))?;
    Ok(watched)
}

/// The backend refused a path because it no longer exists.
fn vanished(error: &notify::Error) -> bool {
    match &error.kind {
        notify::ErrorKind::PathNotFound => true,
        notify::ErrorKind::Io(io) => io.kind() == std::io::ErrorKind::NotFound,
        _ => false,
    }
}

/// The directories and regular files under a directory, pruned at the ones
/// the event filter drops anyway.
struct Tree {
    /// `top` first, then every directory below it whose name is not in
    /// `IGNORED_DIRS`; symlinks are not followed.
    dirs: Vec<PathBuf>,
    /// The regular files in those directories, when asked for.
    files: Vec<PathBuf>,
}

/// Walk `top` without descending into an ignored directory.
fn unignored_tree(top: &Path, with_files: bool) -> Tree {
    let mut tree = Tree {
        dirs: vec![top.to_path_buf()],
        files: Vec::new(),
    };
    let mut stack = vec![top.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if !is_ignored_dir(&entry.file_name()) {
                    tree.dirs.push(entry.path());
                    stack.push(entry.path());
                }
            } else if with_files && kind.is_file() {
                tree.files.push(entry.path());
            }
        }
    }
    tree
}

/// A directory name whose whole subtree the watch skips.
fn is_ignored_dir(name: &std::ffi::OsStr) -> bool {
    IGNORED_DIRS.iter().any(|d| name == *d)
}

/// `dir` sits in an ignored directory below `root`.
fn ignored_below(root: &Path, dir: &Path) -> bool {
    dir.strip_prefix(root).is_ok_and(|relative| {
        relative.components().any(|c| match c {
            Component::Normal(name) => is_ignored_dir(name),
            _ => false,
        })
    })
}

/// [`run_corpus`] with the watch registration as a parameter, so a test can
/// stand in a slow one.
fn run_corpus_with(mut service: impl Corpus, register: Registrar) -> Result<(), ServeError> {
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
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
    std::fs::write(pid_path(&root), std::process::id().to_string())?;
    // From here every way out of this function, a panic included, releases
    // the socket, pid and lock files.
    let _release = ReleaseOnExit([sock.clone(), pid_path(&root), lock_path.clone()]);

    eprintln!(
        "pixel daemon: root={} socket={}",
        root.display(),
        sock.display()
    );

    let (tx, rx) = mpsc::channel::<Msg>();
    let (jobs_tx, jobs_rx) = mpsc::channel::<Job>();
    let busy = Arc::new(Busy::default());
    let replicas: Option<Vec<Box<dyn ReadReplica>>> =
        (0..READERS).map(|_| service.read_replica()).collect();
    let readers = replicas.map_or_else(Readers::none, |replicas| {
        Readers::spawn(replicas, &tx, &busy)
    });
    let lane = Lane {
        root,
        sock,
        listener,
        register,
        watch_paths: service.watch_paths(),
        sweep_every: service.sweep_interval(),
        pong: service.pong(),
        tx: tx.clone(),
        rx,
        writer: Queued::new(service.root().to_path_buf(), jobs_tx, Arc::clone(&busy)),
        readers,
    };
    let dispatcher = std::thread::spawn(move || run_loop(lane));
    // Declared after the queue's receiver, so a panic unwinding out of the
    // writer wakes the loop before the receiver goes.
    let wake = WakeOnDrop(tx);
    run_writer(&mut service, &jobs_rx, &busy);
    drop(wake);
    if let Err(payload) = dispatcher.join() {
        std::panic::resume_unwind(payload);
    }
    Ok(())
}

/// Everything the loop thread owns.
struct Lane {
    root: PathBuf,
    sock: PathBuf,
    listener: UnixListener,
    register: Registrar,
    watch_paths: Vec<PathBuf>,
    sweep_every: Option<Duration>,
    /// The corpus's constant `ping` answer, given by the loop itself.
    pong: Option<Response>,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    writer: Queued,
    /// Answer reads while the writer is busy.
    readers: Readers,
}

/// The writer: run every job on the corpus, in order, until the loop drops
/// the queue. A panic in a request answers that request with a failure
/// ([`answer`]); a panic anywhere else unwinds out of here.
fn run_writer(service: &mut dyn Corpus, jobs: &mpsc::Receiver<Job>, busy: &Busy) {
    for job in jobs {
        let apply = job.refreshes();
        match job {
            Job::Apply(batch) => service.apply_changes(&batch),
            Job::Serve(req, reply) => {
                // The client may have gone: its answer has nowhere to go.
                let _ = reply.send(answer(service, req));
            }
            Job::GitMoved => service.git_moved(),
            Job::WatcherError(error) => service.watcher_error(&error),
            Job::WatchReady(report) => service.watch_ready(&report),
            Job::Sweep => service.sweep(),
        }
        busy.done(apply);
    }
}

/// Tells the loop the writer stopped, when dropped.
struct WakeOnDrop(mpsc::Sender<Msg>);

impl Drop for WakeOnDrop {
    fn drop(&mut self) {
        let _ = self.0.send(Msg::WriterGone);
    }
}

/// The loop's handle on the writer: each corpus callback the loop makes
/// becomes a job in the writer's queue, in the order the loop makes it.
struct Queued {
    root: PathBuf,
    jobs: mpsc::Sender<Job>,
    /// What the writer holds, so the loop knows when a read may skip it.
    busy: Arc<Busy>,
    /// A send failed: the writer is gone and nothing will answer.
    gone: bool,
}

impl Queued {
    fn new(root: PathBuf, jobs: mpsc::Sender<Job>, busy: Arc<Busy>) -> Self {
        Queued {
            root,
            jobs,
            busy,
            gone: false,
        }
    }

    fn send(&mut self, job: Job) {
        let apply = job.refreshes();
        self.busy.queued(apply);
        if self.jobs.send(job).is_err() {
            self.busy.done(apply);
            self.gone = true;
        }
    }

    /// Serve `req`: on a reader when it is a read the writer's queue may be
    /// skipped for ([`Busy::reader_may_take`]), on the writer otherwise.
    fn route(&mut self, readers: &Readers, req: Request, reply: mpsc::Sender<Response>) {
        if self.busy.reader_may_take(&req) {
            if let Some((req, reply)) = readers.offer(req, reply) {
                self.serve(req, reply);
            }
        } else {
            self.serve(req, reply);
        }
    }

    /// Queue `req` without waiting: the writer sends the answer to `reply`.
    fn serve(&mut self, req: Request, reply: mpsc::Sender<Response>) {
        self.send(Job::Serve(req, reply));
    }
}

impl Corpus for Queued {
    fn root(&self) -> &Path {
        &self.root
    }

    /// Queue `req` and wait for its answer. The loop never calls it (it
    /// queues with [`Queued::serve`] and moves on); it keeps the handle a
    /// complete corpus.
    fn handle(&mut self, req: Request) -> Response {
        let op = req.op_name();
        let (reply, answer) = mpsc::channel();
        self.serve(req, reply);
        answer
            .recv()
            .unwrap_or_else(|_| failure_response(op, "daemon writer stopped"))
    }

    fn apply_change(&mut self, abs: &Path, removed: bool) {
        self.send(Job::Apply(vec![(abs.to_path_buf(), removed)]));
    }

    fn apply_changes(&mut self, changes: &[(PathBuf, bool)]) {
        self.send(Job::Apply(changes.to_vec()));
    }

    fn sweep(&mut self) {
        self.send(Job::Sweep);
    }

    fn watch_ready(&mut self, report: &WatchReport) {
        self.send(Job::WatchReady(*report));
    }

    fn git_moved(&mut self) {
        self.send(Job::GitMoved);
    }

    fn watcher_error(&mut self, error: &str) {
        self.send(Job::WatcherError(error.to_owned()));
    }
}

/// The loop: socket, watcher, debounce, idle and root checks. It never runs
/// a corpus callback itself; each one goes to the writer through
/// `lane.writer`, in the order the loop decides it.
fn run_loop(lane: Lane) {
    let Lane {
        root,
        sock,
        listener,
        register,
        watch_paths,
        sweep_every,
        pong,
        tx,
        rx,
        writer: mut service,
        readers,
    } = lane;
    let bound = socket_identity(&sock);
    let stop = Arc::new(AtomicBool::new(false));
    let acceptor = spawn_acceptor(listener, tx.clone(), Arc::clone(&stop), root.clone());

    // Watcher: raw notify events into the channel; debounced below. It is
    // registered on its own thread so the loop answers from the first
    // request; `WatchReady` hands the loop its guard. A backend error goes
    // through the same channel: a watch that stopped reporting is exactly
    // the failure that leaves the index stale, so it must not be dropped.
    let mut watcher: Option<WatchGuard> = None;
    // Directories created since the last pass, extended into the watch
    // (held until it is registered).
    let mut created: Vec<PathBuf> = Vec::new();
    if !watch_paths.is_empty() {
        let tx_watch = tx.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            let ready = register(watch_paths, tx_watch.clone()).map(|registered| {
                let report = WatchReport {
                    dirs: registered.dirs,
                    recursive: registered.recursive,
                    elapsed: started.elapsed(),
                    git_dir: registered.git_dir,
                };
                (registered.guard, report)
            });
            let _ = tx_watch.send(Msg::WatchReady(ready));
        });
    }
    drop(tx);

    // absolute path -> removed?
    let mut pending: BTreeMap<PathBuf, bool> = BTreeMap::new();
    let mut flush_at: Option<Instant> = None;
    let mut first_pending_at = None;
    let mut last_activity = Instant::now();
    let mut shutdown = false;
    let mut next_sweep = sweep_every.map(|every| Instant::now() + every);

    // Compared on every pass: a directory swapped in at the same path must
    // stop this daemon as surely as a deleted one.
    let opened_root = root_identity(&root);
    while !shutdown && !service.gone {
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
            Ok(Msg::Request(Request::Ping, reply)) if pong.is_some() => {
                // A liveness probe: answered here, never queued behind the
                // writer, and it neither drains nor flushes the watcher.
                last_activity = Instant::now();
                let _ = reply.send(pong.clone().expect("guarded by is_some"));
            }
            Ok(Msg::Request(req, reply)) => {
                last_activity = Instant::now();
                // The watcher may have queued a mutation just before this
                // request. Drain those events before the request so the
                // first post-mutation read never sees the preceding
                // publication; requests drained meanwhile wait their turn.
                let mut requests = vec![(req, reply)];
                let mut failed = false;
                while let Ok(message) = rx.try_recv() {
                    match message {
                        Msg::Request(req, reply) => requests.push((req, reply)),
                        Msg::Requeue(req, reply) => service.serve(req, reply),
                        Msg::AcceptFailed | Msg::WriterGone => failed = true,
                        Msg::Fs(ev) => {
                            if moves_head(&root, &ev) {
                                service.git_moved();
                            }
                            note_event(
                                &root,
                                &ev,
                                &mut pending,
                                &mut flush_at,
                                &mut first_pending_at,
                            );
                            created.extend(created_dirs(&ev));
                        }
                        Msg::WatcherError(error) => service.watcher_error(&error),
                        Msg::WatchReady(ready) => {
                            watcher = watch_ready(&mut service, ready);
                        }
                    }
                }
                extend_watch(
                    watcher.as_mut(),
                    &mut created,
                    &mut service,
                    &mut pending,
                    &mut flush_at,
                    &mut first_pending_at,
                );
                // Queue the debounced batch ahead of the requests: the
                // debounce coalesces bursts between requests, it must not
                // let a request read the index from before a mutation.
                flush_pending(
                    &mut service,
                    &mut pending,
                    &mut flush_at,
                    &mut first_pending_at,
                );
                for (req, reply) in requests {
                    // Nothing after a shutdown is served; dropping `reply`
                    // tells its connection.
                    if shutdown {
                        continue;
                    }
                    match (&req, &pong) {
                        (Request::Ping, Some(pong)) => {
                            let _ = reply.send(pong.clone());
                        }
                        _ => {
                            shutdown = matches!(req, Request::Shutdown);
                            service.route(&readers, req, reply);
                        }
                    }
                }
                if failed {
                    break;
                }
                continue;
            }
            Ok(Msg::AcceptFailed | Msg::WriterGone) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                break;
            }
            Ok(Msg::Requeue(req, reply)) => service.serve(req, reply),
            Ok(Msg::Fs(ev)) => {
                if moves_head(&root, &ev) {
                    service.git_moved();
                }
                note_event(
                    &root,
                    &ev,
                    &mut pending,
                    &mut flush_at,
                    &mut first_pending_at,
                );
                created.extend(created_dirs(&ev));
            }
            Ok(Msg::WatcherError(error)) => service.watcher_error(&error),
            Ok(Msg::WatchReady(ready)) => watcher = watch_ready(&mut service, ready),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        extend_watch(
            watcher.as_mut(),
            &mut created,
            &mut service,
            &mut pending,
            &mut flush_at,
            &mut first_pending_at,
        );

        if let Some(at) = flush_at
            && Instant::now() >= at
        {
            flush_pending(
                &mut service,
                &mut pending,
                &mut flush_at,
                &mut first_pending_at,
            );
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
        if root_replaced(&root, opened_root) {
            eprintln!(
                "pixel daemon: root {} was removed or replaced, exiting",
                root.display()
            );
            break;
        }
    }

    // Closing the queue lets the writer finish the jobs already in it, the
    // request in flight included, then return. Requests still in `rx` are
    // dropped with it: their connections learn nothing will answer.
    drop(service);
    drop(readers);
    drop(rx);
    let _ = stop_acceptor(&sock, bound, &stop, acceptor);
}

/// The files a running daemon owns, removed when the serving function ends:
/// by returning, or by a panic unwinding out of the loop, which used to
/// leave a socket nobody answered and a pid file naming a dead process.
struct ReleaseOnExit([PathBuf; 3]);

impl Drop for ReleaseOnExit {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Take the registered watch: keep its guard and let the corpus catch up on
/// what changed meanwhile, or report why there is no watch.
fn watch_ready(
    service: &mut impl Corpus,
    ready: Result<(WatchGuard, WatchReport), String>,
) -> Option<WatchGuard> {
    match ready {
        Ok((guard, report)) => {
            service.watch_ready(&report);
            Some(guard)
        }
        Err(error) => {
            service.watcher_error(&error);
            None
        }
    }
}

/// Accept connections on a blocking listener and give each one a connection
/// thread that reads its requests and hands them to the loop, so an idle
/// daemon sleeps instead of polling the listener and a slow client never
/// holds the loop. The thread ends on `stop` (checked after each accept), on
/// an accept error, or once the loop has dropped its receiver.
fn spawn_acceptor(
    listener: UnixListener,
    tx: mpsc::Sender<Msg>,
    stop: Arc<AtomicBool>,
    root: PathBuf,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let slots = Arc::new(Slots::new(MAX_CONN_THREADS));
        for stream in listener.incoming() {
            if stop.load(Ordering::Acquire) {
                return;
            }
            let Ok(stream) = stream else {
                let _ = tx.send(Msg::AcceptFailed);
                return;
            };
            let slot = Slots::acquire(&slots);
            let mut forward = Forward {
                root: root.clone(),
                tx: tx.clone(),
            };
            std::thread::spawn(move || {
                let _slot = slot;
                let mut shutdown = false;
                handle_conn(&mut forward, stream, &mut shutdown);
            });
        }
    })
}

/// A bound on the connection threads alive at once.
struct Slots {
    busy: Mutex<usize>,
    freed: Condvar,
    cap: usize,
}

impl Slots {
    fn new(cap: usize) -> Self {
        Slots {
            busy: Mutex::new(0),
            freed: Condvar::new(),
            cap,
        }
    }

    /// Wait for a free slot and take it; dropping the guard gives it back.
    fn acquire(slots: &Arc<Slots>) -> Slot {
        let mut busy = slots.busy.lock().unwrap_or_else(PoisonError::into_inner);
        while *busy >= slots.cap {
            busy = slots
                .freed
                .wait(busy)
                .unwrap_or_else(PoisonError::into_inner);
        }
        *busy += 1;
        Slot(Arc::clone(slots))
    }
}

/// A taken slot, given back when its connection thread ends (a panic too).
struct Slot(Arc<Slots>);

impl Drop for Slot {
    fn drop(&mut self) {
        let mut busy = self.0.busy.lock().unwrap_or_else(PoisonError::into_inner);
        *busy -= 1;
        self.0.freed.notify_one();
    }
}

/// A connection thread's corpus: each request goes to the loop, which queues
/// it for the writer, and the answer comes back on its own channel.
struct Forward {
    root: PathBuf,
    tx: mpsc::Sender<Msg>,
}

impl Corpus for Forward {
    fn root(&self) -> &Path {
        &self.root
    }

    fn handle(&mut self, req: Request) -> Response {
        let op = req.op_name();
        let (reply, answer) = mpsc::channel();
        if self.tx.send(Msg::Request(req, reply)).is_err() {
            return failure_response(op, "daemon is shutting down");
        }
        answer
            .recv()
            .unwrap_or_else(|_| failure_response(op, "daemon is shutting down"))
    }

    /// Never called: the loop owns the watcher, not a connection.
    fn apply_change(&mut self, _abs: &Path, _removed: bool) {}
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

/// The `(device, inode)` of the served root when it is a directory, `None`
/// when it is gone or is no longer a directory.
fn root_identity(root: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(root)
        .ok()
        .filter(std::fs::Metadata::is_dir)
        .map(|meta| (meta.dev(), meta.ino()))
}

/// The served root is no longer the directory the daemon opened (`opened`):
/// deleted, replaced by a non-directory, or replaced in place by another
/// directory (an atomic `mv new old`, or `rm -rf` and `git clone` with no
/// gap between them). In every case the index and the watch describe a tree
/// that no longer exists at that path, so the loop exits and releases the
/// socket, pid and lock; the next hook starts a daemon on the new tree.
fn root_replaced(root: &Path, opened: Option<(u64, u64)>) -> bool {
    opened.is_none() || root_identity(root) != opened
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
fn record_event(root: &Path, ev: &notify::Event, pending: &mut BTreeMap<PathBuf, bool>) -> bool {
    if matches!(
        ev.kind,
        notify::EventKind::Access(_)
            | notify::EventKind::Modify(notify::event::ModifyKind::Metadata(_))
    ) {
        return false;
    }
    let mut recorded = false;
    for path in &ev.paths {
        let root_git_exclude = path
            .strip_prefix(root)
            .is_ok_and(|relative| relative == Path::new(".git/info/exclude"));
        // Only the components below the root: a repository that lives under
        // `~/.cache/` or `~/code/build/` is not itself ignored (#1194). A
        // path outside the root is judged whole, as before.
        let judged = path.strip_prefix(root).unwrap_or(path);
        if !root_git_exclude
            && judged.components().any(|c| match c {
                Component::Normal(name) => is_ignored_dir(name),
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
        recorded = true;
    }
    recorded
}

/// Only recorded events move the quiet deadline, capped from the first pending event.
fn note_event(
    root: &Path,
    ev: &notify::Event,
    pending: &mut BTreeMap<PathBuf, bool>,
    flush_at: &mut Option<Instant>,
    first_pending_at: &mut Option<Instant>,
) {
    if record_event(root, ev, pending) {
        arm_flush(flush_at, first_pending_at);
    }
}

/// An event that moved HEAD or a branch of the repository at `root`: a
/// write to `.git/HEAD`, its reflog, `packed-refs` or a branch ref. Reads
/// are not (every `git` run opens `HEAD`), and neither is `.git/index`,
/// which `git status` itself rewrites.
fn moves_head(root: &Path, ev: &notify::Event) -> bool {
    if matches!(
        ev.kind,
        notify::EventKind::Access(_)
            | notify::EventKind::Modify(notify::event::ModifyKind::Metadata(_))
    ) {
        return false;
    }
    let git_dir = root.join(".git");
    ev.paths.iter().any(|path| {
        path.strip_prefix(&git_dir).is_ok_and(|rel| {
            rel == Path::new("HEAD")
                || rel == Path::new("logs/HEAD")
                || rel == Path::new("packed-refs")
                || rel.starts_with("refs/heads")
        })
    })
}

/// Push the quiet deadline a `DEBOUNCE` away, capped at `MAX_PENDING_WAIT`
/// from the first pending change.
fn arm_flush(flush_at: &mut Option<Instant>, first_pending_at: &mut Option<Instant>) {
    let now = Instant::now();
    let first = *first_pending_at.get_or_insert(now);
    *flush_at = Some((now + DEBOUNCE).min(first + MAX_PENDING_WAIT));
}

/// The directories an event created or moved in: a per-directory watch does
/// not see inside them until it is extended to them.
fn created_dirs(ev: &notify::Event) -> Vec<PathBuf> {
    if !matches!(
        ev.kind,
        notify::EventKind::Create(_)
            | notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
    ) {
        return Vec::new();
    }
    ev.paths.iter().filter(|p| p.is_dir()).cloned().collect()
}

/// Extend the watch to the directories created since the last call, and
/// record the files already inside them as pending changes: they were
/// written before any watch could see them. Without a watch yet, the
/// directories wait for it.
fn extend_watch(
    watch: Option<&mut WatchGuard>,
    created: &mut Vec<PathBuf>,
    service: &mut impl Corpus,
    pending: &mut BTreeMap<PathBuf, bool>,
    flush_at: &mut Option<Instant>,
    first_pending_at: &mut Option<Instant>,
) {
    let Some(watch) = watch else {
        return;
    };
    let mut recorded = false;
    for dir in std::mem::take(created) {
        match watch.extend(&dir) {
            Ok(files) => {
                for file in files {
                    pending.insert(file, false);
                    recorded = true;
                }
            }
            Err(error) => service.watcher_error(&error),
        }
    }
    if recorded {
        arm_flush(flush_at, first_pending_at);
    }
}

/// Apply the debounced watcher batch now, when there is one. The loop calls
/// this before every connection so a request following a mutation cannot
/// read an index built before it, and again on the debounce timer.
fn flush_pending(
    service: &mut dyn Corpus,
    pending: &mut BTreeMap<PathBuf, bool>,
    flush_at: &mut Option<Instant>,
    first_pending_at: &mut Option<Instant>,
) {
    if pending.is_empty() {
        return;
    }
    let batch: Vec<(PathBuf, bool)> = std::mem::take(pending).into_iter().collect();
    service.apply_changes(&batch);
    *flush_at = None;
    *first_pending_at = None;
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
                (answer(service, req), is_shutdown)
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

/// One request's answer. A panic in the handler becomes a failure envelope
/// naming it, instead of unwinding out of the loop and taking the daemon
/// down; the locks it held recover on their next acquisition
/// ([`crate::api::RecoverLock`]).
fn answer(service: &mut dyn Corpus, req: Request) -> Response {
    let op = req.op_name();
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| service.handle(req))).unwrap_or_else(
        |payload| {
            failure_response(
                op,
                format!("request panicked: {}", panic_message(payload.as_ref())),
            )
        },
    )
}

/// The text a panic was raised with (`panic!("...")` carries a `&str`, a
/// formatted one a `String`).
fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload")
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
    read_capped_line_with(reader, buf, max_bytes, deadline, Instant::now)
}

/// [`read_capped_line`] over any buffered reader, with the clock passed in.
/// The line is taken a buffered block at a time, and the deadline is
/// checked once per block, not once per byte.
fn read_capped_line_with(
    reader: &mut impl BufRead,
    buf: &mut String,
    max_bytes: usize,
    deadline: Instant,
    mut now: impl FnMut() -> Instant,
) -> ReadResult {
    let mut bytes = Vec::with_capacity(max_bytes.min(4096));
    loop {
        if now() >= deadline {
            return ReadResult::TimedOut;
        }
        let Ok(block) = reader.fill_buf() else {
            return ReadResult::Err;
        };
        if block.is_empty() {
            if bytes.is_empty() {
                return ReadResult::Eof;
            }
            break;
        }
        let newline = block.iter().position(|&b| b == b'\n');
        let line_part = newline.unwrap_or(block.len());
        if bytes.len() + line_part > max_bytes {
            return ReadResult::TooLong;
        }
        bytes.extend_from_slice(&block[..line_part]);
        match newline {
            Some(at) => {
                reader.consume(at + 1);
                break;
            }
            None => reader.consume(line_part),
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

    /// A watch that covers nothing: the loop tests drive events by hand.
    impl Watch for () {
        fn extend(&mut self, _dir: &Path) -> Result<Vec<PathBuf>, String> {
            Ok(Vec::new())
        }
    }

    /// The report of a watch registered at once on one root.
    const STUB_REPORT: WatchReport = WatchReport {
        dirs: 1,
        recursive: true,
        elapsed: Duration::ZERO,
        git_dir: false,
    };

    fn stub_watch() -> Registered {
        Registered {
            guard: Box::new(()),
            dirs: 1,
            recursive: true,
            git_dir: false,
        }
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

    /// The daemon socket is the only untrusted-to-Pixel channel on the
    /// machine: anything that can connect can ask for the repository's facts.
    /// `run_corpus_with` narrows it to 0600 *after* `bind`, which is the
    /// window a test has to read back — `bind` creates the node with the
    /// process umask, so the mode is only ever proven by stat-ing the file
    /// the daemon actually left behind.
    ///
    /// The assertion is the exact mode, not "no group/other bits": a socket
    /// left at 0400 satisfies a `mode & 0o077 == 0` check while denying the
    /// owner the access the socket needs, so the mask alone would pass a mode
    /// that is not the required 0600. Deleting the `set_permissions` call
    /// leaves the umask's mode and fails this test.
    #[cfg(unix)]
    #[test]
    fn the_daemon_socket_should_be_0600_after_bind() {
        use std::os::unix::fs::PermissionsExt;

        let root = scratch_root("socket-mode");
        let sock = socket_path(&root);
        let served = root.clone();
        let daemon = std::thread::spawn(move || run_corpus(StubCorpus(served)));
        wait_until("socket to answer", Duration::from_secs(10), || ping(&sock));

        let mode = std::fs::metadata(&sock)
            .expect("stat the bound socket")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "the daemon socket must be 0600 once bound, so no other local \
             account can reach the repository's facts through it"
        );

        let _response = shutdown(&sock).expect("shutdown request must receive a response");
        wait_until("daemon to exit", Duration::from_secs(10), || {
            daemon.is_finished()
        });
        daemon.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The accept thread gives each connection a thread that hands its
    /// requests to the loop's channel, and stops when told to: the daemon's
    /// own wake-up connection ends it, so `run_corpus` returns without a
    /// thread still blocked in `accept`.
    #[test]
    fn acceptor_should_forward_connections_and_stop_when_woken() {
        let root = scratch_root("acceptor");
        let sock = root.join("a.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let acceptor = spawn_acceptor(listener, tx, Arc::clone(&stop), root.clone());

        let mut client = UnixStream::connect(&sock).unwrap();
        writeln!(client, "{}", serde_json::to_string(&Request::Ping).unwrap()).unwrap();
        let Ok(Msg::Request(req, reply)) = rx.recv_timeout(Duration::from_secs(5)) else {
            panic!("the connection's request must reach the loop");
        };
        assert_eq!(req, Request::Ping);
        reply
            .send(Response::success("ping", serde_json::json!({"pong": true})))
            .unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut line = String::new();
        BufReader::new(&client).read_line(&mut line).unwrap();
        let answered: Response = serde_json::from_str(&line).unwrap();
        assert!(answered.ok, "the loop's answer reaches the client: {line}");

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

    struct FlushedCorpus {
        root: PathBuf,
        applied: mpsc::Sender<(Instant, Vec<(PathBuf, bool)>)>,
    }

    impl Corpus for FlushedCorpus {
        fn root(&self) -> &Path {
            &self.root
        }
        fn handle(&mut self, _req: Request) -> Response {
            failure_response("stub", "stub corpus")
        }
        fn apply_change(&mut self, _abs: &Path, _removed: bool) {}
        fn apply_changes(&mut self, changes: &[(PathBuf, bool)]) {
            let _ = self.applied.send((Instant::now(), changes.to_vec()));
        }
    }

    /// Wait on the production event loop's apply notification, without a client
    /// connection that could force the pending batch to flush prematurely.
    fn autonomous_flush(tag: &str, noise: Option<&str>, limit: Duration) -> Duration {
        let root = scratch_root(tag);
        let source = root.join("edited.rs");
        std::fs::write(&source, "fn edited() {}\n").unwrap();
        let (applied, flushed) = mpsc::channel();
        let corpus = FlushedCorpus {
            root: root.clone(),
            applied,
        };
        let (registered, watch) = mpsc::channel();
        let register: Registrar = Box::new(move |_, tx| {
            registered.send(tx).unwrap();
            Ok(stub_watch())
        });
        let daemon = std::thread::spawn(move || run_corpus_with(corpus, register));
        let events = watch
            .recv_timeout(Duration::from_secs(3))
            .expect("watch registration");
        let changed = |path| {
            notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Any,
            )))
            .add_path(path)
        };
        let started = Instant::now();
        events.send(Msg::Fs(changed(source.clone()))).unwrap();
        let (stop, stopped) = mpsc::channel();
        let noise_path = noise.map(|path| root.join(path));
        let emitter = std::thread::spawn(move || {
            if let Some(path) = noise_path {
                for _ in 0..200 {
                    // This is the input event cadence, not an assertion sleep.
                    if stopped.recv_timeout(Duration::from_millis(25)).is_ok() {
                        return;
                    }
                    if events.send(Msg::Fs(changed(path.clone()))).is_err() {
                        return;
                    }
                }
            }
        });
        let result = flushed.recv_timeout(limit);
        let _ = stop.send(());
        emitter.join().unwrap();
        let sock = socket_path(&root);
        let stopped = shutdown(&sock);
        daemon.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            stopped.is_ok(),
            "daemon must stop after the timed observation"
        );
        let (at, batch) = result.expect("the timer must apply the source edit without any request");
        assert_eq!(
            batch,
            [(source, false)],
            "ignored paths never enter a batch"
        );
        at.duration_since(started)
    }

    #[test]
    fn ignored_events_every_25_ms_should_not_delay_a_pending_edit() {
        let elapsed = autonomous_flush(
            "ignored-debounce",
            Some(".pixel/noise"),
            Duration::from_millis(1500),
        );
        assert!(
            elapsed >= Duration::from_millis(500),
            "retain the quiet debounce"
        );
    }

    #[test]
    fn continuous_edits_should_flush_by_the_maximum_wait() {
        let elapsed = autonomous_flush(
            "bounded-debounce",
            Some("edited.rs"),
            Duration::from_secs(3),
        );
        assert!(
            elapsed >= Duration::from_secs(2),
            "real edits extend the quiet deadline up to the cap"
        );
    }

    #[test]
    fn a_single_edit_should_flush_after_the_debounce_without_a_request() {
        let elapsed = autonomous_flush("single-debounce", None, Duration::from_millis(1500));
        assert!(
            elapsed >= Duration::from_millis(500),
            "do not flush before the debounce"
        );
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
        let mut first_pending_at = None;

        let read = notify::Event::new(EventKind::Access(AccessKind::Any)).add_path(file.clone());
        note_event(
            &root,
            &read,
            &mut pending,
            &mut flush_at,
            &mut first_pending_at,
        );
        assert_eq!(flush_at, None);

        let before = Instant::now();
        let create = notify::Event::new(EventKind::Create(CreateKind::File)).add_path(file.clone());
        note_event(
            &root,
            &create,
            &mut pending,
            &mut flush_at,
            &mut first_pending_at,
        );
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
        let acceptor = spawn_acceptor(listener, tx, Arc::clone(&stop), root.clone());
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
        let acceptor = spawn_acceptor(listener, tx, Arc::clone(&stop), root.clone());
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
        let mut first_pending_at = None;

        flush_pending(
            &mut corpus,
            &mut pending,
            &mut flush_at,
            &mut first_pending_at,
        );
        assert!(
            batches.lock().unwrap().is_empty(),
            "an empty batch must not reach the corpus"
        );

        pending.insert(changed.clone(), false);
        flush_at = Some(Instant::now() + Duration::from_secs(60));
        first_pending_at = Some(Instant::now() - MAX_PENDING_WAIT);
        flush_pending(
            &mut corpus,
            &mut pending,
            &mut flush_at,
            &mut first_pending_at,
        );
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
        assert_eq!(
            first_pending_at, None,
            "a new batch gets a new maximum wait"
        );
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

    /// A repository whose own path crosses a directory named like an ignored
    /// one (`~/.cache/…`, `~/code/build/…`) still records its edits; only
    /// the trees below its root are judged. Before #1194 every event of such
    /// a repository was dropped, and nothing reached the index or the graph.
    #[test]
    fn record_event_should_judge_only_the_components_below_the_root() {
        use notify::event::{DataChange, ModifyKind};

        let base = scratch_root("ignored-ancestor");
        for parent in [".cache", "build", "vendor"] {
            let root = base.join(parent).join("repo");
            let source = root.join("app/models/link.rb");
            let below = root.join("node_modules/pkg/index.js");
            for path in [&source, &below] {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, "changed\n").unwrap();
            }
            let event =
                notify::Event::new(notify::EventKind::Modify(ModifyKind::Data(DataChange::Any)))
                    .add_path(source.clone())
                    .add_path(below.clone());
            let mut pending = BTreeMap::new();
            assert!(record_event(&root, &event, &mut pending), "under {parent}");
            assert_eq!(pending.get(&source), Some(&false), "under {parent}");
            assert!(
                !pending.contains_key(&below),
                "an ignored tree below the root stays ignored, under {parent}"
            );
        }
        let _ = std::fs::remove_dir_all(&base);
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

    /// Counts the loop's watch callbacks; watches its root like `Service`.
    struct WatchedCorpus {
        root: PathBuf,
        ready: Arc<AtomicUsize>,
        errors: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl Corpus for WatchedCorpus {
        fn root(&self) -> &Path {
            &self.root
        }
        fn handle(&mut self, _req: Request) -> Response {
            failure_response("stub", "stub corpus")
        }
        fn apply_change(&mut self, _abs: &Path, _removed: bool) {}
        fn watch_ready(&mut self, _report: &WatchReport) {
            self.ready.fetch_add(1, Ordering::SeqCst);
        }
        fn watcher_error(&mut self, error: &str) {
            self.errors.lock().unwrap().push(error.to_string());
        }
    }

    /// A repository whose recursive watch takes seconds to register (11 515
    /// ignored `node_modules` directories: 11 to 28 s) still answers its
    /// first request at once, and the corpus catches up once the watch lands.
    #[test]
    fn a_slow_watch_registration_should_not_delay_the_first_answer() {
        let root = scratch_root("slow-watch");
        let sock = socket_path(&root);
        let ready = Arc::new(AtomicUsize::new(0));
        let corpus = WatchedCorpus {
            root: root.clone(),
            ready: Arc::clone(&ready),
            errors: Arc::default(),
        };
        let (release, gate) = mpsc::channel::<()>();
        let registered = Arc::new(AtomicBool::new(false));
        let registered_flag = Arc::clone(&registered);
        let register: Registrar = Box::new(move |paths, _tx| {
            assert_eq!(paths.len(), 1, "the corpus root is watched");
            // Held until the test has been answered; never longer than 5 s.
            let _ = gate.recv_timeout(Duration::from_secs(5));
            registered_flag.store(true, Ordering::SeqCst);
            Ok(stub_watch())
        });
        let started = Instant::now();
        let daemon = std::thread::spawn(move || run_corpus_with(corpus, register));
        wait_until("socket to answer", Duration::from_secs(4), || ping(&sock));
        assert!(
            !registered.load(Ordering::SeqCst),
            "the answer waited for the watch registration ({:?})",
            started.elapsed()
        );
        assert_eq!(
            ready.load(Ordering::SeqCst),
            0,
            "ready before the watch landed"
        );

        release.send(()).unwrap();
        wait_until("the watch to land", Duration::from_secs(10), || {
            ping(&sock) && ready.load(Ordering::SeqCst) == 1
        });
        assert!(ping(&sock));
        assert_eq!(ready.load(Ordering::SeqCst), 1, "watch_ready runs once");

        let _response = shutdown(&sock).expect("shutdown request must receive a response");
        wait_until("daemon to exit", Duration::from_secs(10), || {
            daemon.is_finished()
        });
        daemon.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A watch that cannot be registered is reported as a watcher error (the
    /// index may go stale) instead of failing the daemon, which keeps serving.
    #[test]
    fn a_failed_watch_registration_should_be_reported_and_keep_serving() {
        let root = scratch_root("failed-watch");
        let sock = socket_path(&root);
        let ready = Arc::new(AtomicUsize::new(0));
        let errors: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let corpus = WatchedCorpus {
            root: root.clone(),
            ready: Arc::clone(&ready),
            errors: Arc::clone(&errors),
        };
        let register: Registrar = Box::new(|_paths, _tx| Err("watch limit reached".into()));
        let daemon = std::thread::spawn(move || run_corpus_with(corpus, register));
        wait_until("the error to be reported", Duration::from_secs(10), || {
            ping(&sock) && !errors.lock().unwrap().is_empty()
        });
        assert_eq!(
            *errors.lock().unwrap(),
            vec!["watch limit reached".to_string()]
        );
        assert_eq!(ready.load(Ordering::SeqCst), 0);
        assert!(ping(&sock), "the daemon keeps serving without a watch");

        let _response = shutdown(&sock).expect("shutdown request must receive a response");
        wait_until("daemon to exit", Duration::from_secs(10), || {
            daemon.is_finished()
        });
        daemon.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    fn search_hits(svc: &mut Service, pattern: &str) -> usize {
        let resp = svc.handle(Request::Search {
            paths: None,
            pattern: pattern.into(),
            json: true,
            limit: None,
            offset: None,
            scope: None,
            globs: Vec::new(),
            types: Vec::new(),
        });
        assert!(resp.ok, "search {pattern}: {resp:?}");
        resp.data()
            .get("matches")
            .and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len)
    }

    /// Edits made before the watch was live reach no event: `watch_ready`
    /// re-reads what `git status` lists, so a modified, a new and a deleted
    /// file are all current afterwards.
    #[test]
    fn service_watch_ready_should_catch_up_on_unwatched_edits() {
        let root = scratch_root("service-watch-ready");
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&["init", "-q"]);
        std::fs::write(root.join("a.rs"), "fn alphaNeedle() {}\n").unwrap();
        std::fs::write(root.join("b.rs"), "fn deltaNeedle() {}\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "initial"]);
        let mut svc = Service::open(&root).unwrap();
        assert_eq!(search_hits(&mut svc, "alphaNeedle"), 1);

        // Changed with no watcher running.
        std::fs::write(root.join("a.rs"), "fn betaNeedle() {}\n").unwrap();
        std::fs::write(root.join("c.rs"), "fn gammaNeedle() {}\n").unwrap();
        std::fs::remove_file(root.join("b.rs")).unwrap();
        assert_eq!(
            search_hits(&mut svc, "betaNeedle"),
            0,
            "no event, no refresh"
        );

        Corpus::watch_ready(&mut svc, &STUB_REPORT);
        assert_eq!(search_hits(&mut svc, "betaNeedle"), 1);
        assert_eq!(search_hits(&mut svc, "alphaNeedle"), 0);
        assert_eq!(search_hits(&mut svc, "gammaNeedle"), 1);
        assert_eq!(search_hits(&mut svc, "deltaNeedle"), 0);
        drop(svc);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `git` in `root` with a fixed identity and no user config.
    fn git_in(root: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// One committed `a.rs` defining `alphaNeedle`.
    fn committed_repo(tag: &str) -> PathBuf {
        let root = scratch_root(tag);
        git_in(&root, &["init", "-q"]);
        std::fs::write(root.join("a.rs"), "fn alphaNeedle() {}\n").unwrap();
        git_in(&root, &["add", "."]);
        git_in(&root, &["commit", "-qm", "initial"]);
        root
    }

    /// An edit and a deletion the daemon opened with, both discarded before
    /// the watch was live (`git checkout -- a.rs b.rs`), leave `git status`
    /// clean: the overlay still answers with the discarded text and hides the
    /// restored file unless `watch_ready` re-reads what it held.
    #[test]
    fn service_watch_ready_should_drop_an_edit_discarded_before_the_watch() {
        let root = committed_repo("watch-ready-discard");
        std::fs::write(root.join("b.rs"), "fn betaNeedle() {}\n").unwrap();
        git_in(&root, &["add", "b.rs"]);
        git_in(&root, &["commit", "-qm", "b"]);
        std::fs::write(root.join("a.rs"), "fn draftNeedle() {}\n").unwrap();
        std::fs::remove_file(root.join("b.rs")).unwrap();
        let mut svc = Service::open(&root).unwrap();
        assert_eq!(search_hits(&mut svc, "draftNeedle"), 1);
        assert_eq!(search_hits(&mut svc, "betaNeedle"), 0);

        git_in(&root, &["checkout", "--", "a.rs", "b.rs"]);
        Corpus::watch_ready(&mut svc, &STUB_REPORT);
        assert_eq!(search_hits(&mut svc, "draftNeedle"), 0, "discarded edit");
        assert_eq!(search_hits(&mut svc, "alphaNeedle"), 1, "committed text");
        assert_eq!(search_hits(&mut svc, "betaNeedle"), 1, "restored deletion");
        drop(svc);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A branch switch before the watch was live rewrites files `git status`
    /// never lists: `watch_ready` re-reads what HEAD's move changed.
    /// The catch-up re-reads only what may have changed: it diffs from the
    /// HEAD the index opened at and re-reads the overlay's paths. Without the
    /// opened HEAD it would diff from the empty tree and re-read every
    /// tracked file of a large repository after each daemon start.
    #[test]
    fn index_catch_up_should_report_the_opened_head_and_only_the_overlay_paths() {
        let root = committed_repo("index-catch-up");
        std::fs::write(root.join("u.rs"), "fn untrackedNeedle() {}\n").unwrap();
        let svc = Service::open(&root).unwrap();
        let head = String::from_utf8(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let (opened, paths) = svc.index_catch_up();
        assert_eq!(opened.as_deref(), Some(head.trim()));
        assert_eq!(
            paths,
            std::collections::BTreeSet::from(["u.rs".to_string()])
        );
        drop(svc);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A repository opened before its first commit has no HEAD to diff
    /// from; a file added and committed during registration is clean in
    /// `git status` and absent from the overlay, so only the empty-tree
    /// diff brings it in.
    #[test]
    fn service_watch_ready_should_pick_up_a_first_commit_before_the_watch() {
        let root = scratch_root("watch-ready-first-commit");
        git_in(&root, &["init", "-q"]);
        let mut svc = Service::open(&root).unwrap();

        std::fs::write(root.join("a.rs"), "fn firstNeedle() {}\n").unwrap();
        git_in(&root, &["add", "a.rs"]);
        git_in(&root, &["commit", "-qm", "first"]);
        assert_eq!(search_hits(&mut svc, "firstNeedle"), 0, "no event yet");
        Corpus::watch_ready(&mut svc, &STUB_REPORT);
        assert_eq!(search_hits(&mut svc, "firstNeedle"), 1);
        drop(svc);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn service_watch_ready_should_follow_a_head_move_before_the_watch() {
        let root = committed_repo("watch-ready-head-move");
        git_in(&root, &["checkout", "-qb", "other"]);
        std::fs::write(root.join("a.rs"), "fn otherNeedle() {}\n").unwrap();
        std::fs::write(root.join("n.rs"), "fn newNeedle() {}\n").unwrap();
        git_in(&root, &["add", "."]);
        git_in(&root, &["commit", "-qm", "other"]);
        git_in(&root, &["checkout", "-q", "-"]);
        let mut svc = Service::open(&root).unwrap();
        assert_eq!(search_hits(&mut svc, "alphaNeedle"), 1);

        git_in(&root, &["checkout", "-q", "other"]);
        Corpus::watch_ready(&mut svc, &STUB_REPORT);
        assert_eq!(search_hits(&mut svc, "otherNeedle"), 1, "modified on HEAD");
        assert_eq!(search_hits(&mut svc, "newNeedle"), 1, "added on HEAD");
        assert_eq!(search_hits(&mut svc, "alphaNeedle"), 0, "old text");
        let head = pixel_index::gitsync::rev_parse_head(&root);
        assert_eq!(
            svc.index_covered_head(),
            head,
            "the catch-up tells the freshness gate the index covers HEAD"
        );

        git_in(&root, &["checkout", "-q", "-"]);
        Corpus::watch_ready(&mut svc, &STUB_REPORT);
        assert_eq!(
            search_hits(&mut svc, "newNeedle"),
            0,
            "removed by the move back"
        );
        assert_eq!(search_hits(&mut svc, "alphaNeedle"), 1);
        drop(svc);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A directory swapped in at the root's path (an atomic `mv`, a re-clone
    /// with no gap) stops the daemon like a deletion does (#1107): it used to
    /// keep serving the old index with a dead watch.
    #[test]
    fn daemon_exits_once_its_root_is_replaced_in_place() {
        let root = scratch_root("root-replaced");
        let sock = socket_path(&root);
        let served = root.clone();
        let daemon = std::thread::spawn(move || run_corpus(StubCorpus(served)));
        wait_until("socket to answer", Duration::from_secs(10), || ping(&sock));
        std::thread::sleep(Duration::from_millis(300));
        assert!(ping(&sock), "daemon stopped serving while its root existed");

        let staged = root.with_extension("staged");
        let old = root.with_extension("old");
        for dir in [&staged, &old] {
            let _ = std::fs::remove_dir_all(dir);
        }
        std::fs::create_dir(&staged).unwrap();
        std::fs::rename(&root, &old).unwrap();
        std::fs::rename(&staged, &root).unwrap();
        assert!(root.is_dir(), "a directory is at the path again");
        wait_until(
            "daemon to exit after the root was replaced",
            ROOT_POLL + Duration::from_secs(10),
            || daemon.is_finished(),
        );
        daemon.join().unwrap().unwrap();
        assert!(!sock.exists(), "socket must be released on exit");
        for dir in [&root, &old] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// A corpus whose first request panics and whose later ones answer.
    struct PanicOnceCorpus {
        root: PathBuf,
        panicked: AtomicBool,
    }

    impl Corpus for PanicOnceCorpus {
        fn root(&self) -> &Path {
            &self.root
        }
        fn handle(&mut self, _req: Request) -> Response {
            assert!(
                self.panicked.swap(true, Ordering::SeqCst),
                "first request panics"
            );
            Response::success("ping", serde_json::json!({"pong": true}))
        }
        fn apply_change(&mut self, _abs: &Path, _removed: bool) {}
        fn watch_paths(&self) -> Vec<PathBuf> {
            Vec::new()
        }
    }

    /// A corpus whose periodic sweep panics: a panic outside any request,
    /// which unwinds out of the serving loop.
    struct PanickingSweepCorpus(PathBuf);

    impl Corpus for PanickingSweepCorpus {
        fn root(&self) -> &Path {
            &self.0
        }
        fn handle(&mut self, _req: Request) -> Response {
            failure_response("stub", "stub corpus")
        }
        fn apply_change(&mut self, _abs: &Path, _removed: bool) {}
        fn watch_paths(&self) -> Vec<PathBuf> {
            Vec::new()
        }
        fn sweep_interval(&self) -> Option<Duration> {
            Some(Duration::from_millis(50))
        }
        fn sweep(&mut self) {
            panic!("sweep panicked");
        }
    }

    /// One request on the socket and its decoded reply.
    fn request(sock: &Path, req: &Request) -> serde_json::Value {
        let mut stream = UnixStream::connect(sock).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        writeln!(stream, "{}", serde_json::to_string(req).unwrap()).unwrap();
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply).unwrap();
        serde_json::from_str(&reply).unwrap_or_else(|e| panic!("{e}: {reply:?}"))
    }

    /// A panic in a request handler degrades that one answer to a failure
    /// naming the panic (#1102); the daemon keeps serving the next request.
    #[test]
    fn a_panicking_request_should_fail_alone_and_the_next_ping_should_answer() {
        let root = scratch_root("panic-once");
        let sock = socket_path(&root);
        let served = root.clone();
        let daemon = std::thread::spawn(move || {
            run_corpus(PanicOnceCorpus {
                root: served,
                panicked: AtomicBool::new(false),
            })
        });
        wait_until("socket to bind", Duration::from_secs(10), || sock.exists());

        let failed = request(&sock, &Request::Ping);
        assert_eq!(failed["ok"], false, "{failed}");
        assert_eq!(failed["op"], "ping", "{failed}");
        assert_eq!(
            failed["error"]["message"], "request panicked: first request panics",
            "{failed}"
        );
        let answered = request(&sock, &Request::Ping);
        assert_eq!(answered["ok"], true, "{answered}");
        assert!(!daemon.is_finished(), "the panic took the daemon down");

        shutdown(&sock).expect("shutdown after the panic");
        wait_until("daemon to exit", Duration::from_secs(10), || {
            daemon.is_finished()
        });
        daemon.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A panic that does unwind out of the loop still releases the socket,
    /// pid and lock files, which a dead daemon used to leave behind.
    #[test]
    fn a_panic_out_of_the_loop_should_release_the_daemon_files() {
        let root = scratch_root("panic-sweep");
        let sock = socket_path(&root);
        let lock = pid_path(&root).with_extension("lock");
        let served = root.clone();
        let daemon = std::thread::spawn(move || run_corpus(PanickingSweepCorpus(served)));
        wait_until("the sweep to panic", Duration::from_secs(10), || {
            daemon.is_finished()
        });
        assert!(daemon.join().is_err(), "the sweep panic unwinds the loop");
        assert!(!sock.exists(), "socket left behind");
        assert!(!pid_path(&root).exists(), "pid file left behind");
        assert!(!lock.exists(), "lock file left behind");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `root` relative paths of `paths`, sorted, `.` for the root itself.
    fn relative(root: &Path, paths: &[PathBuf]) -> Vec<String> {
        let mut out: Vec<String> = paths
            .iter()
            .map(|p| {
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                if rel.is_empty() { ".".to_string() } else { rel }
            })
            .collect();
        out.sort();
        out
    }

    /// A tree with tracked sources beside ignored directories at the top
    /// and nested inside a source directory.
    fn mixed_tree(tag: &str) -> PathBuf {
        let root = scratch_root(tag);
        for dir in [
            "src/a/b",
            "node_modules/pkg/lib",
            "src/node_modules/dep",
            "src/build/out",
            ".git/info",
            ".git/objects/ab",
        ] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        for file in [
            "src/a/f.rs",
            "node_modules/pkg/lib/g.js",
            "src/build/out/h.o",
            "top.rs",
        ] {
            std::fs::write(root.join(file), "x\n").unwrap();
        }
        root
    }

    /// The watch skips every directory the event filter drops anyway, at
    /// any depth, and keeps the sources beside them.
    #[test]
    fn unignored_tree_should_skip_ignored_directories_at_any_depth() {
        let root = mixed_tree("unignored-tree");
        let tree = unignored_tree(&root, true);
        assert_eq!(
            relative(&root, &tree.dirs),
            [".", "src", "src/a", "src/a/b"]
        );
        assert_eq!(relative(&root, &tree.files), ["src/a/f.rs", "top.rs"]);
        assert!(
            unignored_tree(&root, false).files.is_empty(),
            "the registration walk collects no files"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `.git` is pruned, but its `info/exclude` is an ignore-control file the
    /// event filter keeps, so its directory is watched on its own.
    #[test]
    fn pruned_watch_dirs_should_add_the_git_info_directory() {
        let root = mixed_tree("pruned-dirs");
        assert_eq!(
            relative(&root, &pruned_watch_dirs(std::slice::from_ref(&root))),
            [".", ".git", ".git/info", "src", "src/a", "src/a/b"]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Only the components below the watched root count: a root that
    /// itself sits under a directory named `build` is still watched.
    #[test]
    fn ignored_below_should_judge_only_the_components_under_the_root() {
        let root = Path::new("/work/build/repo");
        assert!(!ignored_below(root, Path::new("/work/build/repo/src")));
        assert!(ignored_below(
            root,
            Path::new("/work/build/repo/src/node_modules/x")
        ));
        assert!(ignored_below(root, Path::new("/work/build/repo/target")));
        assert!(!ignored_below(root, Path::new("/elsewhere/target")));
    }

    #[test]
    fn created_dirs_should_take_existing_directories_from_create_and_rename_events() {
        let root = mixed_tree("created-dirs");
        let dir = root.join("src/a");
        let file = root.join("top.rs");
        let event = |kind| {
            notify::Event::new(kind)
                .add_path(dir.clone())
                .add_path(file.clone())
        };
        let create = event(notify::EventKind::Create(notify::event::CreateKind::Folder));
        assert_eq!(created_dirs(&create), std::slice::from_ref(&dir));
        let renamed = event(notify::EventKind::Modify(notify::event::ModifyKind::Name(
            notify::event::RenameMode::To,
        )));
        assert_eq!(created_dirs(&renamed), std::slice::from_ref(&dir));
        let modified = event(notify::EventKind::Modify(notify::event::ModifyKind::Data(
            notify::event::DataChange::Any,
        )));
        assert!(
            created_dirs(&modified).is_empty(),
            "a data change creates nothing"
        );
        let removed = event(notify::EventKind::Remove(notify::event::RemoveKind::Folder));
        assert!(created_dirs(&removed).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn moves_head_should_take_only_writes_to_the_head_and_branch_refs() {
        let root = Path::new("/repo");
        let event = |kind, path: &str| notify::Event::new(kind).add_path(root.join(path));
        let write = || {
            notify::EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Any,
            ))
        };
        for path in [
            ".git/HEAD",
            ".git/logs/HEAD",
            ".git/packed-refs",
            ".git/refs/heads/main",
        ] {
            assert!(moves_head(root, &event(write(), path)), "{path}");
        }
        let created = notify::EventKind::Create(notify::event::CreateKind::File);
        assert!(moves_head(root, &event(created, ".git/refs/heads/topic/x")));
        for path in [
            ".git/index",
            ".git/refs/remotes/origin/main",
            ".git/logs/refs/heads/main",
            "HEAD",
            "src/HEAD",
        ] {
            assert!(!moves_head(root, &event(write(), path)), "{path}");
        }
        let read = notify::EventKind::Access(notify::event::AccessKind::Open(
            notify::event::AccessMode::Read,
        ));
        assert!(
            !moves_head(root, &event(read, ".git/HEAD")),
            "a read moves nothing"
        );
    }

    /// Records the directories the loop extends the watch to, and answers
    /// with the files it was told are inside.
    struct ExtendingWatch {
        extended: mpsc::Sender<PathBuf>,
        files: Vec<PathBuf>,
    }

    impl Watch for ExtendingWatch {
        fn extend(&mut self, dir: &Path) -> Result<Vec<PathBuf>, String> {
            let _ = self.extended.send(dir.to_path_buf());
            Ok(std::mem::take(&mut self.files))
        }
    }

    /// A directory created while the watch was still registering waits for
    /// it: once live, the watch is extended to that directory and the file
    /// already inside reaches the corpus with no event of its own.
    #[test]
    fn a_directory_created_before_the_watch_landed_should_be_extended_into_it() {
        let root = scratch_root("extend-early");
        let fresh = root.join("fresh");
        std::fs::create_dir_all(&fresh).unwrap();
        let inside = fresh.join("inside.rs");
        std::fs::write(&inside, "fn inside() {}\n").unwrap();
        let (applied, flushed) = mpsc::channel();
        let corpus = FlushedCorpus {
            root: root.clone(),
            applied,
        };
        let (extended, extensions) = mpsc::channel();
        let created = fresh.clone();
        let files = vec![inside.clone()];
        let register: Registrar = Box::new(move |_, tx| {
            let event =
                notify::Event::new(notify::EventKind::Create(notify::event::CreateKind::Folder))
                    .add_path(created);
            tx.send(Msg::Fs(event)).unwrap();
            Ok(Registered {
                guard: Box::new(ExtendingWatch { extended, files }),
                dirs: 1,
                recursive: false,
                git_dir: false,
            })
        });
        let daemon = std::thread::spawn(move || run_corpus_with(corpus, register));
        let dir = extensions.recv_timeout(Duration::from_secs(5));
        let batch = flushed.recv_timeout(Duration::from_secs(5));
        shutdown(&socket_path(&root)).expect("shutdown");
        daemon.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(
            dir.expect("the watch is extended to the new directory"),
            fresh
        );
        let (_, batch) = batch.expect("the file inside is applied");
        assert_eq!(batch, [(inside, false)]);
    }

    /// Hands the loop's watch report and applied batches to the test.
    struct ObservedCorpus {
        root: PathBuf,
        reports: mpsc::Sender<WatchReport>,
        applied: mpsc::Sender<Vec<(PathBuf, bool)>>,
        moved: mpsc::Sender<()>,
    }

    impl Corpus for ObservedCorpus {
        fn root(&self) -> &Path {
            &self.root
        }
        fn handle(&mut self, _req: Request) -> Response {
            failure_response("stub", "stub corpus")
        }
        fn apply_change(&mut self, _abs: &Path, _removed: bool) {}
        fn apply_changes(&mut self, changes: &[(PathBuf, bool)]) {
            let _ = self.applied.send(changes.to_vec());
        }
        fn watch_ready(&mut self, report: &WatchReport) {
            let _ = self.reports.send(*report);
        }
        fn git_moved(&mut self) {
            let _ = self.moved.send(());
        }
    }

    /// The pruned registration on the real backend: it watches exactly the
    /// directories that are not ignored, and a file written in a directory
    /// created afterwards still reaches the corpus, which a non-recursive
    /// watch alone never reports.
    #[test]
    fn a_pruned_watch_should_cover_a_directory_created_after_it() {
        let root = mixed_tree("pruned-live");
        let (reports, report) = mpsc::channel();
        let (applied, batches) = mpsc::channel();
        let (moved, head_moves) = mpsc::channel();
        let corpus = ObservedCorpus {
            root: root.clone(),
            reports,
            applied,
            moved,
        };
        let daemon =
            std::thread::spawn(move || run_corpus_with(corpus, Box::new(register_pruned_watch)));
        let report = report.recv_timeout(Duration::from_secs(10));
        let late = root.join("src/late/deeper");
        std::fs::create_dir_all(&late).unwrap();
        let file = late.join("late.rs");
        std::fs::write(&file, "fn late() {}\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = false;
        while !seen && Instant::now() < deadline {
            if let Ok(batch) = batches.recv_timeout(Duration::from_millis(200)) {
                seen = batch
                    .iter()
                    .any(|(path, removed)| path == &file && !removed);
            }
        }
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/topic\n").unwrap();
        let head_moved = head_moves.recv_timeout(Duration::from_secs(10));
        shutdown(&socket_path(&root)).expect("shutdown");
        daemon.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            head_moved.is_ok(),
            "a write to .git/HEAD never reached git_moved"
        );
        let report = report.expect("the watch registers");
        assert_eq!(report.dirs, 6, "root, .git, .git/info, src, src/a, src/a/b");
        assert!(report.git_dir);
        assert!(!report.recursive);
        assert!(
            seen,
            "the file in the late directory never reached the corpus"
        );
    }

    #[test]
    fn panic_message_should_read_str_and_string_payloads() {
        let str_payload: Box<dyn std::any::Any + Send> = Box::new("plain");
        let string_payload: Box<dyn std::any::Any + Send> = Box::new(String::from("formatted 7"));
        let other_payload: Box<dyn std::any::Any + Send> = Box::new(7_u8);
        assert_eq!(panic_message(str_payload.as_ref()), "plain");
        assert_eq!(panic_message(string_payload.as_ref()), "formatted 7");
        assert_eq!(
            panic_message(other_payload.as_ref()),
            "non-string panic payload"
        );
    }

    /// The reader the old loop read one byte at a time: the clock was read
    /// once per byte, 65 537 times for a 64 KiB request. Now it is read once
    /// per buffered block (8 KiB for a default `BufReader`).
    #[test]
    fn a_64_kib_line_should_read_the_clock_once_per_block() {
        let mut line = vec![b'x'; MAX_REQUEST_LINE];
        line.push(b'\n');
        let mut reader = BufReader::new(std::io::Cursor::new(line));
        let mut clock_reads = 0_usize;
        let mut out = String::new();
        let result = read_capped_line_with(
            &mut reader,
            &mut out,
            MAX_REQUEST_LINE,
            Instant::now() + Duration::from_secs(5),
            || {
                clock_reads += 1;
                Instant::now()
            },
        );
        assert!(matches!(result, ReadResult::Ok));
        assert_eq!(out.len(), MAX_REQUEST_LINE);
        assert_eq!(clock_reads, 9, "eight 8 KiB blocks, then the newline");
    }

    /// The cap is exact across block boundaries: a line of `max_bytes` is
    /// read, one more byte is refused.
    #[test]
    fn capped_line_should_admit_exactly_max_bytes_across_blocks() {
        let read = |input: &[u8]| {
            let mut reader = BufReader::with_capacity(2, std::io::Cursor::new(input.to_vec()));
            let mut out = String::new();
            let result = read_capped_line_with(
                &mut reader,
                &mut out,
                4,
                Instant::now() + Duration::from_secs(5),
                Instant::now,
            );
            (result, out)
        };
        let (result, out) = read(b"0123\n");
        assert!(matches!(result, ReadResult::Ok));
        assert_eq!(out, "0123");
        assert!(matches!(read(b"01234\n").0, ReadResult::TooLong));
        let (result, out) = read(b"01");
        assert!(
            matches!(result, ReadResult::Ok),
            "a last line without newline"
        );
        assert_eq!(out, "01");
        assert!(matches!(read(b"").0, ReadResult::Eof));
    }

    /// A connection carries several requests: reading one line leaves the
    /// next one in the buffer, even when both arrived in one block.
    #[test]
    fn capped_line_should_leave_the_next_request_in_the_buffer() {
        let mut reader = BufReader::new(std::io::Cursor::new(b"ab\ncd\n".to_vec()));
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut first = String::new();
        let mut second = String::new();
        assert!(matches!(
            read_capped_line_with(&mut reader, &mut first, 8, deadline, Instant::now),
            ReadResult::Ok
        ));
        assert!(matches!(
            read_capped_line_with(&mut reader, &mut second, 8, deadline, Instant::now),
            ReadResult::Ok
        ));
        assert_eq!((first.as_str(), second.as_str()), ("ab", "cd"));
    }

    /// A corpus that holds every `status` until the test releases it.
    struct HeldCorpus {
        root: PathBuf,
        started: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
        /// Gives the loop a constant `ping` answer.
        pong: bool,
    }

    impl Corpus for HeldCorpus {
        fn root(&self) -> &Path {
            &self.root
        }
        fn pong(&self) -> Option<Response> {
            self.pong
                .then(|| Response::success("ping", serde_json::json!({"pong": "loop"})))
        }
        fn handle(&mut self, req: Request) -> Response {
            if matches!(req, Request::Status {}) {
                self.started.send(()).unwrap();
                self.release
                    .recv_timeout(Duration::from_secs(10))
                    .expect("the test releases the held request");
            }
            Response::success(req.op_name(), serde_json::json!({"served": req.op_name()}))
        }
        fn apply_change(&mut self, _abs: &Path, _removed: bool) {}
        fn watch_paths(&self) -> Vec<PathBuf> {
            Vec::new()
        }
    }

    /// A shutdown that arrives while the writer runs a long request ends
    /// the daemon only after that request is answered: the loop closes the
    /// queue, it does not abandon what is in it (#1149).
    #[test]
    fn a_shutdown_should_wait_for_the_request_in_flight() {
        let root = scratch_root("held-shutdown");
        let sock = socket_path(&root);
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let served = root.clone();
        let daemon = std::thread::spawn(move || {
            run_corpus(HeldCorpus {
                root: served,
                started: started_tx,
                release: release_rx,
                pong: false,
            })
        });
        wait_until("socket to bind", Duration::from_secs(10), || sock.exists());

        let held_sock = sock.clone();
        let held = std::thread::spawn(move || request(&held_sock, &Request::Status {}));
        started_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the writer takes the held request");
        let bye_sock = sock.clone();
        let bye = std::thread::spawn(move || request(&bye_sock, &Request::Shutdown));
        // The shutdown reaches the loop and the queue while `status` runs.
        std::thread::sleep(Duration::from_millis(200));
        assert!(!daemon.is_finished(), "the daemon left a request in flight");
        release_tx.send(()).unwrap();

        let held = held.join().unwrap();
        assert_eq!(held["ok"], true, "{held}");
        assert_eq!(held["result"]["served"], "status", "{held}");
        let bye = bye.join().unwrap();
        assert_eq!(bye["ok"], true, "{bye}");
        assert_eq!(bye["op"], "shutdown", "{bye}");
        wait_until("daemon to exit", Duration::from_secs(10), || {
            daemon.is_finished()
        });
        daemon.join().unwrap().unwrap();
        assert!(!sock.exists(), "socket left behind");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Every callback the loop makes reaches the writer as a job, in the
    /// order it was made, and the writer runs them in that order: a watcher
    /// batch queued before a request is applied before it is answered.
    #[test]
    fn queued_callbacks_should_reach_the_writer_in_order() {
        let (jobs_tx, jobs_rx) = mpsc::channel();
        let mut queued = Queued::new(PathBuf::from("/repo"), jobs_tx, Arc::default());
        let (reply, answer) = mpsc::channel();
        queued.git_moved();
        queued.apply_changes(&[(PathBuf::from("/repo/a.rs"), false)]);
        queued.apply_change(Path::new("/repo/b.rs"), true);
        queued.watcher_error("lost");
        queued.watch_ready(&STUB_REPORT);
        queued.sweep();
        queued.serve(Request::Ping, reply);
        drop(queued);

        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut writer = LogCorpus(Arc::clone(&log));
        run_writer(&mut writer, &jobs_rx, &Busy::default());
        assert_eq!(
            *log.lock().unwrap(),
            [
                "git_moved",
                "apply /repo/a.rs false",
                "apply /repo/b.rs true",
                "watcher_error lost",
                "watch_ready 1",
                "sweep",
                "handle ping",
            ]
        );
        assert_eq!(answer.recv().unwrap().op, "ping");
    }

    /// Writes down every callback the writer makes, in order.
    struct LogCorpus(Arc<std::sync::Mutex<Vec<String>>>);

    impl Corpus for LogCorpus {
        fn root(&self) -> &Path {
            Path::new("/repo")
        }
        fn handle(&mut self, req: Request) -> Response {
            self.0
                .lock()
                .unwrap()
                .push(format!("handle {}", req.op_name()));
            Response::success(req.op_name(), serde_json::json!({}))
        }
        fn apply_change(&mut self, abs: &Path, removed: bool) {
            self.0
                .lock()
                .unwrap()
                .push(format!("apply {} {removed}", abs.display()));
        }
        fn git_moved(&mut self) {
            self.0.lock().unwrap().push("git_moved".into());
        }
        fn watcher_error(&mut self, error: &str) {
            self.0
                .lock()
                .unwrap()
                .push(format!("watcher_error {error}"));
        }
        fn watch_ready(&mut self, report: &WatchReport) {
            self.0
                .lock()
                .unwrap()
                .push(format!("watch_ready {}", report.dirs));
        }
        fn sweep(&mut self) {
            self.0.lock().unwrap().push("sweep".into());
        }
    }

    /// Once the writer is gone, a send marks the handle so the loop exits,
    /// and a waiting `handle` answers with a failure instead of hanging.
    #[test]
    fn queued_should_notice_a_writer_that_is_gone() {
        let (jobs_tx, jobs_rx) = mpsc::channel();
        let mut queued = Queued::new(PathBuf::from("/repo"), jobs_tx, Arc::default());
        queued.sweep();
        assert!(!queued.gone, "a live writer is not gone");
        drop(jobs_rx);
        let answered = queued.handle(Request::Status {});
        assert!(queued.gone);
        assert!(!answered.ok);
        assert_eq!(answered.op, "status");
        assert_eq!(
            answered.error.map(|e| e.message),
            Some("daemon writer stopped".to_string())
        );
    }

    /// A connection whose request cannot reach the loop, or gets no answer,
    /// is told the daemon is shutting down.
    #[test]
    fn forward_should_answer_shutting_down_when_the_loop_is_gone() {
        let (tx, rx) = mpsc::channel();
        let mut forward = Forward {
            root: PathBuf::from("/repo"),
            tx,
        };
        let loop_side = std::thread::spawn(move || {
            let Ok(Msg::Request(req, reply)) = rx.recv() else {
                panic!("the request reaches the loop");
            };
            assert_eq!(req, Request::Ping);
            drop(reply);
            // The loop goes away with the next request still unsent.
        });
        let unanswered = forward.handle(Request::Ping);
        loop_side.join().unwrap();
        let unsent = forward.handle(Request::Status {});
        for (reply, op) in [(unanswered, "ping"), (unsent, "status")] {
            assert!(!reply.ok);
            assert_eq!(reply.op, op);
            assert_eq!(
                reply.error.map(|e| e.message),
                Some("daemon is shutting down".to_string())
            );
        }
    }

    /// The accept thread never runs more connection threads than the cap:
    /// a slot past it waits until one is given back.
    #[test]
    fn slots_should_wait_at_the_cap_until_one_is_given_back() {
        let slots = Arc::new(Slots::new(2));
        let first = Slots::acquire(&slots);
        let _second = Slots::acquire(&slots);
        let (got_tx, got_rx) = mpsc::channel();
        let waiting = Arc::clone(&slots);
        let third = std::thread::spawn(move || {
            let slot = Slots::acquire(&waiting);
            got_tx.send(()).unwrap();
            slot
        });
        assert!(
            got_rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "a third slot was handed out at a cap of two"
        );
        drop(first);
        got_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a given-back slot is taken");
        drop(third.join().unwrap());
        assert_eq!(*slots.busy.lock().unwrap(), 1);
    }

    /// The hooks' liveness probe gives a daemon `PROBE_TIMEOUT` (1.5 s) to
    /// answer `ping`, then restarts it. A corpus with a constant answer has
    /// the loop give it while the writer runs a long request, so a busy
    /// daemon is never taken for a dead one (#1152).
    #[test]
    fn ping_should_be_answered_while_the_writer_holds_a_request() {
        let root = scratch_root("held-ping");
        let sock = socket_path(&root);
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let served = root.clone();
        let daemon = std::thread::spawn(move || {
            run_corpus(HeldCorpus {
                root: served,
                started: started_tx,
                release: release_rx,
                pong: true,
            })
        });
        wait_until("socket to bind", Duration::from_secs(10), || sock.exists());

        let held_sock = sock.clone();
        let held = std::thread::spawn(move || request(&held_sock, &Request::Status {}));
        started_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the writer takes the held request");

        let mut stream = UnixStream::connect(&sock).unwrap();
        stream.set_read_timeout(Some(PROBE_TIMEOUT)).unwrap();
        stream.set_write_timeout(Some(PROBE_TIMEOUT)).unwrap();
        let started = Instant::now();
        assert!(
            probe_ping(&mut stream),
            "ping went unanswered while the writer was busy"
        );
        assert!(started.elapsed() < PROBE_TIMEOUT);
        let pong = request(&sock, &Request::Ping);
        assert_eq!(pong["result"]["pong"], "loop", "the loop's answer: {pong}");
        assert!(!held.is_finished(), "the held request is still running");

        release_tx.send(()).unwrap();
        assert_eq!(held.join().unwrap()["ok"], true);
        assert_eq!(request(&sock, &Request::Shutdown)["ok"], true);
        daemon.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The loop's `ping` is the one the writer would give: same envelope,
    /// byte for byte, for the repository and the recall corpus.
    #[test]
    fn pong_should_equal_the_handled_ping() {
        let root = scratch_root("pong-service");
        let mut service = Service::open(&root).unwrap();
        let pong = Corpus::pong(&service).expect("a service answers ping from the loop");
        let handled = Corpus::handle(&mut service, Request::Ping);
        assert_eq!(
            serde_json::to_value(&pong).unwrap(),
            serde_json::to_value(&handled).unwrap()
        );
        assert_eq!(pong.result.unwrap()["pong"], true);
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod contract_tests;
