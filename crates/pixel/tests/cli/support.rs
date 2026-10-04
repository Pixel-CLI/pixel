// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Shared fixture plumbing for the CLI contract tests.
//!
//! Two rules every module follows through these helpers:
//!
//! 1. The binary under test never auto-starts a repo daemon. A daemon is
//!    detached from its parent (`process_group(0)`) and lives until its idle
//!    timeout, so one seed `pixel search-content` per fixture left one
//!    `target/debug/pixel daemon` behind per test: 18 per run of this binary,
//!    load average past 70 on 8 cores after a few runs.
//! 2. Every fixture root checks on drop that no daemon is serving it, and
//!    fails the test when one is. A test that starts a daemon on purpose
//!    stops it before the fixture goes out of scope.
//! 3. The binary never runs from the test's own working directory. Cargo
//!    starts a test in `crates/pixel`, inside this checkout, and the CLI
//!    appends every invocation to the action log of the repository around
//!    its path, falling back to its working directory when the path does not
//!    resolve: a `pixel classify` or a `check-release --repo <missing dir>`
//!    spawned from there landed in the checkout's own `.pixel/actions.jsonl`
//!    and drowned the real errors in `pixel action-log --errors-only`.
//! 4. The binary never reads the developer's home. `HOME` points at an empty
//!    directory unless the test sets its own: every command compares the
//!    prompts `pixel install` deployed there with its own and warns on
//!    stderr, so a machine that ran another release's install failed every
//!    test that reads stderr exactly, while CI, with no install, passed.

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The built `pixel` binary with daemon auto-start disabled, started from
/// [`neutral_cwd`] so its action log never reaches this checkout, with
/// `HOME` at [`neutral_home`]. A test that needs another working directory
/// or home sets it after this call (the last `current_dir` or `env` wins).
pub fn pixel_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pixel"));
    command
        .env("PIXEL_DAEMON_AUTO_START", "0")
        .env("HOME", neutral_home())
        .current_dir(neutral_cwd());
    command
}

/// An empty home, one per test process and created once, like
/// [`neutral_cwd`]: nothing `pixel install` deployed on this machine is in it.
pub fn neutral_home() -> &'static Path {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let dir =
            std::env::temp_dir().join(format!("pixel-cli-neutral-home-{}", std::process::id()));
        std::fs::create_dir_all(&dir)
            .unwrap_or_else(|e| panic!("create neutral home {}: {e}", dir.display()));
        dir
    })
}

/// A directory outside every repository, one per test process and created
/// once: where a command given no path, or a path that does not exist,
/// records itself. The process id keeps it from colliding with a file or a
/// directory another process left at a fixed name. It is not removed: a test
/// binary has no teardown hook, and under nextest each test is its own
/// process, so the next one could not know when the last user has exited.
pub fn neutral_cwd() -> &'static Path {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let dir =
            std::env::temp_dir().join(format!("pixel-cli-neutral-cwd-{}", std::process::id()));
        std::fs::create_dir_all(&dir)
            .unwrap_or_else(|e| panic!("create neutral cwd {}: {e}", dir.display()));
        dir
    })
}

/// `ps` lines of every `pixel daemon start <root> --foreground` process whose
/// root is exactly `root`. Process-table based on purpose: a daemon that is
/// still opening its index has neither socket nor pid file yet, but it is
/// already a process this test will leak.
pub fn daemons_serving(root: &Path) -> Vec<String> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
        .expect("ps must be runnable");
    let root = root.to_string_lossy();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| {
            line.split(" daemon start ")
                .nth(1)
                .and_then(|rest| rest.split_whitespace().next())
                == Some(root.as_ref())
        })
        .map(str::to_string)
        .collect()
}

/// Fail the calling test when a daemon serves `root`; the daemon is
/// terminated first so a red test does not also heat the machine.
pub fn assert_no_daemon(root: &Path) {
    report_leak(root, daemons_serving(root));
}

/// Terminate and report the daemons found by an earlier scan. Separate from
/// the scan because a fixture drop scans before deleting its directory: the
/// daemon notices the deletion and exits within milliseconds, so a rescan
/// after the deletion would miss the leak the first scan saw.
fn report_leak(root: &Path, leaked: Vec<String>) {
    if leaked.is_empty() {
        return;
    }
    for line in &leaked {
        if let Some(pid) = line.split_whitespace().next() {
            let _ = Command::new("kill").arg(pid).status();
        }
    }
    panic!(
        "a pixel daemon outlived the test for {}:\n{}\nrun the binary through support::pixel_command() or stop the daemon before the fixture drops",
        root.display(),
        leaked.join("\n")
    );
}

/// A fake recall-daemon socket at `recall_dir`'s socket path: it answers the
/// client's `Ping` (the probe and the op open separate connections) and then
/// the first `Recall` with `answer`, so a test can drive the daemon-first
/// path without a corpus or a model. The caller joins the handle and removes
/// the socket the thread leaves behind.
pub fn fake_recall_daemon(
    recall_dir: &Path,
    answer: pixel_daemon::Response,
) -> std::thread::JoinHandle<()> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    let listener = UnixListener::bind(pixel_daemon::daemon::socket_path(recall_dir)).unwrap();
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        // Poll with a deadline: a client that never reaches the `Recall` must
        // fail its own assertion, not hang here.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut served = false;
        while !served && std::time::Instant::now() < deadline {
            let Ok((mut stream, _)) = listener.accept() else {
                std::thread::sleep(std::time::Duration::from_millis(20));
                continue;
            };
            // A non-blocking listener hands back a non-blocking socket on
            // BSD: reset it, or the read races the client's write.
            let _ = stream.set_nonblocking(false);
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            while !served {
                let mut line = String::new();
                let Ok(n) = reader.read_line(&mut line) else {
                    break;
                };
                if n == 0 {
                    break;
                }
                let reply = match serde_json::from_str::<pixel_daemon::Request>(&line).unwrap() {
                    pixel_daemon::Request::Ping => pixel_daemon::Response::success(
                        "ping",
                        serde_json::json!({"pong": true, "protocol_version": pixel_daemon::api::PROTOCOL_VERSION}),
                    ),
                    pixel_daemon::Request::Recall { .. } => {
                        served = true;
                        answer.clone()
                    }
                    other => panic!("unexpected request: {other:?}"),
                };
                writeln!(stream, "{}", serde_json::to_string(&reply).unwrap()).unwrap();
            }
        }
    })
}

