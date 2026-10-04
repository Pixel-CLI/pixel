// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Core git-subprocess execution primitive: bounded by a wall-clock timeout
//! and a stdout byte cap, both enforced *during* the read (not after
//! buffering unbounded output first) — the exact defect class PLAN.md calls
//! out from usable-git's ingest path. Neither of the two existing Rust git
//! wrappers in this workspace enforces either bound today.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::GitError;
use crate::redact::redact;

/// Matches usable-git's `runner.ts` default (`defaultTimeoutMs = 120_000`).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
/// Matches usable-git's `runner.ts` default (`defaultMaxOutputBytes = 1_048_576`).
/// Only appropriate for calls whose output is inherently small and bounded
/// (a single OID, a branch name, a blob size) — see `ENUMERATION_MAX_OUTPUT_BYTES`
/// and `BLOB_MAX_OUTPUT_BYTES` for calls whose legitimate output can be much
/// larger than 1 MiB.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 1_048_576;

/// Cap for calls that enumerate repo-wide path lists (`ls-files`, `status
/// --porcelain`, `diff --name-status`, `diff --unified=0`). A real repo can
/// legitimately produce enumeration output well past 1 MiB — tens of
/// thousands of tracked files, or a large untracked tree — and treating
/// that overflow as "empty" is a correctness/safety bug, not graceful
/// degradation: it has previously caused the index to appear empty above
/// ~25k files and, far worse, caused `pixel rescue --apply`'s dirty-file
/// guard to see a large untracked tree, overflow `status --porcelain`, and
/// silently conclude "nothing is dirty" — overwriting uncommitted work with
/// no strategy flag given. 64 MiB is generous enough that legitimate
/// enumeration output essentially never hits it, while still bounding
/// worst-case memory use.
pub const ENUMERATION_MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

/// Cap for blob-content reads (`show_blob`, `show_blob_string`). Must stay
/// equal to `pixel_index::index::MAX_FILE_BYTES` (currently 4 MiB) — that
/// constant is the contract for "this file is small enough to index", and a
/// blob-read cap smaller than it silently drops indexable files (observed:
/// files between 1 MiB and 4 MiB were dropped from the index even though
/// `MAX_FILE_BYTES` said they should be kept). pixel-git does not depend on
/// pixel-index, so this value is duplicated rather than shared — if either
/// constant changes, update the other to match.
pub const BLOB_MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct GitOptions {
    /// `None` = no timeout enforced.
    pub timeout: Option<Duration>,
    /// `None` = stdout size unbounded.
    pub max_output_bytes: Option<usize>,
}

impl Default for GitOptions {
    fn default() -> Self {
        GitOptions {
            timeout: Some(DEFAULT_TIMEOUT),
            max_output_bytes: Some(DEFAULT_MAX_OUTPUT_BYTES),
        }
    }
}

/// What a git call produced when its exit status is data rather than
/// failure (`merge-tree --write-tree` exits 1 on conflicts and still prints
/// the conflicted entries; `rebase --continue` reports "nothing to continue"
/// through its status). The timeout and the stdout cap still apply and are
/// still errors: only the exit code is handed back instead of being turned
/// into [`GitError::NonZeroExit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitOutput {
    /// `status.code()`: `None` when the process died from a signal.
    pub code: Option<i32>,
    /// Raw stdout, complete (the cap would have been an error).
    pub stdout: Vec<u8>,
    /// stderr, trimmed and redacted like the text of a `NonZeroExit`.
    pub stderr: String,
}

