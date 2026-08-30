//! Core git-subprocess execution primitive: bounded by a wall-clock timeout
//! and a stdout byte cap, both enforced *during* the read (not after
//! buffering unbounded output first) — the exact defect class PLAN.md calls
//! out from usable-git's ingest path. Neither of the two existing Rust git
//! wrappers in this workspace enforces either bound today.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
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
        let arg_strings: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        execute(cmd, arg_strings, &self.options)
    }

    /// Same as `run` but returns `None` instead of erroring — the "graceful
    /// degradation outside a git repo" behavior `gitsync.rs` relies on
    /// today.
    pub fn run_opt(&self, args: &[&str]) -> Option<Vec<u8>> {
        self.run(args).ok()
    }

    /// Runs `git merge-file <current> <base> <other>` (same positional
    /// semantics as `pixel-cli::rescue_cmd`'s invocation, minus the
    /// rescue-specific `-L` diff3 conflict-marker labels, which are cosmetic
    /// and derived from rescue's own oid/state context rather than being
    /// part of a generic git wrapper). Returns the raw exit status: 0 means
    /// a clean merge, a positive count means that many conflicts were left
    /// with markers in `current`, negative means a real failure.
    pub fn merge_file(
        &self,
        current: &Path,
        base: &Path,
        other: &Path,
    ) -> Result<ExitStatus, GitError> {
        Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .arg("merge-file")
            .arg(current)
            .arg(base)
            .arg(other)
            .status()
            .map_err(GitError::from)
    }
}

/// Read `r` into a growing buffer, stopping (and signalling overflow) as
/// soon as the byte count exceeds `cap` — never buffers past the cap.
fn read_capped<R: Read>(mut r: R, cap: Option<usize>) -> Result<Vec<u8>, ()> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        match r.read(&mut chunk) {
            Ok(0) => return Ok(buf),
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if let Some(cap) = cap
                    && buf.len() > cap
                {
                    return Err(());
                }
            }
            Err(_) => return Ok(buf),
        }
    }
}

/// The shared execution primitive behind `GitRunner::run`. Takes an
/// already-configured `Command` (program + args already set) plus the args
/// for error messages, so the timeout/cap machinery can be exercised
/// directly against non-git commands in tests (see `mod tests` below) —
/// proving the exact poll/kill logic `run()` uses, without depending on a
/// git hook or a slow git operation to create a deterministic hang.
fn execute(mut cmd: Command, args_for_err: Vec<String>, options: &GitOptions) -> Result<Vec<u8>, GitError> {
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.stdin(Stdio::null());

    let mut child = cmd.spawn()?;

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let max_out = options.max_output_bytes;

    let stdout_thread = std::thread::spawn(move || read_capped(stdout, max_out));
    let stderr_thread = std::thread::spawn(move || read_capped(stderr, max_out).unwrap_or_default());

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
        let _ = stdout_thread.join();
        let _ = stderr_thread.join();
        return Err(GitError::Timeout {
            args: args_for_err,
        });
    }

    let stdout_result = stdout_thread
        .join()
        .map_err(|_| GitError::Io(std::io::Error::other("stdout reader thread panicked")))?;

    if stdout_result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
        let _ = stderr_thread.join();
        return Err(GitError::OutputTooLarge {
            args: args_for_err,
            cap: max_out.unwrap_or(0),
        });
    }
    let stdout_bytes = stdout_result.unwrap();

    let status = child.wait()?;
    let stderr_bytes = stderr_thread
        .join()
        .map_err(|_| GitError::Io(std::io::Error::other("stderr reader thread panicked")))?;

    if !status.success() {
        let stderr_text = redact(String::from_utf8_lossy(&stderr_bytes).trim());
        return Err(GitError::NonZeroExit {
            args: args_for_err,
            code: status.code(),
            stderr: stderr_text,
        });
    }

    Ok(stdout_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let status = runner
            .merge_file(
                &dir.join("current.txt"),
                &dir.join("base.txt"),
                &dir.join("other.txt"),
            )
            .expect("merge-file spawns");
        assert_eq!(status.code(), Some(0), "expected a clean, conflict-free merge");
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
