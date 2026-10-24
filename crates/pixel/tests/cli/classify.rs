// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `pixel classify` outer-routing errors exercised without opening the model.

use std::io::{Read, Write};

use crate::support::project;

fn home(fixture: &crate::support::Fixture) -> std::path::PathBuf {
    fixture.dir.path().join("home")
}

#[test]
fn classify_without_labels_needs_the_local_engine() {
    let fixture = project();
    let home = home(&fixture);
    std::fs::create_dir_all(home.join(".pixel")).unwrap();
    std::fs::write(
        home.join(".pixel/config.yaml"),
        "classify: {enabled: true}\n",
    )
    .unwrap();
    let out = fixture
        .pixel(&["classify", "some state text", "--engine", "remote"])
        .env("PIXEL_METRICS", "0")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--label") && stderr.contains("ollaya"),
        "{stderr}"
    );
}

#[test]
fn classify_rejects_command_context_in_jsonl_mode_without_opening_model() {
    let fixture = project();
    let out = fixture.run(&["classify", "--jsonl", "--context", "the rubric preamble"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr.lines().next().unwrap(),
        "error: the argument '--jsonl' cannot be used with '--context <CONTEXT>'"
    );
}

#[test]
fn classify_rejects_bad_criterion_after_outer_dispatch_without_opening_model() {
    let fixture = project();
    let home = home(&fixture);
    std::fs::create_dir_all(home.join(".pixel")).unwrap();
    std::fs::write(
        home.join(".pixel/config.yaml"),
        "classify: {enabled: true}\n",
    )
    .unwrap();
    let out = fixture
        .pixel(&[
            "classify",
            "the state",
            "--label",
            "yes,no",
            "--criterion",
            "missing-equals",
        ])
        .env("PIXEL_METRICS", "0")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "pixel: --criterion needs <label>=<description>, got \"missing-equals\"; \
         declared labels: yes, no; \
         example: --criterion \"yes=one bounded edit\""
    );
}

/// A port nothing listens on: bound, read, released.
fn closed_base() -> String {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    format!("http://127.0.0.1:{}", probe.local_addr().unwrap().port())
}

#[test]
fn classify_if_warm_should_fail_fast_with_empty_stdout_when_no_local_engine_listens() {
    let fixture = project();
    let home = home(&fixture);
    let base = closed_base();
    std::fs::create_dir_all(home.join(".pixel")).unwrap();
    // A recorded launch that would be auto-started without --if-warm.
    std::fs::write(
        home.join(".pixel/config.yaml"),
        format!(
            "classify: {{enabled: true, engine: local, ollaya: {{base: \"{base}\", argv: [\"/usr/bin/false\"]}}}}\n"
        ),
    )
    .unwrap();
    let started = std::time::Instant::now();
    let out = fixture
        .pixel(&[
            "classify",
            "fix the login bug",
            "--task-intent",
            "--if-warm",
        ])
        .env("PIXEL_METRICS", "0")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "--if-warm never waits for a start: {:?}",
        started.elapsed()
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        format!(
            "pixel: not warm: no local classify engine is listening at {base}; --if-warm never starts it (`pixel classify` without --if-warm does)"
        )
    );
}

/// A local daemon that completes the TCP handshake and drains the request,
/// then answers only after `delay` with a verdict a classifying CLI would
/// use: warm to the connect probe, slow to answer, which is the daemon still
/// loading its model. The accept loop is nonblocking with a deadline so a
/// mutant that never connects fails an assertion instead of hanging the
/// test.
fn stalled_daemon(delay: std::time::Duration) -> String {
    let body = r#"{"answers":{"q1":{"type":"choice","choice":"bugfix","probabilities":{"bugfix":0.9,"feature":0.02,"refactor":0.02,"investigate":0.02,"question":0.02,"review":0.01,"ops":0.01},"confidence":0.9}}}"#;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            let Ok((mut stream, _)) = listener.accept() else {
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            };
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = [0u8; 4096];
            if stream.read(&mut request).unwrap_or(0) == 0 {
                continue; // the reachability probe connects, then closes
            }
            std::thread::sleep(delay);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    base
}

