// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

use super::*;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::time::Instant;

fn scratch_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("pixel-daemon-ping-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root.canonicalize().unwrap()
}

/// Nothing listens: the probe is false and opens nothing — the repository
/// `Service` an auto-starting probe would open is what left `.pixel/`
/// inside `~/.local/share/pixel/recall`.
#[test]
fn daemon_ping_is_false_without_a_daemon() {
    let root = scratch_root("idle");
    assert!(!daemon_ping(&root));
    assert!(
        !root.join(pixel_index::index::SHARD_DIR).exists(),
        "the probe must not open a Service on {}",
        root.display()
    );
}

/// A daemon answering `Ping` is what the probe reports: without this half,
/// a probe that always returned false would pass the idle test.
#[test]
fn daemon_ping_is_true_when_a_daemon_answers() {
    let root = scratch_root("live");
    let listener = UnixListener::bind(daemon::socket_path(&root)).unwrap();
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
                        "the probe asks with a Ping"
                    );
                    let reply = Response::success(
                        "ping",
                        json!({"pong": true, "protocol_version": PROTOCOL_VERSION}),
                    );
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
    assert!(daemon_ping(&root));
    server.join().unwrap();
    let _ = std::fs::remove_file(daemon::socket_path(&root));
}

fn ping_reply(ok: bool, version: Option<u64>) -> Response {
    let mut reply = Response::success("ping", json!({"pong": true}));
    if let Some(version) = version {
        reply = Response::success("ping", json!({"pong": true, "protocol_version": version}));
    }
    reply.ok = ok;
    reply
}

#[test]
fn only_a_healthy_ping_on_this_protocol_may_serve_and_only_a_newer_one_is_spared() {
    assert_eq!(
        classify_ping(&ping_reply(true, Some(PROTOCOL_VERSION))),
        DaemonProbe::Current
    );
    assert_eq!(
        classify_ping(&ping_reply(false, Some(PROTOCOL_VERSION))),
        DaemonProbe::Stale,
        "a failing daemon on our protocol is replaced, not used"
    );
    assert_eq!(
        classify_ping(&ping_reply(true, Some(PROTOCOL_VERSION - 1))),
        DaemonProbe::Stale
    );
    assert_eq!(classify_ping(&ping_reply(true, None)), DaemonProbe::Stale);
    assert_eq!(
        classify_ping(&ping_reply(true, Some(PROTOCOL_VERSION + 1))),
        DaemonProbe::Newer
    );
}