impl GitOutput {
    /// Exit code 0.
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

pub struct GitRunner {
    root: PathBuf,
    options: GitOptions,
}

impl GitRunner {
    /// Sensible defaults: 120s timeout, 1MiB stdout cap.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            options: GitOptions::default(),
        }
    }

    pub fn with_options(root: impl Into<PathBuf>, options: GitOptions) -> Self {
        Self {
            root: root.into(),
            options,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A runner over the same root and timeout, with `max_output_bytes`
    /// overridden. Lets individual plumbing calls (enumeration vs. blob
    /// reads vs. small fixed-shape output) pick the cap appropriate to their
    /// own worst-case legitimate output size, instead of every call sharing
    /// one construction-time default that is too small for some call sites
    /// and unnecessarily large for others.
    pub fn with_max_output_bytes(&self, max_output_bytes: Option<usize>) -> Self {
        Self {
            root: self.root.clone(),
            options: GitOptions {
                timeout: self.options.timeout,
                max_output_bytes,
            },
        }
    }

    /// Runs `git -C <root> <args>`, enforcing the configured timeout and
    /// output byte cap. Returns raw stdout bytes on success (status 0).
    /// stderr on failure is redacted before being embedded in `GitError`.
    pub fn run(&self, args: &[&str]) -> Result<Vec<u8>, GitError> {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&self.root).args(args);
        let arg_strings: Vec<String> = args.iter().map(ToString::to_string).collect();
        execute(cmd, arg_strings, &self.options)
    }

    /// Run repository plumbing without inherited Git routing, injected config,
    /// global configuration or hooks. Existing callers retain `run` semantics.
    pub fn run_isolated(&self, args: &[&str]) -> Result<Vec<u8>, GitError> {
        let mut cmd = Command::new("git");
        for (name, _) in std::env::vars_os() {
            if name.as_encoded_bytes().starts_with(b"GIT_") {
                cmd.env_remove(name);
            }
        }
        cmd.env_remove("ANTHROPIC_API_KEY")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .arg("-C")
            .arg(&self.root)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
            ])
            .args(args);
        let arg_strings = args.iter().map(ToString::to_string).collect();
        execute(cmd, arg_strings, &self.options)
    }

    /// Same as `run` but returns `None` instead of erroring — the "graceful
    /// degradation outside a git repo" behavior `gitsync.rs` relies on
    /// today.
    pub fn run_opt(&self, args: &[&str]) -> Option<Vec<u8>> {
        self.run(args).ok()
    }

    /// Runs `git -C <root> <args>` with `env` added to the child's
    /// environment and hands back stdout plus the exit code whatever the
    /// status was. For the calls where a non-zero exit is an answer, not a
    /// failure: `merge-tree --write-tree` (1 = conflicts, with the entries
    /// on stdout) and `rebase --continue` (`GIT_EDITOR=true` so no editor
    /// opens). The timeout and the output cap of this runner still apply:
    /// they are the only `Err`s, so a caller never blocks on a hung
    /// subprocess the way a bare `std::process::Command` would.
    pub fn run_output(&self, args: &[&str], env: &[(&str, &str)]) -> Result<GitOutput, GitError> {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&self.root).args(args);
        for (key, value) in env {
            cmd.env(key, value);
        }
        let arg_strings: Vec<String> = args.iter().map(ToString::to_string).collect();
        execute_output(cmd, arg_strings, &self.options)
    }

    /// Runs `git -C <root> <args>` with `input` written to the child's
    /// stdin, and hands back stdout plus the exit code whatever the status
    /// was (like [`GitRunner::run_output`]). For `apply` fed a patch and
    /// `hash-object --stdin`: the two calls the task sandbox used to spawn
    /// bare, with no timeout and no output cap. The input is written from
    /// its own thread so a patch larger than the pipe buffer cannot
    /// deadlock against a child that is already producing output.
    pub fn run_with_stdin(&self, args: &[&str], input: &[u8]) -> Result<GitOutput, GitError> {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&self.root).args(args);
        let arg_strings: Vec<String> = args.iter().map(ToString::to_string).collect();
        execute_output_with_stdin(cmd, arg_strings, &self.options, Some(input.to_vec()))
    }

    /// Read every object `specs` names (`<commit>:<path>`, a blob oid, any
    /// object name git accepts), calling `visit(index, object)` once per
    /// spec, in no particular order. Blobs of at most `max_blob_bytes` come
    /// back with their content, larger ones as
    /// [`crate::BatchObject::Oversized`] without git ever inflating them.
    ///
    /// Two processes whatever the number of specs: `git cat-file
    /// --batch-check` sizes every object first (bounded like any `run`), then
    /// `git cat-file --batch` streams the content of the blobs under the cap,
    /// asked by the oids the check resolved, so the second pass reads exactly
    /// what the first one measured. On that stream the runner's timeout is an
    /// idle limit, not a total: git is given up on when it goes that long
    /// without the next answer, never for the time `visit` spends.
    ///
    /// # Errors
    ///
    /// A spawn failed, either git exited early or unsuccessfully, the check
    /// did not answer every spec, or the stream stopped answering for the
    /// timeout. Specs visited before the failure keep their answer; the
    /// rest are never visited.
    pub fn cat_file_blobs<F>(
        &self,
        specs: &[String],
        max_blob_bytes: u64,
        mut visit: F,
    ) -> Result<(), GitError>
    where
        F: FnMut(usize, crate::batch::BatchObject<'_>),
    {
        let (sent, input) = crate::batch::requests(specs, &mut visit);
        if sent.is_empty() {
            return Ok(());
        }
        let check_args = ["cat-file", "--batch-check"];
        let check = self
            .with_max_output_bytes(Some(ENUMERATION_MAX_OUTPUT_BYTES))
            .run_with_stdin(&check_args, &input)?;
        if !check.success() {
            return Err(GitError::NonZeroExit {
                args: check_args.iter().map(ToString::to_string).collect(),
                code: check.code,
                stderr: check.stderr,
            });
        }
        let reads = crate::batch::plan_reads(&check.stdout, &sent, max_blob_bytes, &mut visit)
            .map_err(GitError::Io)?;
        let oids: Vec<String> = reads.iter().map(|(_, oid)| oid.clone()).collect();
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(self.root()).args(["cat-file", "--batch"]);
        crate::batch::batch_session(
            cmd,
            &oids,
            max_blob_bytes,
            self.options.timeout,
            |j, object| {
                visit(reads[j].0, object);
            },
        )
    }

    /// Runs `git merge-file <current> <base> <other>` — git's result is the
    /// exit code, not its output: 0 means a clean merge, a positive count
    /// means that many conflicts were left with markers in `current`,
    /// negative (or `None` for a signal) means a real failure. Bounded like
    /// every other call: the runner's timeout and output cap apply, and
    /// stderr is captured and redacted.
    pub fn merge_file(
        &self,
        current: &Path,
        base: &Path,
        other: &Path,
    ) -> Result<GitOutput, GitError> {
        self.run_merge_file(current, base, other, None)
    }

    /// [`GitRunner::merge_file`] with the three `-L` diff3 conflict-marker
    /// labels when `labels` is `Some` (ours, base, theirs, in marker order);
    /// git's own file-name labels otherwise. The one bounded primitive both
    /// merge-file entry points go through
    /// (`plumbing::merge_file_with_labels` is the other), so neither can
    /// spawn a git that outlives the deadline or floods a pipe. The merged
    /// text is written into `current`, not stdout, which is why the
    /// runner's default cap leaves ample room here.
    pub(crate) fn run_merge_file(
        &self,
        current: &Path,
        base: &Path,
        other: &Path,
        labels: Option<[&str; 3]>,
    ) -> Result<GitOutput, GitError> {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&self.root).arg("merge-file");
        let mut args_for_err = vec!["merge-file".to_string()];
        if let Some(labels) = labels {
            for label in labels {
                cmd.arg("-L").arg(label);
                args_for_err.push("-L".to_string());
                args_for_err.push(label.to_string());
            }
        }
        for path in [current, base, other] {
            cmd.arg(path);
            args_for_err.push(path.display().to_string());
        }
        execute_output(cmd, args_for_err, &self.options)
    }
}

