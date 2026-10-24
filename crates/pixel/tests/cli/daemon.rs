// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

use crate::support::{action_log, project, stderr, stdout};

#[test]
fn a_daemon_starts_serves_the_cli_and_stops() {
    let fixture = project();
    assert!(stdout(&fixture.run(&["daemon", "status"])).contains("not running"));
    let started = fixture.run(&["daemon", "start"]);
    assert!(started.status.success(), "{}", stderr(&started));
    assert!(
        stdout(&started).contains("daemon started"),
        "{}",
        stdout(&started)
    );
    assert!(stdout(&fixture.run(&["daemon", "status"])).contains("daemon running"));
    // A query is now served by the daemon, and says so in its action line.
    let found = fixture.run(&["find-symbol", "run"]);
    assert!(found.status.success(), "{}", stderr(&found));
    let routes: Vec<String> = action_log(&fixture.root())
        .iter()
        .filter(|line| line["command"] == "find-symbol")
        .map(|line| line["serve"][0]["route"].as_str().unwrap_or("").to_owned())
        .collect();
    assert_eq!(routes, ["daemon"]);
    // A second start is a no-op.
    assert!(stdout(&fixture.run(&["daemon", "start"])).contains("already running"));
    let stopped = fixture.run(&["daemon", "stop"]);
    assert!(
        stdout(&stopped).contains("daemon stopped"),
        "{}",
        stdout(&stopped)
    );
    assert!(stdout(&fixture.run(&["daemon", "status"])).contains("not running"));
}

#[test]
fn auto_start_brings_a_daemon_up_unless_the_variable_forbids_it() {
    let fixture = project();
    let forbidden = fixture.run(&["--metrics", "off", "find-symbol", "run"]);
    assert!(forbidden.status.success());
    assert!(stdout(&fixture.run(&["daemon", "status"])).contains("not running"));
    let allowed = fixture
        .pixel(&["--metrics", "off", "find-symbol", "run"])
        .env("PIXEL_DAEMON_AUTO_START", "1")
        .output()
        .unwrap();
    assert!(allowed.status.success(), "{}", stderr(&allowed));
    assert!(stdout(&fixture.run(&["daemon", "status"])).contains("daemon running"));
    let _ = fixture.run(&["daemon", "stop"]);
}
