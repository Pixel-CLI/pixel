// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Differential checks against the actual native executables, not snapshots
//! of Pixel's own formatting.
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

const PIXEL: &str = env!("CARGO_BIN_EXE_pixel");
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new(content: &[u8]) -> Self {
        let root = std::env::temp_dir().join(format!(
            "pixel-search-compat-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join(".pixel")).unwrap();
        std::fs::write(root.join("a file.rs"), content).unwrap();
        Self(root.canonicalize().unwrap())
    }

    fn command(&self, binary: &str) -> Command {
        let mut command = Command::new(binary);
        command
            .current_dir(&self.0)
            .env("PIXEL_DAEMON_AUTO_START", "0")
            // An empty home: no global `.pixel/config.yaml` sets the policy,
            // and no deployed prompt adds a note to stderr.
            .env("HOME", crate::support::neutral_home())
            .env_remove("RIPGREP_CONFIG_PATH")
            .env_remove("GREP_OPTIONS")
            .env_remove("PIXEL_POLICY")
            .env_remove("PIXEL_TARGETS_GUARD");
        command
    }

    fn compare(&self, tool: &str, args: &[&str], backend: &str) {
        self.compare_with_env(tool, args, backend, &[]);
    }

    /// `compare` with extra environment variables on both the native and
    /// the routed run.
    fn compare_with_env(&self, tool: &str, args: &[&str], backend: &str, env: &[(&str, &str)]) {
        let mut native_command = self.command(tool);
        let mut routed_command = self.command(PIXEL);
        for (key, value) in env {
            native_command.env(key, value);
            routed_command.env(key, value);
        }
        let native = native_command.args(args).output().unwrap();
        let routed = routed_command
            .args(["search-like-rg", tool, "--"])
            .args(args)
            .output()
            .unwrap();
        assert_output_eq(&native, &routed, &format!("{tool} {args:?}"));
        if backend == "pixel" {
            let log = std::fs::read_to_string(self.0.join(".pixel/actions.jsonl")).unwrap();
            assert!(
                log.contains("backend=pixel"),
                "Pixel backend not observed: {log}"
            );
        } else {
            assert!(
                !self.0.join(".pixel/actions.jsonl").exists(),
                "native fallback must not add to the search corpus"
            );
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            crate::support::assert_no_daemon(&self.0);
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn assert_output_eq(native: &Output, routed: &Output, label: &str) {
    assert_eq!(
        routed.status.code(),
        native.status.code(),
        "status: {label}; {}",
        String::from_utf8_lossy(&routed.stderr)
    );
    assert_eq!(routed.stdout, native.stdout, "stdout: {label}");
    assert_eq!(routed.stderr, native.stderr, "stderr: {label}");
}

#[test]
fn literal_file_search_matches_native_bytes_and_status() {
    // `grep` is available on both macOS and GitHub's Ubuntu runners. The
    // routing contract is shared with `rg`, but the test must not require an
    // optional executable on the runner.
    for tool in ["grep"] {
        for flags in [
            vec![],
            vec!["-n"],
            vec!["-nH"],
            vec!["--line-number", "--no-filename"],
        ] {
            let fixture = Fixture::new(b"first needle\nplain\nlast needle");
            let mut args = flags;
            args.extend(["needle", "a file.rs"]);
            fixture.compare(tool, &args, "pixel");
        }
        Fixture::new(b"nothing here\n").compare(tool, &["-n", "needle", "a file.rs"], "pixel");
        Fixture::new(b"foo.bar\nfooXbar\n").compare(
            tool,
            &["-nF", "foo.bar", "a file.rs"],
            "pixel",
        );
    }
    Fixture::new(b"needle\n").compare("grep", &["-nHh", "needle", "a file.rs"], "pixel");
}

#[test]
fn more_than_enriched_search_default_is_not_truncated() {
    let content = "needle\n".repeat(150);
    Fixture::new(content.as_bytes()).compare("grep", &["-n", "needle", "a file.rs"], "pixel");
}

#[test]
fn unsupported_arguments_and_bytes_execute_original_native_search() {
    let cases: &[(&str, &[&str], &[u8])] = &[
        (
            "grep",
            &["-A", "2", "needle", "a file.rs"],
            b"needle\na\nb\n",
        ),
        ("grep", &["-i", "NEEDLE", "a file.rs"], b"needle\n"),
        ("grep", &["-n", "needle", "a file.rs"], b"needle\r\n"),
        (
            "grep",
            &["-n", "needle", "a file.rs"],
            b"needle\x00binary\n",
        ),
        (
            "grep",
            &["-n", "needle", "a file.rs"],
            "needle café\n".as_bytes(),
        ),
        ("grep", &["-n", "needle", "missing.rs"], b"needle\n"),
    ];
    for (tool, args, bytes) in cases {
        Fixture::new(bytes).compare(tool, args, "native");
    }
    let content = format!("needle {}\n", "x".repeat(70_000));
    Fixture::new(content.as_bytes()).compare("grep", &["needle", "a file.rs"], "native");
}

#[test]
fn modified_file_is_refreshed_before_compatibility_search() {
    let fixture = Fixture::new(b"old needle\n");
    fixture.compare("grep", &["needle", "a file.rs"], "pixel");
    std::fs::write(fixture.0.join("a file.rs"), b"new needle\n").unwrap();
    fixture.compare("grep", &["needle", "a file.rs"], "pixel");
}

#[test]
fn repeated_search_keeps_executing_and_reports_changed_file() {
    let fixture = Fixture::new(b"before needle\n");
    for attempt in 0..5 {
        if attempt == 4 {
            std::fs::write(fixture.0.join("a file.rs"), b"after needle\n").unwrap();
        }
        let output = fixture
            .command(PIXEL)
            .env_remove("PIXEL_TEST")
            .args([
                "search-content",
                "needle",
                "a file.rs",
                "--json",
                "--no-daemon",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "attempt {attempt}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        // The first NDJSON line is the match; the last is the page metadata.
        let stdout = String::from_utf8_lossy(&output.stdout);
        let row: serde_json::Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
        assert_eq!(
            row["text"],
            if attempt == 4 {
                "after needle"
            } else {
                "before needle"
            }
        );
        if attempt >= 2 {
            assert!(String::from_utf8_lossy(&output.stderr).contains("Continuing retrieval."));
        }
    }
}

/// A TRACKED `.env` whose body matches a token must not echo the token, the
/// path, or the matching line through `pixel search-content`. The daemon is
/// the single sink every output mode goes through, so a fixture
/// that asserts this in `--json`, human, and `-l` modes is the contract:
/// drop the credential bytes everywhere, name the hidden count in the
/// envelope so the partial answer is not silent (CONTRIBUTING.md: every cap
/// is named in `basis` and mirrored as a warning).
#[test]
fn tracked_dotenv_with_matching_token_is_hidden_from_search_content() {
    let fixture = tracked_credential_fixture();
    // Synthetic, nonsensitive bytes only — the test never reads real secrets.
    // Split so source-level secret scanners do not read the fixture as a token.
    let token = concat!("ABCDEF_GUARDED", "_TOKEN_xyzzy_42");

    // Human format: the path and the matching line must both be absent.
    let human = fixture
        .command(PIXEL)
        .args(["search-content", "-F", token, "."])
        .output()
        .unwrap();
    assert!(
        human.status.success(),
        "human: {}",
        String::from_utf8_lossy(&human.stderr)
    );
    let human_stdout = String::from_utf8_lossy(&human.stdout);
    // Only the credential-shaped paths must be hidden. The non-credential
    // `src/safe.rs` carries the same synthetic token and is allowed to
    // match; the contract is "drop matches from credential-shaped files,
    // not every line that happens to contain the token".
    assert!(
        !human_stdout.contains(".env"),
        "human stdout must not name the .env path: {human_stdout:?}"
    );
    assert!(
        !human_stdout.contains("secrets/"),
        "human stdout must not name the secrets/ path: {human_stdout:?}"
    );
    // The synthetic non-credential match is the positive case: it shows.
    assert!(
        human_stdout.contains("src/safe.rs"),
        "human stdout must still show the safe match: {human_stdout:?}"
    );

    // NDJSON: every match line carries a `path`; the page metadata line is
    // the last one. The hidden count must reach the caller via the envelope
    // so a partial answer is never silent.
    let json = fixture
        .command(PIXEL)
        .args(["search-content", "-F", token, ".", "--json"])
        .output()
        .unwrap();
    assert!(
        json.status.success(),
        "json: {}",
        String::from_utf8_lossy(&json.stderr)
    );
    let json_stdout = String::from_utf8_lossy(&json.stdout);
    // The synthetic non-credential match is the positive case: it appears.
    assert!(
        json_stdout.contains("src/safe.rs"),
        "json stdout must still carry the safe match: {json_stdout:?}"
    );
    // The hidden-match basis names the count: 2 (.env + secrets/real.pem).
    assert!(
        json_stdout.contains(
            "\"basis\":\"text index; caps: 2 match(es) in credential-shaped files hidden"
        ),
        "json trailer must name the hidden count: {json_stdout:?}"
    );
    let docs: Vec<serde_json::Value> = json_stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(
        !docs.is_empty(),
        "json page must carry at least the trailer"
    );
    for d in &docs[..docs.len() - 1] {
        assert!(d.get("path").is_some(), "match line missing path: {d}");
        let path = d["path"].as_str().unwrap_or("");
        assert!(!path.contains(".env"), "match path must not be .env: {d}");
    }
    let trailer = docs.last().unwrap();
    let warnings = trailer
        .get("warnings")
        .and_then(|w| w.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        warnings.iter().any(|w| {
            w.get("code").and_then(|c| c.as_str()) == Some("RESULT_CAPPED")
                && w.get("message")
                    .and_then(|m| m.as_str())
                    .is_some_and(|m| m.contains("credential-shaped"))
        }),
        "envelope warning must name the credential-shaped cap: {warnings:?}"
    );
    let basis = trailer
        .pointer("/epistemics/basis")
        .and_then(|b| b.as_str())
        .unwrap_or_default();
    assert!(
        basis.contains("credential-shaped"),
        "epistemics.basis must name the credential-shaped cap: {basis:?}"
    );

    // `-l` (files-only) must not list any credential-shaped path either.
    let files_only = fixture
        .command(PIXEL)
        .args(["search-content", "-F", token, ".", "-l"])
        .output()
        .unwrap();
    assert!(
        files_only.status.success(),
        "-l: {}",
        String::from_utf8_lossy(&files_only.stderr)
    );
    let lo_stdout = String::from_utf8_lossy(&files_only.stdout);
    assert!(
        lo_stdout
            .lines()
            .all(|line| !line.contains(".env") && !line.contains(token)),
        "-l must not list credential-shaped files or echo the token: {lo_stdout:?}"
    );
}

/// Builds a fixture with a tracked `.env`, a tracked non-credential file,
/// and a tracked `secrets/` directory holding a tracked `.pem`. The `.env`
/// and the `.pem` carry the test token; the non-credential file carries it
/// too, so the test can prove the daemon does NOT also hide matches in
/// safe files when it strips matches from credential-shaped ones.
fn tracked_credential_fixture() -> TrackedFixture {
    use std::process::Command;

    let root = std::env::temp_dir().join(format!(
        "pixel-search-content-credential-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("secrets")).unwrap();
    // Synthetic, nonsensitive fixture bytes only — the test never reads
    // real secrets, only asserts that the daemon hides matches in paths
    // whose shape looks credential-shaped. The literals are split with
    // `concat!` so source-level secret scanners do not flag the fixtures.
    std::fs::write(
        root.join(".env"),
        concat!("ABCDEF_GUARDED", "_TOKEN_xyzzy_42=please_do_not_match_me\n"),
    )
    .unwrap();
    std::fs::set_permissions(root.join(".env"), std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(
        root.join("secrets/real.pem"),
        concat!(
            "-----BEGIN PRIVATE",
            " KEY-----\nABCDEF_GUARDED",
            "_TOKEN_xyzzy_42\n-----END ...\n"
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("src/safe.rs"),
        concat!(
            "// ABCDEF_GUARDED",
            "_TOKEN_xyzzy_42 lives here too, but this is safe\n"
        ),
    )
    .unwrap();
    std::fs::write(root.join(".gitignore"), ".pixel/\n").unwrap();
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .current_dir(&root)
            .arg("-C")
            .arg(&root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&["commit", "-qm", "fixture"]);
    TrackedFixture(root.canonicalize().unwrap())
}

/// A git-anchored fixture that is removed on drop, after a daemon-leak
/// check (the same one `Fixture` and `Scratch` use elsewhere in this suite).
struct TrackedFixture(PathBuf);

impl TrackedFixture {
    fn command(&self, binary: &str) -> Command {
        let mut command = Command::new(binary);
        command
            .current_dir(&self.0)
            .env("PIXEL_DAEMON_AUTO_START", "0")
            // An empty home: no global `.pixel/config.yaml` sets the policy,
            // and no deployed prompt adds a note to stderr.
            .env("HOME", crate::support::neutral_home())
            .env_remove("RIPGREP_CONFIG_PATH")
            .env_remove("GREP_OPTIONS")
            .env_remove("PIXEL_POLICY")
            .env_remove("PIXEL_TARGETS_GUARD");
        command
    }
}

impl Drop for TrackedFixture {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            crate::support::assert_no_daemon(&self.0);
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Credential-shaped paths never qualify for the pixel backend: the search
/// runs natively and leaves no action record in the tree it searches.
#[test]
fn credential_shaped_paths_keep_native_permission_boundaries() {
    let fixture = Fixture::new(b"needle\n");
    std::fs::create_dir(fixture.0.join("secrets")).unwrap();
    // GitHub's runners ship no `rg` (see the premise note on the piped-stdin
    // test below); the rg case runs wherever ripgrep is installed.
    let rg_installed = Command::new("rg").arg("--version").output().is_ok();
    for path in [
        ".env",
        ".env.local",
        "credentials.json",
        "test.pem",
        "id_ed25519",
        "serviceAccountKey.json",
        "secrets/fake.rs",
    ] {
        // Synthetic, nonsensitive fixture bytes only. The search examines
        // path metadata, never the contents of credential-shaped files.
        std::fs::write(fixture.0.join(path), b"fake fixture\n").unwrap();
        fixture.compare("grep", &["needle", path], "native");
        if rg_installed {
            fixture.compare("rg", &["needle", path], "native");
        }
    }
}

/// A configuration for the OTHER tool changes nothing about the command
/// being emulated: an exported `RIPGREP_CONFIG_PATH` (every ripgrep user
/// with an rgrc) must not turn every `grep` emulation off. Execution applies
/// the same per-tool rule: `grep` still runs on the pixel backend under an rg
/// configuration, and stays native under its own `GREP_OPTIONS`.
#[test]
fn the_other_tools_configuration_does_not_keep_a_search_native() {
    Fixture::new(b"first needle\nplain\n").compare_with_env(
        "grep",
        &["-n", "needle", "a file.rs"],
        "pixel",
        &[("RIPGREP_CONFIG_PATH", "/nonexistent/rgrc")],
    );
    Fixture::new(b"first needle\nplain\n").compare_with_env(
        "grep",
        &["-n", "needle", "a file.rs"],
        "native",
        &[("GREP_OPTIONS", "")],
    );
}

/// Runs `command` with `input` on a pipe for stdin, the way an agent's shell
/// tool runs it: never a terminal.
fn run_with_piped_stdin(mut command: Command, input: &[u8]) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

/// The one documented divergence (module docs of `search_compat`): with no
/// path and a stdin that is a pipe, native `rg` searches stdin (and, in an
/// agent's shell tool, blocks until the call times out), while the rewritten
/// command searches the current directory, as native `rg` does with
/// `/dev/null` on stdin. The emulation never reads stdin, whether the pipe
/// carries input or stays open with nothing written to it.
#[test]
fn implicit_rg_search_should_search_the_cwd_even_with_a_piped_stdin() {
    let fixture = Fixture::new(b"needle in the file\n");
    let stdin = b"needle on stdin\n";

    let mut routed = fixture.command(PIXEL);
    routed.args(["search-like-rg", "rg", "--", "needle"]);
    let routed = run_with_piped_stdin(routed, stdin);
    assert_eq!(
        String::from_utf8_lossy(&routed.stdout),
        "a file.rs:needle in the file\n",
        "{}",
        String::from_utf8_lossy(&routed.stderr)
    );
    assert_eq!(routed.status.code(), Some(0));

    // An agent's shell tool: a pipe held open that nothing writes to. The
    // rewrite answers without waiting for input; the deadline bounds a
    // regression that would read stdin.
    let mut open = fixture.command(PIXEL);
    let mut child = open
        .args(["search-like-rg", "rg", "--", "needle"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let held_stdin = child.stdin.take();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let finished = child.try_wait().unwrap().is_some();
    if !finished {
        let _ = child.kill();
    }
    drop(held_stdin);
    let open = child.wait_with_output().unwrap();
    assert!(finished, "the rewrite waited on an open stdin");
    assert_eq!(open.stdout, routed.stdout);
    assert_eq!(open.status.code(), Some(0));

    // The premise, where `rg` is installed (GitHub's runners have none):
    // native `rg` reads the pipe instead, and searches the cwd only when
    // stdin is not a pipe or a file: `Command::output` gives it `/dev/null`.
    if Command::new("rg").arg("--version").output().is_ok() {
        let mut native = fixture.command("rg");
        native.arg("needle");
        let native = run_with_piped_stdin(native, stdin);
        assert_eq!(String::from_utf8_lossy(&native.stdout), "needle on stdin\n");
        let native = fixture.command("rg").arg("needle").output().unwrap();
        assert_eq!(native.stdout, routed.stdout);
    }
}
