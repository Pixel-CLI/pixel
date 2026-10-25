// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Retrieval commands run outside any git repository answer or fail within
//! seconds. An ancestor `.pixel/base.shard` makes a gitless invocation root
//! there (`discover_root`); the freshness check of that root walks and hashes
//! the whole tree, and an unrelated huge one (`/tmp` with a shard in it)
//! held `search-content` and `find-code` for minutes, behind a daemon the
//! command had started for the non-repository root first.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::support::{Scratch, daemons_serving, pixel_command};

/// The hard limit a command gets here; the fixed behaviour needs a fraction.
const HARD_TIMEOUT: Duration = Duration::from_secs(20);
/// What "fast" means for a fixture of a few hundred small files.
const FAST: Duration = Duration::from_secs(10);

struct Run {
    /// `None` when the command was still running at [`HARD_TIMEOUT`].
    code: Option<i32>,
    elapsed: Duration,
    stdout: String,
    stderr: String,
}

/// Run `command` with piped output; kill it at [`HARD_TIMEOUT`].
fn run_with_deadline(command: &mut Command) -> Run {
    let started = Instant::now();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let code = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status.code().unwrap_or(-1));
        }
        if started.elapsed() > HARD_TIMEOUT {
            let _ = child.kill();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let elapsed = started.elapsed();
    let out = child.wait_with_output().unwrap();
    Run {
        code,
        elapsed,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// `pixel <args>` from `cwd` with daemon auto-start allowed (the fixture
/// checks that none was started) and an isolated shard cache.
fn pixel_in(cwd: &Path, args: &[&str]) -> Command {
    let mut command = pixel_command();
    command
        .args(args)
        .current_dir(cwd)
        .env("PIXEL_DAEMON_AUTO_START", "1")
        .env("XDG_CACHE_HOME", cwd.join("xdg-cache-unused"));
    command
}

/// A gitless tree of `files` small files holding one `retry` call, indexed
/// (budget lifted for the build), with a `work/` subdirectory to run from:
/// the shape of a scratch directory below a directory somebody indexed.
fn indexed_gitless_tree(tag: &str, files: usize) -> Scratch {
    let root = Scratch::for_test("pixel-outside-repo", tag);
    for n in 0..files {
        std::fs::write(
            root.join(format!("f{n}.rs")),
            format!("fn filler_{n}() {{ let x = {n}; }}\n"),
        )
        .unwrap();
    }
    std::fs::write(root.join("a.rs"), "fn retry_once() {}\n").unwrap();
    std::fs::create_dir(root.join("work")).unwrap();
    // The first open builds the shard and stores the tree signature beside it.
    let built = pixel_command()
        .args(["search-content", "retry_once"])
        .current_dir(&*root)
        .env("PIXEL_INDEX_BUDGET_MS", "0")
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    assert!(root.join(".pixel/base.shard").is_file() && root.join(".pixel/base.sig").is_file());
    root
}

#[test]
fn a_gitless_tree_over_its_time_budget_should_fail_fast_without_a_daemon() {
    let root = indexed_gitless_tree("budget", 400);
    for args in [
        &["search-content", "retry"][..],
        &["find-code", "retry"][..],
        &["search-history", "retry"][..],
    ] {
        let mut command = pixel_in(&root.join("work"), args);
        // 1 ms: the freshness walk of 400 files cannot finish inside it.
        command.env("PIXEL_INDEX_BUDGET_MS", "1");
        let run = run_with_deadline(&mut command);
        let label = args[0];
        assert_eq!(run.code, Some(1), "{label} exit; stderr: {}", run.stderr);
        assert!(run.elapsed < FAST, "{label} took {:?}", run.elapsed);
        assert!(
            run.stderr.contains("not inside a git repository")
                && run.stderr.contains("PIXEL_INDEX_BUDGET_MS"),
            "{label} stderr names the cause and the way out: {}",
            run.stderr
        );
        assert_eq!(run.stdout, "", "{label} printed an answer it did not have");
        assert_eq!(
            daemons_serving(&root),
            Vec::<String>::new(),
            "{label} started a daemon for a non-repository root"
        );
        assert_eq!(
            daemons_serving(&root.join("work")),
            Vec::<String>::new(),
            "{label} started a daemon for its working directory"
        );
    }
}

#[test]
fn a_small_gitless_tree_should_be_answered_in_process_without_a_daemon() {
    let root = Scratch::for_test("pixel-outside-repo", "small");
    std::fs::write(root.join("a.rs"), "fn retry_once() {}\n").unwrap();
    for args in [
        &["search-content", "retry_once"][..],
        &["find-code", "retry"][..],
    ] {
        let run = run_with_deadline(&mut pixel_in(&root, args));
        let label = args[0];
        assert_eq!(run.code, Some(0), "{label} exit; stderr: {}", run.stderr);
        assert!(run.elapsed < FAST, "{label} took {:?}", run.elapsed);
        assert!(
            run.stdout.contains("a.rs"),
            "{label} found the file: {}",
            run.stdout
        );
        assert_eq!(
            daemons_serving(&root),
            Vec::<String>::new(),
            "{label} started a daemon for a non-repository root"
        );
    }
}
