// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Integration: the `targets` op over a real fixture repo — index + graph
//! built for real, list closed, tiers assigned, output deterministic.

use std::path::Path;
use std::process::Command;

use pixel_daemon::{Request, Service};

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

fn fixture(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("gpx-targets-{tag}-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(dir.join("src/auth")).unwrap();
    std::fs::create_dir_all(dir.join("src/util")).unwrap();
    std::fs::write(
        dir.join("src/auth/login.rs"),
        "pub fn login_user(name: &str) -> bool {\n    !name.is_empty()\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/auth/session.rs"),
        "use crate::login::login_user;\n\npub fn start_session(name: &str) -> bool {\n    login_user(name)\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/util/strings.rs"),
        "pub fn pad_left(s: &str, n: usize) -> String {\n    format!(\"{s:>n$}\")\n}\n",
    )
    .unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "fixture"]);
    dir
}

fn run_targets(dir: &Path, task: &str) -> serde_json::Value {
    let mut svc = Service::open(dir).unwrap();
    let resp = svc.handle(Request::Targets {
        task: task.to_string(),
        limit: Some(10),
        max_tier: None,
        precision: false,
        regions: false,
    });
    assert!(resp.ok, "targets op failed: {:?}", resp.error);
    resp.into_data()
}

#[test]
fn targets_tiers_reasons_and_closed_list() {
    let dir = fixture("main");
    let data = run_targets(&dir, "fix `login_user` auth flow");

    let targets = data["targets"].as_array().unwrap();
    let paths: Vec<&str> = targets
        .iter()
        .map(|t| t["path"].as_str().unwrap())
        .collect();

    // Primary file: defines the backticked symbol → P0 with a defines-reason.
    let login = targets
        .iter()
        .find(|t| t["path"] == "src/auth/login.rs")
        .expect("login.rs must be targeted");
    assert_eq!(login["tier"], "P0");
    let reasons = login["reasons"].as_array().unwrap();
    assert!(
        reasons
            .iter()
            .any(|r| r.as_str().unwrap().contains("defines symbol `login_user`")),
        "missing defines-reason: {reasons:?}"
    );

    // Caller file present (content and/or graph evidence).
    assert!(
        paths.contains(&"src/auth/session.rs"),
        "caller missing: {paths:?}"
    );

    // Unrelated file excluded — the list is closed.
    assert!(
        !paths.contains(&"src/util/strings.rs"),
        "unrelated file leaked in"
    );

    // Envelope + closed-world claim always present.
    assert!(data["envelope"].get("lower_bound").is_some());
    assert!(data["closed_world"].as_str().unwrap().contains("Restrict"));
    assert_eq!(data["envelope"]["graph"], "fresh");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn targets_is_deterministic() {
    let dir = fixture("det");
    let a = run_targets(&dir, "fix `login_user` auth flow");
    let b = run_targets(&dir, "fix `login_user` auth flow");
    // stats.elapsed_ms varies; compare everything else.
    let strip = |mut v: serde_json::Value| {
        v.as_object_mut().unwrap().remove("stats");
        v.as_object_mut().unwrap().remove("graph_build");
        v
    };
    assert_eq!(strip(a), strip(b));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn targets_rejects_empty_task() {
    let dir = fixture("empty");
    let mut svc = Service::open(&dir).unwrap();
    let resp = svc.handle(Request::Targets {
        task: "fix the code".to_string(),
        limit: None,
        max_tier: None,
        precision: false,
        regions: false,
    });
    assert!(!resp.ok);
    assert!(
        resp.error
            .unwrap()
            .message
            .contains("no searchable keywords")
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A bare directory (NO `.git`) now builds a graph from the filesystem walk
/// (capped by PIXEL_GRAPH_MAX_FILES, default 50000) instead of refusing.
/// The graph is fresh because the file-hash signature matches; the envelope
/// reports `graph: "fresh"` and `gitless: true` in the build info. This is a
/// net improvement over the old behavior (hard refusal → "unavailable"):
/// `resolve`, `impact`, `context`, and `symbol` all work in non-git dirs now.
/// Regression for the `pi-fanout-flash` no-`.git` hang (6+ min, 732MB RAM) —
/// the file-count cap prevents that pathological case.
#[test]
fn targets_no_git_builds_graphless() {
    // A gitless temp dir — no `.git`, no ancestor `.pixel`/`.git` — and
    // `Service::open` uses the given path directly (not the upward
    // `discover_root` walk), so the root is precisely this dir.

    let dir = std::env::temp_dir().join(format!("gpx-targets-nogit-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/login.rs"),
        "pub fn login_user(name: &str) -> bool {\n    !name.is_empty()\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/session.rs"),
        "use crate::login::login_user;\n\npub fn start_session(name: &str) -> bool {\n    login_user(name)\n}\n",
    )
    .unwrap();

    let mut svc = Service::open(&dir).unwrap();
    let resp = svc.handle(Request::Targets {
        task: "fix `login_user` auth flow".to_string(),
        limit: Some(10),
        max_tier: None,
        precision: false,
        regions: false,
    });
    assert!(
        resp.ok,
        "targets op failed on a no-git dir: {:?}",
        resp.error
    );
    let data = resp.into_data();

    // The graph IS built (gitless mode) — the file-count cap keeps it safe.
    // The build info marks it as gitless.
    assert_eq!(
        data["envelope"]["graph"], "fresh",
        "no-git dir should build a fresh graph (gitless mode): {data}"
    );
    assert_eq!(
        data["graph_build"]["gitless"], true,
        "build info should mark gitless: {data}"
    );

    // Graph signal finds the defining file AND the caller.
    let targets = data["targets"].as_array().unwrap();
    assert!(
        targets.iter().any(|t| t["path"] == "src/login.rs"),
        "graph-signals target missing login.rs: {targets:?}",
    );
    assert!(
        targets.iter().any(|t| t["path"] == "src/session.rs"),
        "graph-signals target missing session.rs (caller): {targets:?}",
    );

    std::fs::remove_dir_all(&dir).ok();
}
