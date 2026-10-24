// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Reaching the daemon (`specs/ops.md`, "Shape"): a request goes to the
//! repository's daemon when one answers, to a daemon this call starts when
//! none does, and to an in-process [`Service`] otherwise. Each request leaves
//! a [`ServeStep`] in the invocation's action-log line, so a slow line says
//! whether the seconds went to a busy daemon, a cold start or an open.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pixel_daemon::{Request, Response, Service, socket_path};
use pixel_metrics::{InProcessReason, ServeRoute, ServeStep};
use pixel_proto::ENVELOPE_PROTOCOL_VERSION;
use serde_json::Value;

use crate::serve_trace;

/// How long the daemon gets to read and answer a connection's first line.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(5_000);

/// A request may legitimately take a while (the first graph build).
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(600);

/// How long a stale daemon gets to release its socket after `Shutdown`.
const STALE_DAEMON_EXIT_CAP: Duration = Duration::from_secs(2);

/// How long a started daemon gets to come up: 50 polls, 100 ms apart.
const START_POLLS: u32 = 50;
const START_POLL_EVERY: Duration = Duration::from_millis(100);

/// The root of the work tree that contains `path`: a directory, or a file in
/// one.
pub fn discover_root(path: &Path) -> Result<PathBuf, String> {
    let directory = if path.is_file() {
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
    } else {
        path
    };
    pixel_graph::tree::root(directory).map_err(|err| err.to_string())
}

/// One NDJSON round trip on an open stream.
fn roundtrip(stream: &mut UnixStream, request: &Request) -> Option<Response> {
    let mut line = serde_json::to_string(request).ok()?;
    line.push('\n');
    stream.write_all(line.as_bytes()).ok()?;
    stream.flush().ok()?;
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut answer = String::new();
    reader.read_line(&mut answer).ok()?;
    serde_json::from_str(&answer).ok()
}

fn open_stream(root: &Path) -> Option<UnixStream> {
    let stream = UnixStream::connect(socket_path(root)).ok()?;
    // The daemon drains its debounced watcher batch before serving a
    // connection; on a cold or loaded host that drain can outlast a short
    // timeout.
    stream.set_read_timeout(Some(CONNECT_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(CONNECT_TIMEOUT)).ok()?;
    Some(stream)
}

/// What answers on a root's daemon socket, from this CLI's point of view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    /// Nothing answers a ping.
    Absent,
    /// A healthy daemon of this very version.
    Current,
    /// Another version, an older protocol, or a failed ping: it must not
    /// serve this CLI, and every daemon ever shipped understands `Shutdown`.
    Stale,
    /// A newer protocol: a newer CLI started it and may still use it.
    Newer,
}

fn classify_ping(ping: &Response) -> Probe {
    let data = ping.data();
    match data.get("protocol").and_then(Value::as_u64) {
        Some(protocol) if protocol > u64::from(ENVELOPE_PROTOCOL_VERSION) => Probe::Newer,
        Some(protocol)
            if ping.ok
                && protocol == u64::from(ENVELOPE_PROTOCOL_VERSION)
                && data.get("version").and_then(Value::as_str)
                    == Some(env!("CARGO_PKG_VERSION")) =>
        {
            Probe::Current
        }
        _ => Probe::Stale,
    }
}

pub fn probe(root: &Path) -> Probe {
    open_stream(root)
        .and_then(|mut stream| roundtrip(&mut stream, &Request::Ping))
        .map_or(Probe::Absent, |ping| classify_ping(&ping))
}

