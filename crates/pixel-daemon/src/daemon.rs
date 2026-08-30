//! Unix-socket NDJSON daemon: one JSON `Request` per line, one JSON
//! `Response` line back. Single-threaded request handling (requests are
//! fast); an accept thread and a notify watcher feed one mpsc channel.

use std::collections::BTreeMap;
use std::io::{BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher};

use crate::api::{Request, Response, ServeError, Service, failure_response};

const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const DEBOUNCE: Duration = Duration::from_millis(500);
/// Idle poll interval for the facts ingest thread once fresh. A ref move
/// re-triggers ingest on the next poll without blocking queries.
const INGEST_IDLE_POLL: Duration = Duration::from_secs(5);
/// Backoff after a transient ingest error (e.g. a git lock held by another
/// process) before retrying.
const INGEST_ERROR_BACKOFF: Duration = Duration::from_secs(2);
const IGNORED_DIRS: &[&str] = [".pixel", ".git", "target", "node_modules"].as_slice();
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
/// Deliberately distinct from the legacy gitpixel tool's `gitpixel-*.sock`
/// prefix: the two daemons speak incompatible response envelopes (gitpixel's
/// `{ok,error,data}` vs pixel's `{op,protocol,...}`), so an old gitpixel
/// daemon and this one must bind to different socket paths and coexist
/// independently rather than collide on the same one.
pub fn socket_path(root: &Path) -> PathBuf {
    let canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let h = xxhash_rust::xxh3::xxh3_64(canon.to_string_lossy().as_bytes());
    std::env::temp_dir().join(format!("pixel-{h:016x}.sock"))
}

pub fn pid_path(root: &Path) -> PathBuf {
    socket_path(root).with_extension("pid")
}

enum Msg {
    Conn(UnixStream),
    Fs(notify::Event),
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
    /// Directories the watcher observes (default: the root).
    fn watch_paths(&self) -> Vec<PathBuf> {
        vec![self.root().to_path_buf()]
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
}

/// Run the repo daemon in the foreground until Shutdown, idle timeout, or
/// error.
pub fn run(root: &Path) -> Result<(), ServeError> {
    let service = Service::open(root)?;
    spawn_facts_ingest(root);
    run_corpus(service)
}

/// Spawn a low-priority background thread that periodically ticks the facts
/// ingest (history.db) until fresh, then idle-polls so a ref move re-triggers
/// ingest. Queries never block on it: the ingest shares the WAL-mode
/// connection and yields every tick budget.
fn spawn_facts_ingest(root: &Path) {
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
        loop {
            match pixel_facts::ingest::ingest_tick(&mut store, &opts) {
                Ok(report) if report.fresh => {
                    std::thread::sleep(INGEST_IDLE_POLL);
                }
                Ok(_) => {
                    // Not fresh yet — keep ticking (budget-bounded per call).
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

    let (tx, rx) = mpsc::channel::<Msg>();

    // Accept thread: forwards connections into the single-threaded loop.
    let tx_conn = tx.clone();
    let accept_listener = listener.try_clone()?;
    std::thread::spawn(move || {
        for stream in accept_listener.incoming() {
            match stream {
                Ok(s) => {
                    if tx_conn.send(Msg::Conn(s)).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Watcher: raw notify events into the channel; debounced below.
    let tx_fs = tx.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            let _ = tx_fs.send(Msg::Fs(ev));
        }
    })
    .map_err(|e| ServeError::Msg(format!("watcher init: {e}")))?;
    let watch_paths = service.watch_paths();
    for wp in &watch_paths {
        watcher
            .watch(wp, RecursiveMode::Recursive)
            .map_err(|e| ServeError::Msg(format!("watch {}: {e}", wp.display())))?;
    }

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

    while !shutdown {
        let now = Instant::now();
        let idle_left = IDLE_TIMEOUT
            .checked_sub(now.duration_since(last_activity))
            .unwrap_or(Duration::ZERO);
        let timeout = match flush_at {
            Some(at) => at.saturating_duration_since(now).min(idle_left),
            None => idle_left,
        };

        match rx.recv_timeout(timeout.max(Duration::from_millis(10))) {
            Ok(Msg::Conn(stream)) => {
                last_activity = Instant::now();
                handle_conn(&mut service, stream, &mut shutdown);
            }
            Ok(Msg::Fs(ev)) => {
                record_event(&ev, &mut pending);
                if !pending.is_empty() {
                    flush_at = Some(Instant::now() + DEBOUNCE);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        if let Some(at) = flush_at
            && Instant::now() >= at
        {
            for (abs, removed) in std::mem::take(&mut pending) {
                service.apply_change(&abs, removed);
            }
            flush_at = None;
        }

        if last_activity.elapsed() >= IDLE_TIMEOUT {
            eprintln!("pixel daemon: idle timeout, exiting");
            break;
        }
    }

    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(pid_path(&root));
    Ok(())
}

fn record_event(ev: &notify::Event, pending: &mut BTreeMap<PathBuf, bool>) {
    for path in &ev.paths {
        if path.components().any(|c| match c {
            Component::Normal(s) => IGNORED_DIRS.iter().any(|d| s == *d),
            _ => false,
        }) {
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

fn handle_conn(service: &mut dyn Corpus, stream: UnixStream, shutdown: &mut bool) {
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
            Err(e) => (failure_response("error", format!("bad request: {e}")), false),
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