#[test]
fn classify_if_warm_should_give_up_when_the_daemon_accepts_but_still_loads_the_model() {
    let fixture = project();
    let home = home(&fixture);
    std::fs::create_dir_all(home.join(".pixel")).unwrap();
    // Answers at 5 s with a usable verdict, far past the 300 ms warm cap but
    // inside the connect probe: without that cap the call succeeds and
    // prints the verdict instead of exiting empty-handed.
    let base = stalled_daemon(std::time::Duration::from_secs(5));
    std::fs::write(
        home.join(".pixel/config.yaml"),
        format!(
            "classify: {{enabled: true, engine: local, ollaya: {{base: \"{base}\", argv: [\"/usr/bin/false\"]}}}}\n"
        ),
    )
    .unwrap();
    let out = fixture
        .pixel(&[
            "classify",
            "fix the login bug",
            "--task-intent",
            "--if-warm",
        ])
        .env("PIXEL_METRICS", "0")
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "--if-warm gives up instead of waiting for the model: {out:?}"
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.starts_with(&format!("pixel: ollaya {base}/v1/systemone:")),
        "{stderr}"
    );
}

#[test]
fn classify_task_intent_should_refuse_explicit_labels() {
    let fixture = project();
    let out = fixture.run(&["classify", "t", "--task-intent", "--label", "a,b"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.starts_with("error: the argument '--task-intent' cannot be used with"),
        "{stderr}"
    );
}

#[test]
fn a_stored_key_should_reach_no_log_no_listing_and_only_an_owner_readable_file() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = project();
    let secret = "sk-test-0123456789abcdef";
    let set = fixture.run(&["config", "remote-key", "openai", secret]);
    assert!(
        set.status.success(),
        "{}",
        String::from_utf8_lossy(&set.stderr)
    );
    let report = fixture.run(&["config", "remote-key", "openai"]);
    assert!(String::from_utf8_lossy(&report.stdout).contains("remote-key openai: set"));

    for output in [
        &set,
        &report,
        &fixture.run(&["config"]),
        &fixture.run(&["config", "--json"]),
    ] {
        let shown = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!shown.contains(secret), "{shown}");
    }
    let log = std::fs::read_to_string(fixture.root().join(".pixel/actions.jsonl")).unwrap();
    assert!(log.contains("config remote-key openai <redacted>"), "{log}");
    assert!(!log.contains(secret), "the key reached the action log");

    let file = home(&fixture).join(".pixel/config.yaml");
    assert!(std::fs::read_to_string(&file).unwrap().contains(secret));
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o077,
        0
    );

    // The key read from stdin is stored without ever being an argument.
    let mut child = fixture
        .pixel(&["config", "remote-key", "ollama", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"piped-secret-value\n")
        .unwrap();
    let piped = child.wait_with_output().unwrap();
    assert!(piped.status.success());
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .contains("piped-secret-value")
    );
    let log = std::fs::read_to_string(fixture.root().join(".pixel/actions.jsonl")).unwrap();
    assert!(!log.contains("piped-secret-value"));
}

#[test]
fn classify_should_refuse_before_any_engine_unless_it_was_switched_on() {
    let fixture = project();
    let out = fixture.run(&["classify", "some text", "--label", "a,b"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("classify is disabled"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");

    let on = fixture.run(&["config", "classify", "on"]);
    assert!(on.status.success());
    let off = fixture.run(&["config", "classify", "off"]);
    assert!(off.status.success());
    let again = fixture.run(&[
        "classify",
        "some text",
        "--label",
        "a,b",
        "--engine",
        "remote",
    ]);
    assert!(String::from_utf8_lossy(&again.stderr).contains("classify is disabled"));
    // Switching it off keeps nothing of the engine choice away: only the switch moves.
    let text = std::fs::read_to_string(home(&fixture).join(".pixel/config.yaml")).unwrap();
    assert!(text.contains("enabled: false"), "{text}");
}