/// A throwaway directory under the system temp dir. Dereferences to `Path`;
/// removed on drop, after the daemon leak check.
pub struct Scratch(PathBuf);

impl Scratch {
    /// Create (or recreate) `<temp_dir>/<name>` and canonicalize it, so that
    /// the path compares equal to the one an auto-started daemon receives.
    pub fn create(name: &str) -> Self {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir.canonicalize().unwrap())
    }

    /// Fixture name prefixed with the test process id, so parallel test
    /// binaries never share a directory.
    pub fn for_test(module: &str, tag: &str) -> Self {
        Self::create(&format!("{module}-{tag}-{}", std::process::id()))
    }
}

impl Deref for Scratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // A test that already failed keeps its own panic message; a second
        // panic inside drop would abort the whole test binary.
        let leaked = if std::thread::panicking() {
            Vec::new()
        } else {
            daemons_serving(&self.0)
        };
        let _ = std::fs::remove_dir_all(&self.0);
        report_leak(&self.0, leaked);
    }
}

/// The one git fixture helper for this crate's CLI suite. It pins
/// `GIT_CONFIG_GLOBAL` and `GIT_CONFIG_SYSTEM` to `/dev/null`, so a
/// developer's global hooks, commit signing or `init.defaultBranch` cannot
/// reach into a fixture; a suite that grows its own helper loses that.
pub(crate) fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn wait_until(what: &str, secs: u64, mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while !done() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// The leak detector must see a real daemon, otherwise every fixture's drop
/// check passes vacuously and the leak this module exists for comes back
/// unnoticed.
#[test]
fn leak_guard_sees_a_daemon_serving_the_fixture_and_its_stop() {
    let repo = Scratch::for_test("pixel-support-leak-guard", "daemon");
    std::fs::write(repo.join("lib.rs"), "pub fn seed() {}\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "seed"]);
    assert!(daemons_serving(&repo).is_empty(), "no daemon before start");

    let start = pixel_command()
        .args(["daemon", "start"])
        .arg(&*repo)
        .output()
        .unwrap();
    assert!(
        start.status.success(),
        "daemon start: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    let serving = daemons_serving(&repo);
    assert_eq!(
        serving.len(),
        1,
        "exactly one daemon serves the fixture: {serving:?}"
    );
    assert!(
        serving[0].contains("--foreground"),
        "the match is the detached daemon process, not the start command: {serving:?}"
    );

    let stop = pixel_command()
        .args(["daemon", "stop"])
        .arg(&*repo)
        .output()
        .unwrap();
    assert!(
        stop.status.success(),
        "daemon stop: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
    wait_until("daemon process to exit after stop", 10, || {
        daemons_serving(&repo).is_empty()
    });
}

/// A daemon that outlives its fixture is reported as a test failure with the
/// offending process, not silently reaped.
#[test]
fn leak_guard_fails_the_test_when_a_daemon_outlives_the_fixture() {
    let repo = Scratch::for_test("pixel-support-leak-guard", "leak");
    std::fs::write(repo.join("lib.rs"), "pub fn seed() {}\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "seed"]);
    let start = pixel_command()
        .args(["daemon", "start"])
        .arg(&*repo)
        .output()
        .unwrap();
    assert!(start.status.success());

    let root = repo.to_path_buf();
    let outcome = std::panic::catch_unwind(|| assert_no_daemon(&root));
    let message = match outcome {
        Ok(()) => panic!("assert_no_daemon must fail while a daemon serves the fixture"),
        Err(payload) => payload
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_else(|| {
                payload
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .unwrap_or_default()
            }),
    };
    assert!(
        message.contains("outlived the test") && message.contains("daemon start"),
        "failure names the leak and the process: {message}"
    );
    wait_until("leaked daemon to be terminated by the guard", 10, || {
        daemons_serving(&repo).is_empty()
    });
}

/// Rule 3: a command whose path does not resolve records itself in the
/// neutral directory, never in the checkout the suite runs from. The probe's
/// unique marker keeps both reads immune to anything else writing either log
/// meanwhile (other tests, a developer's own `pixel` in this checkout).
#[test]
fn action_log_of_an_unresolvable_path_lands_in_the_neutral_cwd_not_the_checkout() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let marker = format!("pixel-neutral-cwd-probe-{}-{nanos}", std::process::id());
    let missing = std::env::temp_dir().join(&marker);
    let out = pixel_command()
        .args(["check-release", "release-candidate", "--repo"])
        .arg(&missing)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let logged = |root: &Path| {
        std::fs::read_to_string(root.join(".pixel/actions.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains(&marker))
            .count()
    };
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    assert_eq!(
        logged(neutral_cwd()),
        1,
        "the probe is recorded once, in the neutral directory"
    );
    assert_eq!(
        logged(&checkout),
        0,
        "the probe leaked into {}",
        checkout.display()
    );
}