/// Why a capped read stopped before EOF, when it did: the output exceeded
/// the cap, or the read itself failed. `Ok(bytes)` is EOF, i.e. the bytes
/// are the complete output.
#[derive(Debug)]
enum ReadOutcome {
    /// More than `cap` bytes arrived; the read stopped there.
    Overflow,
    /// Reading the pipe failed. What was read so far is a truncated prefix,
    /// never a complete output, so the caller must not pass it on as one.
    Io(std::io::Error),
}

impl ReadOutcome {
    /// The error a stopped read means to a caller of the runner: stdout
    /// past the cap, or the pipe failing under the reader.
    fn into_git_error(self, args: Vec<String>, cap: Option<usize>) -> GitError {
        match self {
            ReadOutcome::Overflow => GitError::OutputTooLarge {
                args,
                cap: cap.unwrap_or(0),
            },
            ReadOutcome::Io(e) => GitError::Io(e),
        }
    }
}

/// Read `r` into a growing buffer, stopping (and signalling overflow) as
/// soon as the byte count exceeds `cap` — never buffers past the cap. A
/// failed read is an error, never a short-but-complete buffer: a truncated
/// `ls-files`/`status` passed on as complete is how a dirty tree reads as
/// clean.
fn read_capped<R: Read>(mut r: R, cap: Option<usize>) -> Result<Vec<u8>, ReadOutcome> {
    let mut buf = Vec::new();
    // Heap, not stack: 64 KiB is past the stack-array lint's limit and this
    // runs on the daemon's request threads.
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        match r.read(&mut chunk) {
            Ok(0) => return Ok(buf),
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if let Some(cap) = cap
                    && buf.len() > cap
                {
                    return Err(ReadOutcome::Overflow);
                }
            }
            Err(e) => return Err(ReadOutcome::Io(e)),
        }
    }
}