/// Shuts a stale daemon down and waits (up to `cap`) until it stops
/// answering, so a replacement started right after does not die on the lock
/// the old one still holds.
fn retire_stale_daemon(root: &Path, cap: Duration) {
    if let Some(mut stream) = open_stream(root) {
        let _ = roundtrip(&mut stream, &Request::Shutdown);
    }
    let deadline = Instant::now() + cap;
    while Instant::now() < deadline && probe(root) == Probe::Stale {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// How routing one request through the daemon ended.
enum Route {
    Served(Box<Response>),
    /// No daemon can serve it now; starting a current one may help.
    Absent,
    /// A newer daemon is running: serve in process, start nothing.
    Declined,
}

fn route_for_probe(root: &Path, probe: Probe) -> Option<Route> {
    match probe {
        Probe::Current => None,
        Probe::Newer => Some(Route::Declined),
        Probe::Stale => {
            retire_stale_daemon(root, STALE_DAEMON_EXIT_CAP);
            Some(Route::Absent)
        }
        Probe::Absent => Some(Route::Absent),
    }
}

/// Sends `request` to a daemon a probe found current.
fn send(root: &Path, request: &Request) -> Route {
    // Each request gets its own connection: no head-of-line wait behind a
    // handshake.
    let served = open_stream(root).and_then(|mut stream| {
        stream.set_read_timeout(Some(REQUEST_READ_TIMEOUT)).ok()?;
        roundtrip(&mut stream, request)
    });
    served.map_or(Route::Absent, |response| Route::Served(Box::new(response)))
}

/// Sends `request` to the daemon of `root` when a current one answers, and
/// to nothing else: no start, no fallback.
pub fn request_if_running(root: &Path, request: &Request) -> Option<Response> {
    if probe(root) != Probe::Current {
        return None;
    }
    match send(root, request) {
        Route::Served(response) => Some(*response),
        Route::Absent | Route::Declined => None,
    }
}

/// The arguments that run the daemon of `root` in the foreground.
fn daemon_args(root: &Path) -> Vec<OsString> {
    vec![
        "daemon".into(),
        "start".into(),
        root.as_os_str().to_owned(),
        "--foreground".into(),
    ]
}

/// Runs this binary with `args` as a background process of its own: no
/// standard streams, and a process group of its own so a terminal or an agent
/// supervisor that tears down the caller's group leaves it alone.
fn spawn_detached(args: &[OsString]) -> std::io::Result<()> {
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0);
    command.spawn().map(drop)
}

/// Starts the daemon of `root` (an absolute path) in the background. The one
/// spelling of that command: the CLI's auto-start and `pixel daemon start`
/// come here.
pub fn spawn_daemon(root: &Path) -> std::io::Result<()> {
    spawn_detached(&daemon_args(root))
}

/// Whether the CLI may start the daemon of `root`: `daemon_auto_start` in the
/// settings, or `PIXEL_DAEMON_AUTO_START=0`, forbids it.
fn auto_start_allowed(root: &Path) -> bool {
    crate::settings::daemon_auto_start(Some(root))
}

/// Starts the daemon of `root` in the background when none answers and the
/// environment allows it: what a session start does so the first prompt finds
/// one. Never waits.
pub fn warm(root: &Path) {
    if auto_start_allowed(root) && probe(root) == Probe::Absent {
        let absolute = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let _ = spawn_daemon(&absolute);
    }
}

fn auto_start(root: &Path, request: &Request) -> Result<Response, InProcessReason> {
    if !auto_start_allowed(root) {
        return Err(InProcessReason::AutoStartDisabled);
    }
    let absolute = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    spawn_daemon(&absolute).map_err(|_| InProcessReason::StartFailed)?;
    for _ in 0..START_POLLS {
        let route = route_for_probe(root, probe(root)).unwrap_or_else(|| send(root, request));
        if let Route::Served(response) = route {
            return Ok(*response);
        }
        std::thread::sleep(START_POLL_EVERY);
    }
    Err(InProcessReason::StartTimedOut)
}

/// Routes `request` through the daemon, starting one when none answers, and
/// says how it went; `None` leaves the request to this process, and the step
/// then names why.
fn route_through_daemon(root: &Path, request: &Request) -> (Option<Response>, ServeStep) {
    let clock = Instant::now();
    let probed = probe(root);
    let probe_ms = serve_trace::millis_since(clock);
    let mut start_clock = Instant::now();
    let mut request_ms = None;
    let route = route_for_probe(root, probed).unwrap_or_else(|| {
        let clock = Instant::now();
        let route = send(root, request);
        request_ms = Some(serve_trace::millis_since(clock));
        start_clock = Instant::now();
        route
    });
    let (response, mut step) = match route {
        Route::Served(response) => (Some(*response), ServeStep::new(ServeRoute::Daemon)),
        Route::Declined => (None, ServeStep::in_process(InProcessReason::NewerDaemon)),
        Route::Absent => {
            let started = auto_start(root, request);
            let start_ms = serve_trace::millis_since(start_clock);
            let (response, mut step) = match started {
                Ok(response) => (Some(response), ServeStep::new(ServeRoute::DaemonStarted)),
                Err(reason) => (None, ServeStep::in_process(reason)),
            };
            if step.reason != Some(InProcessReason::AutoStartDisabled) {
                step.start_ms = Some(start_ms);
            }
            (response, step)
        }
    };
    step.probe_ms = Some(probe_ms);
    step.request_ms = request_ms;
    (response, step)
}

/// Answers `request` for the repository containing `path`: the daemon when
/// there is one (or one can be started), this process otherwise.
pub fn execute(path: &Path, request: Request, no_daemon: bool) -> Result<Response, String> {
    let root = discover_root(path)?;
    let mut step = if no_daemon {
        ServeStep::in_process(InProcessReason::NoDaemon)
    } else {
        match route_through_daemon(&root, &request) {
            (Some(response), step) => {
                serve_trace::record(step);
                return Ok(response);
            }
            (None, step) => step,
        }
    };
    let clock = Instant::now();
    let opened = Service::open(&root);
    step.open_ms = Some(serve_trace::millis_since(clock));
    let response = opened.map(|mut service| {
        let clock = Instant::now();
        let response = service.handle(request);
        step.handle_ms = Some(serve_trace::millis_since(clock));
        response
    });
    serve_trace::record(step);
    response.map_err(|err| err.to_string())
}

/// The result of a response, with the envelope's honesty fields (epistemics,
/// snapshot, warnings) folded into it, so a caller that prints only the
/// payload cannot strip the completeness contract. A failure is its message.
pub fn unwrap_response(response: Response) -> Result<Value, String> {
    if !response.ok {
        return Err(response.error_message());
    }
    let Response {
        snapshot,
        epistemics,
        warnings,
        result,
        ..
    } = response;
    let mut data = result.unwrap_or(Value::Null);
    if let Some(object) = data.as_object_mut() {
        if let Some(epistemics) = epistemics {
            object
                .entry("epistemics")
                .or_insert_with(|| serde_json::to_value(epistemics).unwrap_or(Value::Null));
        }
        if let Some(snapshot) = snapshot {
            object
                .entry("snapshot")
                .or_insert_with(|| serde_json::to_value(snapshot).unwrap_or(Value::Null));
        }
        if !warnings.is_empty() {
            object
                .entry("warnings")
                .or_insert_with(|| serde_json::to_value(warnings).unwrap_or(Value::Null));
        }
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pixel_proto::Envelope;
    use serde_json::json;

    fn pong(protocol: u32, version: &str) -> Response {
        Envelope::success(
            "ping",
            json!({"pong": true, "protocol": protocol, "version": version}),
        )
    }

    #[test]
    fn a_daemon_of_this_version_and_protocol_is_current() {
        let ours = env!("CARGO_PKG_VERSION");
        assert_eq!(
            classify_ping(&pong(ENVELOPE_PROTOCOL_VERSION, ours)),
            Probe::Current
        );
    }

    #[test]
    fn another_version_or_an_older_protocol_is_stale_and_a_newer_one_is_left_alone() {
        assert_eq!(
            classify_ping(&pong(ENVELOPE_PROTOCOL_VERSION, "9.9.9")),
            Probe::Stale
        );
        assert_eq!(
            classify_ping(&pong(0, env!("CARGO_PKG_VERSION"))),
            Probe::Stale
        );
        assert_eq!(
            classify_ping(&pong(ENVELOPE_PROTOCOL_VERSION + 1, "9.9.9")),
            Probe::Newer
        );
        // A reply that names no protocol at all is an old daemon.
        let bare = Envelope::success("ping", json!({"pong": true}));
        assert_eq!(classify_ping(&bare), Probe::Stale);
        // A failed ping never counts as current.
        let mut failed = pong(ENVELOPE_PROTOCOL_VERSION, env!("CARGO_PKG_VERSION"));
        failed.ok = false;
        assert_eq!(classify_ping(&failed), Probe::Stale);
    }

    #[test]
    fn the_honesty_fields_ride_in_the_payload_without_clobbering_it() {
        let response = Envelope::success("symbol", json!({"symbols": [], "epistemics": "mine"}))
            .with_epistemics(pixel_proto::Epistemics::default())
            .with_snapshot(pixel_proto::SnapshotInfo::default());
        let data = unwrap_response(response).unwrap();
        assert_eq!(data["epistemics"], "mine", "an op's own key is kept");
        assert!(data["snapshot"].is_object());
        assert!(data.get("warnings").is_none());
    }

    #[test]
    fn a_failure_is_its_message() {
        let failed = pixel_daemon::failure_response("symbol", "no symbol named \"x\"");
        assert_eq!(
            unwrap_response(failed),
            Err("no symbol named \"x\"".to_owned())
        );
    }

    #[test]
    fn a_file_is_rooted_through_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let git = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["init", "-q"])
            .status()
            .unwrap();
        assert!(git.success());
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "x").unwrap();
        assert_eq!(discover_root(&root.join("src/a.rs")).unwrap(), root);
    }

    #[test]
    fn a_path_outside_any_work_tree_has_no_root() {
        let dir = tempfile::tempdir().unwrap();
        assert!(discover_root(dir.path()).is_err());
    }
}