/// A fake daemon answering Ping with `version` and recording every
/// request, for at most `connections` connections. After a Shutdown it
/// keeps answering for `linger` (a real daemon takes a moment to exit),
/// then removes its socket and answers whatever is still queued.
fn fake_daemon(
    root: &Path,
    version: u64,
    connections: usize,
    linger: Duration,
) -> std::thread::JoinHandle<Vec<Request>> {
    fn answer(mut stream: UnixStream, version: u64) -> Option<Request> {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut line = String::new();
        if BufReader::new(&stream).read_line(&mut line).unwrap_or(0) == 0 {
            return None;
        }
        let request: Request = serde_json::from_str(&line).unwrap();
        let reply = match request {
            Request::Ping => ping_reply(true, Some(version)),
            _ => Response::success("ok", json!({})),
        };
        writeln!(stream, "{}", serde_json::to_string(&reply).unwrap()).unwrap();
        Some(request)
    }
    let sock = daemon::socket_path(root);
    let listener = UnixListener::bind(&sock).unwrap();
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let mut seen = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut exit_at: Option<Instant> = None;
        while Instant::now() < deadline
            && seen.len() < connections
            && exit_at.is_none_or(|at| Instant::now() < at)
        {
            match listener.accept() {
                Ok((stream, _)) => {
                    if let Some(request) = answer(stream, version) {
                        if request == Request::Shutdown {
                            exit_at = Some(Instant::now() + linger);
                        }
                        seen.push(request);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
        if exit_at.is_some() {
            let _ = std::fs::remove_file(&sock);
            // Answer what connected before the socket went away, so no
            // client waits out its read timeout.
            while let Ok((stream, _)) = listener.accept() {
                let _ = answer(stream, version);
            }
        }
        seen
    })
}

/// A current daemon's answer is logged as a daemon route with both its
/// phases timed: the probe (the queue ahead) and the request itself.
#[test]
fn route_through_daemon_times_the_probe_and_the_request_of_a_current_daemon() {
    let root = scratch_root("route-current");
    let server = fake_daemon(&root, PROTOCOL_VERSION, 2, Duration::ZERO);
    let (response, step) = route_through_daemon(&root, &Request::Status {});
    assert!(response.is_some_and(|r| r.ok));
    assert_eq!(step.route, ServeRoute::Daemon);
    assert_eq!(step.reason, None);
    assert!(
        step.probe_ms.is_some() && step.request_ms.is_some(),
        "{step:?}"
    );
    assert_eq!((step.start_ms, step.open_ms), (None, None), "{step:?}");
    assert_eq!(
        server.join().unwrap(),
        vec![Request::Ping, Request::Status {}]
    );
    let _ = std::fs::remove_file(daemon::socket_path(&root));
}

/// A newer daemon sends the request back to this process without a
/// start attempt: the step says so, and times only the probe.
#[test]
fn route_through_daemon_names_a_newer_daemon_as_the_in_process_reason() {
    let root = scratch_root("route-newer");
    let server = fake_daemon(&root, PROTOCOL_VERSION + 1, 1, Duration::ZERO);
    let (response, step) = route_through_daemon(&root, &Request::Status {});
    assert!(response.is_none());
    assert_eq!(step.route, ServeRoute::InProcess);
    assert_eq!(step.reason, Some(InProcessReason::NewerDaemon));
    assert!(step.probe_ms.is_some(), "{step:?}");
    assert_eq!((step.request_ms, step.start_ms), (None, None), "{step:?}");
    assert_eq!(server.join().unwrap(), vec![Request::Ping]);
    let _ = std::fs::remove_file(daemon::socket_path(&root));
}

/// A stale daemon's retirement is timed as the start of its replacement:
/// on the first call after an upgrade the probe itself is quick, and a
/// `probe_ms` holding the retirement would read as a busy daemon.
#[test]
fn retiring_a_stale_daemon_counts_toward_the_start_not_the_probe() {
    let root = scratch_root("route-stale");
    let server = fake_daemon(&root, PROTOCOL_VERSION - 1, 100, Duration::from_millis(400));
    let (response, step) = route_through_daemon_with(&root, &Request::Status {}, |_, _| {
        Err(InProcessReason::StartTimedOut)
    });
    assert!(response.is_none());
    assert_eq!(step.reason, Some(InProcessReason::StartTimedOut));
    let (probe_ms, start_ms) = (step.probe_ms.unwrap(), step.start_ms.unwrap());
    assert!(
        start_ms >= 300,
        "the retirement waited on the linger: {step:?}"
    );
    assert!(probe_ms < start_ms, "{step:?}");
    let seen = server.join().unwrap();
    assert_eq!(seen[..2], [Request::Ping, Request::Shutdown]);
}

/// No daemon answers: the started one's answer is returned and logged as
/// `daemon_started`; a refused start sends the request back to this
/// process under the start's own reason, untimed when it never ran.
#[test]
fn an_absent_daemon_is_started_or_names_why_it_was_not() {
    let root = scratch_root("route-absent");
    let (response, step) = route_through_daemon_with(&root, &Request::Status {}, |_, _| {
        Ok(Response::success("status", json!({"started": true})))
    });
    assert_eq!(response.unwrap().data()["started"], true);
    assert_eq!(step.route, ServeRoute::DaemonStarted);
    assert!(
        step.start_ms.is_some() && step.probe_ms.is_some(),
        "{step:?}"
    );
    assert_eq!(step.request_ms, None, "{step:?}");

    let (response, step) = route_through_daemon_with(&root, &Request::Status {}, |_, _| {
        Err(InProcessReason::AutoStartDisabled)
    });
    assert!(response.is_none());
    assert_eq!(step.route, ServeRoute::InProcess);
    assert_eq!(step.reason, Some(InProcessReason::AutoStartDisabled));
    assert_eq!(step.start_ms, None, "{step:?}");
}

#[test]
fn a_newer_daemon_is_left_running_and_declines_without_a_restart() {
    let root = scratch_root("newer");
    let server = fake_daemon(&root, PROTOCOL_VERSION + 1, 1, Duration::ZERO);
    assert!(matches!(
        try_daemon_inner(&root, &Request::Status {}),
        DaemonRoute::Declined
    ));
    assert_eq!(
        server.join().unwrap(),
        vec![Request::Ping],
        "no Shutdown sent"
    );
    let _ = std::fs::remove_file(daemon::socket_path(&root));
}

#[test]
fn a_stale_daemon_is_shut_down_so_a_current_one_can_start() {
    let root = scratch_root("stale");
    let server = fake_daemon(&root, PROTOCOL_VERSION - 1, 100, Duration::from_millis(200));
    let started = Instant::now();
    assert!(
        !daemon_ping(&root),
        "a stale daemon is never reported ready"
    );
    let waited = started.elapsed();
    assert!(
        !daemon::socket_path(&root).exists(),
        "retirement waits until the old daemon let its socket go"
    );
    assert!(
        waited < STALE_DAEMON_EXIT_CAP,
        "and returns as soon as it is gone: {waited:?}"
    );
    let seen = server.join().unwrap();
    assert_eq!(seen[..2], [Request::Ping, Request::Shutdown]);
    assert!(seen[2..].iter().all(|r| *r == Request::Ping), "{seen:?}");
}

#[test]
fn daemon_start_refuses_to_fight_a_newer_daemon() {
    let root = scratch_root("start-newer");
    let server = fake_daemon(&root, PROTOCOL_VERSION + 1, 1, Duration::ZERO);
    let error = daemon_start(root.clone(), false, true).unwrap_err();
    assert!(error.contains("newer pixel daemon"), "{error}");
    assert_eq!(server.join().unwrap(), vec![Request::Ping]);
    let _ = std::fs::remove_file(daemon::socket_path(&root));
}

#[test]
fn unwrap_response_folds_envelope_warnings_only_when_there_are_some() {
    let bare = unwrap_response(Response::success("x", json!({"a": 1}))).unwrap();
    assert_eq!(bare, json!({"a": 1}), "no warnings key without warnings");

    let warning = pixel_proto::warning::Warning {
        code: "capped".into(),
        message: "m".into(),
    };
    let warned = Response::success("x", json!({"a": 1})).with_warnings(vec![warning.clone()]);
    assert_eq!(
        unwrap_response(warned).unwrap()["warnings"],
        json!([{"code": "capped", "message": "m"}])
    );
    let own = Response::success("x", json!({"warnings": "op's own"})).with_warnings(vec![warning]);
    assert_eq!(unwrap_response(own).unwrap()["warnings"], json!("op's own"));
}

#[test]
fn only_a_missing_or_refusing_socket_reads_as_no_daemon_to_upgrade() {
    let root = scratch_root("upgrade-absent");
    // On macOS /tmp is a symlink to /private/tmp, and canonicalize() resolves
    // it into a path that already eats ~44 of sockaddr_un's ~104 usable bytes;
    // keep the fixture socket short enough to stay under SUN_LEN.
    let short = PathBuf::from("/tmp").join(format!("px-upg-{}", std::process::id()));
    let _ = std::fs::remove_file(&short);
    assert_eq!(
        upgrade_daemon_request(&short, &Request::Shutdown).map(|r| r.is_none()),
        Ok(true)
    );
    std::fs::write(root.join("file"), b"").unwrap();
    let not_a_dir = upgrade_daemon_request(&root.join("file/daemon.sock"), &Request::Shutdown);
    assert!(
        not_a_dir
            .as_ref()
            .is_err_and(|e| e.starts_with("upgrade daemon connection:")),
        "{:?}",
        not_a_dir.map(|r| r.is_some())
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_closed_stdout_is_not_a_failure_but_another_write_error_is() {
    struct Refusing(std::io::ErrorKind);
    impl Write for Refusing {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(self.0.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    assert_eq!(
        write_text(&mut Refusing(std::io::ErrorKind::BrokenPipe), "x"),
        Ok(())
    );
    let err = write_text(&mut Refusing(std::io::ErrorKind::WriteZero), "x").unwrap_err();
    assert!(err.starts_with("write stdout:"), "{err}");
    let mut sink = Vec::new();
    write_text(&mut sink, "hello").unwrap();
    assert_eq!(sink, b"hello");
}