/// The shared execution primitive behind `GitRunner::run`. Takes an
/// already-configured `Command` (program + args already set) plus the args
/// for error messages, so the timeout/cap machinery can be exercised
/// directly against non-git commands in tests (see `mod tests` below) —
/// proving the exact poll/kill logic `run()` uses, without depending on a
/// git hook or a slow git operation to create a deterministic hang.
fn execute(
    cmd: Command,
    args_for_err: Vec<String>,
    options: &GitOptions,
) -> Result<Vec<u8>, GitError> {
    let out = execute_output(cmd, args_for_err.clone(), options)?;
    if !out.success() {
        return Err(GitError::NonZeroExit {
            args: args_for_err,
            code: out.code,
            stderr: out.stderr,
        });
    }
    Ok(out.stdout)
}

/// The primitive under `execute` and `GitRunner::run_output`: spawn, read
/// stdout and stderr under the byte cap, poll for the timeout, and return
/// the exit code as data. Only a timeout, an overflow or an I/O failure is
/// an `Err` here; `execute` is the layer that makes a non-zero exit one.
fn execute_output(
    cmd: Command,
    args_for_err: Vec<String>,
    options: &GitOptions,
) -> Result<GitOutput, GitError> {
    execute_output_with_stdin(cmd, args_for_err, options, None)
}

/// `execute_output` with an optional stdin payload. `None` closes the
/// child's stdin (`Stdio::null`); `Some` pipes it and writes the bytes from
/// a writer thread so the timeout loop below keeps running while the child
/// consumes them. A child that exits before reading everything (a broken
/// pipe) is not an error here: its exit code and stderr say what happened.
fn execute_output_with_stdin(
    mut cmd: Command,
    args_for_err: Vec<String>,
    options: &GitOptions,
    input: Option<Vec<u8>>,
) -> Result<GitOutput, GitError> {
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });

    let mut child = cmd.spawn()?;

    let writer_thread = input.map(|bytes| {
        let mut stdin = child.stdin.take().expect("piped stdin");
        std::thread::spawn(move || {
            use std::io::Write;
            let _ = stdin.write_all(&bytes);
            // Dropping `stdin` closes the pipe: EOF for the child.
        })
    });

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let max_out = options.max_output_bytes;

    let stdout_thread = std::thread::spawn(move || read_capped(stdout, max_out));
    let stderr_thread =
        std::thread::spawn(move || read_capped(stderr, max_out).unwrap_or_default());

    let start = Instant::now();
    let mut timed_out = false;
    loop {
        if stdout_thread.is_finished() {
            break;
        }
        match child.try_wait() {
            Ok(Some(_)) => {
                // Process exited; give the reader thread a moment to drain
                // the now-closing pipe (EOF should arrive promptly).
                if stdout_thread.is_finished() {
                    break;
                }
            }
            Ok(None) => {}
            Err(e) => return Err(GitError::Io(e)),
        }
        if let Some(timeout) = options.timeout
            && start.elapsed() >= timeout
        {
            timed_out = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    if timed_out {
        let _ = child.kill();
        let _ = child.wait();
        // The reader and writer threads are left to end on their own
        // (dropping a `JoinHandle` detaches it). They see EOF when the last
        // write end of their pipe closes, and a grandchild that inherited a
        // pipe — a hook that backgrounds a process — can hold it open far
        // past the deadline; joining them here would hand it the power to
        // block the caller, the very hang this deadline exists for.
        return Err(GitError::Timeout { args: args_for_err });
    }

    let stdout_result = stdout_thread
        .join()
        .map_err(|_| GitError::Io(std::io::Error::other("stdout reader thread panicked")))?;
    let stdout_bytes = match stdout_result {
        Ok(bytes) => bytes,
        Err(outcome) => {
            let _ = child.kill();
            let _ = child.wait();
            // Detached for the same reason as the timeout above.
            return Err(outcome.into_git_error(args_for_err, max_out));
        }
    };

    // The reader reaching EOF is not the same as the child exiting. A git
    // that closes its own fd 1 (a pager, a hook, a merge driver) leaves the
    // reader at EOF while the process keeps running, and the `child.wait()`
    // below would then block the caller with no deadline at all — the
    // opposite of this crate's contract. Wait for the exit under the same
    // deadline as the read above; `wait()` then returns the status `try_wait`
    // already collected.
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(e) => return Err(GitError::Io(e)),
        }
        if let Some(timeout) = options.timeout
            && start.elapsed() >= timeout
        {
            let _ = child.kill();
            let _ = child.wait();
            // Detached for the same reason as the timeout above.
            return Err(GitError::Timeout { args: args_for_err });
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let status = child.wait()?;
    if let Some(writer) = writer_thread {
        let _ = writer.join();
    }
    let stderr_bytes = stderr_thread
        .join()
        .map_err(|_| GitError::Io(std::io::Error::other("stderr reader thread panicked")))?;

    Ok(GitOutput {
        code: status.code(),
        stdout: stdout_bytes,
        stderr: redact(String::from_utf8_lossy(&stderr_bytes).trim()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolated_git_obeys_root_and_ignores_external_routing_config_and_hooks() {
        use std::os::unix::fs::PermissionsExt;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base =
            std::env::temp_dir().join(format!("pixel-isolated-{}-{nonce}", std::process::id()));
        let root = base.join("root");
        let foreign = base.join("foreign");
        for directory in [&root, &foreign] {
            std::fs::create_dir_all(directory).unwrap();
            GitRunner::new(directory)
                .run_isolated(&["init", "-q"])
                .unwrap();
        }
        let hook = root.join(".git/hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\nexit 23\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let global = base.join("global.gitconfig");
        let system = base.join("system.gitconfig");
        std::fs::write(&global, "[pixel]\nfromglobal = hostile\n").unwrap();
        std::fs::write(&system, "[pixel]\nfromsystem = hostile\n").unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "runner::tests::isolated_git_child", "--ignored"])
            .env("PIXEL_ISOLATED_TEST_ROOT", &root)
            .env("GIT_DIR", foreign.join(".git"))
            .env("GIT_WORK_TREE", &foreign)
            .env("GIT_INDEX_FILE", foreign.join(".git/index"))
            .env("GIT_CONFIG_GLOBAL", &global)
            .env("GIT_CONFIG_SYSTEM", &system)
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "core.worktree")
            .env("GIT_CONFIG_VALUE_0", &foreign)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(!foreign.join(".git/index").exists());
        assert!(
            GitRunner::new(&foreign)
                .run_isolated(&["rev-parse", "--verify", "HEAD"])
                .is_err()
        );
        assert!(
            !GitRunner::new(&root)
                .run_isolated(&["rev-parse", "--verify", "HEAD"])
                .unwrap()
                .is_empty()
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    #[ignore = "subprocess fixture with isolated process environment"]
    fn isolated_git_child() {
        let root = PathBuf::from(std::env::var_os("PIXEL_ISOLATED_TEST_ROOT").unwrap());
        let runner = GitRunner::new(&root);
        let top = runner
            .run_isolated(&["rev-parse", "--show-toplevel"])
            .unwrap();
        assert_eq!(
            String::from_utf8(top).unwrap().trim(),
            root.canonicalize().unwrap().to_str().unwrap()
        );
        assert!(
            runner
                .run_isolated(&["config", "--get", "pixel.fromglobal"])
                .is_err()
        );
        assert!(
            runner
                .run_isolated(&["config", "--get", "pixel.fromsystem"])
                .is_err()
        );
        let committed = runner.run_isolated(&[
            "-c",
            "user.name=Isolated test",
            "-c",
            "user.email=isolated@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-qm",
            "isolated",
        ]);
        assert!(
            committed.is_ok(),
            "local pre-commit hook must not execute: {committed:?}"
        );
        let format = runner.run_isolated(&["log", "-1", "--format=%s"]).unwrap();
        assert_eq!(format, b"isolated\n");
    }

    #[test]
    fn timeout_kills_a_hanging_process_promptly() {
        // Exercises the exact poll/kill code path GitRunner::run uses,
        // against a plain `sleep 5` instead of a contrived git hang — fast
        // and deterministic (should return in well under a second).
        let mut cmd = Command::new("sleep");
        cmd.arg("5");
        let options = GitOptions {
            timeout: Some(Duration::from_millis(100)),
            max_output_bytes: None,
        };
        let start = Instant::now();
        let result = execute(cmd, vec!["sleep".into(), "5".into()], &options);
        let elapsed = start.elapsed();
        assert!(
            matches!(result, Err(GitError::Timeout { .. })),
            "expected Timeout, got {result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "timeout enforcement took too long: {elapsed:?}"
        );
    }

    #[test]
    fn a_git_that_closes_its_stdout_does_not_outrun_the_deadline() {
        // A git that closes its own fd 1 (a pager, a hook, a merge driver)
        // gives the reader EOF while the process keeps running. The wait for
        // the child must stay under the same deadline as the read: the old
        // `child.wait()` after the loop blocked for the full 5s here.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exec 1>&-; sleep 5"]);
        let options = GitOptions {
            timeout: Some(Duration::from_millis(100)),
            max_output_bytes: None,
        };
        let start = Instant::now();
        let result = execute(cmd, vec!["sh".into(), "-c".into()], &options);
        let elapsed = start.elapsed();
        assert!(
            matches!(result, Err(GitError::Timeout { .. })),
            "expected Timeout, got {result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "the closed stdout let the wait outrun the deadline: {elapsed:?}"
        );
    }

    #[test]
    fn a_command_that_closes_stdout_then_exits_still_succeeds() {
        // The child-exit poll loop has its own timeout check; a child that
        // closes fd 1 and then exits quickly must not be killed by that poll.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exec 1>&-; sleep 0.05"]);
        let options = GitOptions {
            timeout: Some(Duration::from_millis(500)),
            max_output_bytes: None,
        };
        let result = execute(cmd, vec!["sh".into(), "-c".into()], &options);
        assert!(result.is_ok(), "expected success, got {result:?}");
    }

    /// A pipe that yields one chunk and then fails the way a closed or
    /// broken pipe does, instead of reporting EOF.
    struct FailingPipe {
        first: Option<Vec<u8>>,
    }

    impl Read for FailingPipe {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.first.take() {
                Some(bytes) => {
                    let n = bytes.len().min(buf.len());
                    buf[..n].copy_from_slice(&bytes[..n]);
                    Ok(n)
                }
                None => Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "simulated read failure",
                )),
            }
        }
    }

    #[test]
    fn a_failed_read_is_an_error_not_a_complete_output() {
        // Half a `status --porcelain` read back as a complete one is how a
        // dirty tree looks clean, so a read error must not be swallowed
        // into the bytes collected so far.
        let result = read_capped(
            FailingPipe {
                first: Some(b"partial".to_vec()),
            },
            None,
        );
        assert!(
            matches!(result, Err(ReadOutcome::Io(_))),
            "expected the read failure, got {result:?}"
        );
    }

    #[test]
    fn output_cap_kills_an_infinite_producer_promptly() {
        // `yes` produces "y\n" forever; with a tiny cap the reader thread
        // must detect the overflow and the child must be killed instead of
        // buffering unboundedly or hanging.
        let cmd = Command::new("yes");
        let options = GitOptions {
            timeout: Some(Duration::from_secs(10)),
            max_output_bytes: Some(64),
        };
        let start = Instant::now();
        let result = execute(cmd, vec!["yes".into()], &options);
        let elapsed = start.elapsed();
        assert!(
            matches!(result, Err(GitError::OutputTooLarge { cap: 64, .. })),
            "expected OutputTooLarge, got {result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "output cap enforcement took too long: {elapsed:?}"
        );
    }

    #[test]
    fn execute_output_keeps_stdout_and_the_exit_code_on_a_nonzero_exit() {
        // The whole point of the primitive: `merge-tree` prints its answer
        // and exits 1, so a non-zero exit must not discard stdout.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo kept; echo warned >&2; exit 3"]);
        let out = execute_output(cmd, vec!["sh".into()], &GitOptions::default()).unwrap();
        assert_eq!(out.code, Some(3));
        assert!(!out.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout), "kept\n");
        assert_eq!(out.stderr, "warned");

        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo fine"]);
        let out = execute_output(cmd, vec!["sh".into()], &GitOptions::default()).unwrap();
        assert_eq!(out.code, Some(0));
        assert!(out.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout), "fine\n");
    }

    #[test]
    fn execute_output_still_times_out() {
        // A caller that wants the exit code as data still must never hang
        // the daemon's request thread on a stuck subprocess.
        let mut cmd = Command::new("sleep");
        cmd.arg("5");
        let options = GitOptions {
            timeout: Some(Duration::from_millis(100)),
            max_output_bytes: None,
        };
        let start = Instant::now();
        let result = execute_output(cmd, vec!["sleep".into(), "5".into()], &options);
        assert!(
            matches!(result, Err(GitError::Timeout { .. })),
            "expected Timeout, got {result:?}"
        );
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn run_output_passes_env_to_git_and_reports_the_status() {
        // `git var GIT_EDITOR` echoes the variable the child was given:
        // proves the env pairs reach git (what `rebase --continue` relies
        // on to not open an editor) and that a real git non-zero exit
        // (`rev-parse` of an unknown ref) comes back as a code, not an Err.
        let dir = std::env::temp_dir().join(format!(
            "pixel-git-runoutput-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let init = Command::new("git")
            .args(["init", "-q"])
            .arg(&dir)
            .status()
            .unwrap();
        assert!(init.success());
        let runner = GitRunner::new(&dir);

        let out = runner
            .run_output(
                &["var", "GIT_EDITOR"],
                &[("GIT_EDITOR", "pixel-test-editor")],
            )
            .unwrap();
        assert_eq!(out.code, Some(0));
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "pixel-test-editor"
        );

        let out = runner
            .run_output(&["rev-parse", "--verify", "-q", "no-such-ref"], &[])
            .unwrap();
        assert_eq!(out.code, Some(1), "{out:?}");
        assert!(!out.success());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_with_stdin_feeds_the_child_and_reports_its_status() {
        // `hash-object --stdin` is the sandbox's use: the OID of "hello\n"
        // is fixed by git's object format, so a wrong or truncated payload
        // (or stdin left closed) gives a different hash, not a flake.
        let dir = std::env::temp_dir().join(format!(
            "pixel-git-stdin-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let init = Command::new("git")
            .args(["init", "-q"])
            .arg(&dir)
            .status()
            .unwrap();
        assert!(init.success());
        let runner = GitRunner::new(&dir);

        let out = runner
            .run_with_stdin(&["hash-object", "--stdin"], b"hello\n")
            .unwrap();
        assert_eq!(out.code, Some(0), "{out:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );

        // A payload past the pipe buffer must not deadlock: the writer
        // runs on its own thread while stdout is drained.
        let big = vec![b'x'; 4 * 1024 * 1024];
        let out = runner
            .run_with_stdin(&["hash-object", "--stdin"], &big)
            .unwrap();
        assert_eq!(out.code, Some(0), "{out:?}");
        assert_eq!(out.stdout.len(), 41, "one OID line: {out:?}");

        // A malformed patch: git exits non-zero, stderr comes back as
        // data (what `promote` reports as `apply_failed:<stderr>`).
        let out = runner
            .run_with_stdin(&["apply", "--check"], b"not a patch\n")
            .unwrap();
        assert_ne!(out.code, Some(0), "{out:?}");
        assert!(!out.success());
        assert!(!out.stderr.is_empty(), "{out:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_with_stdin_still_times_out() {
        // A stuck consumer must be killed like any other child; the writer
        // thread must not keep the call alive after the deadline.
        let mut cmd = Command::new("sleep");
        cmd.arg("5");
        let options = GitOptions {
            timeout: Some(Duration::from_millis(100)),
            max_output_bytes: None,
        };
        let start = Instant::now();
        let result = execute_output_with_stdin(
            cmd,
            vec!["sleep".into(), "5".into()],
            &options,
            Some(b"ignored".to_vec()),
        );
        assert!(
            matches!(result, Err(GitError::Timeout { .. })),
            "expected Timeout, got {result:?}"
        );
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn successful_command_returns_stdout() {
        let mut cmd = Command::new("echo");
        cmd.arg("hello");
        let options = GitOptions::default();
        let result = execute(cmd, vec!["echo".into(), "hello".into()], &options).unwrap();
        assert_eq!(String::from_utf8_lossy(&result).trim(), "hello");
    }

    #[test]
    fn merge_file_performs_a_clean_three_way_merge() {
        let dir = std::env::temp_dir().join(format!(
            "pixel-git-mergefile-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("base.txt"), "line1\nline2\nline3\n").unwrap();
        std::fs::write(dir.join("current.txt"), "line1-mine\nline2\nline3\n").unwrap();
        std::fs::write(dir.join("other.txt"), "line1\nline2\nline3-theirs\n").unwrap();

        let runner = GitRunner::new(&dir);
        let merged = runner
            .merge_file(
                &dir.join("current.txt"),
                &dir.join("base.txt"),
                &dir.join("other.txt"),
            )
            .expect("merge-file spawns");
        assert_eq!(
            merged.code,
            Some(0),
            "expected a clean, conflict-free merge"
        );
        let merged = std::fs::read_to_string(dir.join("current.txt")).unwrap();
        assert!(merged.contains("line1-mine"));
        assert!(merged.contains("line3-theirs"));
    }

    #[test]
    fn nonzero_exit_is_reported_with_redacted_stderr() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("echo 'token=verysecret123' >&2; exit 3");
        let options = GitOptions::default();
        let result = execute(cmd, vec!["sh".into()], &options);
        match result {
            Err(GitError::NonZeroExit { code, stderr, .. }) => {
                assert_eq!(code, Some(3));
                assert!(!stderr.contains("verysecret123"));
            }
            other => panic!("expected NonZeroExit, got {other:?}"),
        }
    }
}
