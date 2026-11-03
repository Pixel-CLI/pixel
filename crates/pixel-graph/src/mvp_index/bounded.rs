// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The one way the indexer waits on `git`: with a deadline and an output cap.
//!
//! A hung `git` (a credential helper, a network filesystem, a long wait on
//! `index.lock`) froze every graph refresh with no deadline, and a huge
//! `ls-files` or `status` was read whole into memory. `mvp-*` may not depend
//! on `pixel-git`, whose `GitRunner` bounds both, so this module does the
//! same job for the few commands `tree` and `git_index` run: the child is
//! killed at the deadline or once its stdout passes the cap, and the caller
//! gets a [`Failure`] naming which bound it hit.

use std::fmt;
use std::io::{self, Read, Write};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long one git command may run. The same bound as `pixel-git`'s
/// `DEFAULT_TIMEOUT`: a refresh of a very large tree stays well inside it,
/// a wedged command stops holding the graph.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(120);

/// The most stdout one git command may produce: 64 MiB, `pixel-git`'s
/// `ENUMERATION_MAX_OUTPUT_BYTES`, since `ls-files` and `status` list every
/// path of the tree and a real repository passes 1 MiB easily.
pub const GIT_MAX_OUTPUT: usize = 67_108_864;

/// The stderr kept for an error message; the rest is read and dropped so a
/// chatty child never blocks on a full pipe.
const STDERR_KEPT: usize = 65_536;

/// How often the exit of the child is checked while its pipes stay open.
const EXIT_POLL: Duration = Duration::from_millis(10);

/// A command that ran to its end within both bounds.
#[derive(Debug)]
pub struct Bounded {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Why a bounded command gave no output.
#[derive(Debug)]
pub enum Failure {
    /// The child could not be started (git missing, say) or awaited.
    Io(io::Error),
    /// The child was still running at the deadline and was killed.
    TimedOut(Duration),
    /// The child wrote more than the cap on stdout and was killed.
    OutputOverCap(usize),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Io(error) => write!(f, "{error}"),
            Failure::TimedOut(after) => {
                write!(f, "still running after {} s, killed", after.as_secs_f64())
            }
            Failure::OutputOverCap(cap) => {
                write!(f, "wrote more than {cap} bytes on stdout, killed")
            }
        }
    }
}

/// Runs `command` with `input` on its stdin, within [`GIT_TIMEOUT`] and
/// [`GIT_MAX_OUTPUT`].
pub fn run(command: Command, input: &[u8]) -> Result<Bounded, Failure> {
    run_within(command, input, GIT_TIMEOUT, GIT_MAX_OUTPUT)
}

enum Event {
    Stdout(io::Result<Vec<u8>>),
    Stderr(Vec<u8>),
}

/// [`run`] with the bounds as parameters, so a test can set them low.
pub fn run_within(
    mut command: Command,
    input: &[u8],
    timeout: Duration,
    cap: usize,
) -> Result<Bounded, Failure> {
    let deadline = Instant::now() + timeout;
    let mut child = command
        .stdin(if input.is_empty() {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(Failure::Io)?;
    let (tx, rx) = mpsc::channel();
    if let Some(mut stdin) = child.stdin.take() {
        let input = input.to_vec();
        // Its own thread: git never waits on a full stdout while this one
        // waits on a full stdin. A write error means git stopped reading.
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    if let Some(stdout) = child.stdout.take() {
        let sender = tx.clone();
        std::thread::spawn(move || {
            let mut kept = Vec::new();
            let read = stdout
                .take(cap as u64 + 1)
                .read_to_end(&mut kept)
                .map(|_| kept);
            let _ = sender.send(Event::Stdout(read));
        });
    }
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut stderr = stderr;
            let mut kept = Vec::new();
            let _ = (&mut stderr)
                .take(STDERR_KEPT as u64)
                .read_to_end(&mut kept);
            let _ = io::copy(&mut stderr, &mut io::sink());
            let _ = tx.send(Event::Stderr(kept));
        });
    }

    let mut stdout = None;
    let mut stderr = None;
    let mut status = None;
    let outcome = loop {
        if let (Some(_), Some(_), Some(_)) = (&stdout, &stderr, &status) {
            break Ok(());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break Err(Failure::TimedOut(timeout));
        }
        match rx.recv_timeout(left.min(EXIT_POLL)) {
            Ok(Event::Stdout(Ok(bytes))) if bytes.len() > cap => {
                break Err(Failure::OutputOverCap(cap));
            }
            Ok(Event::Stdout(read)) => match read {
                Ok(bytes) => stdout = Some(bytes),
                Err(error) => break Err(Failure::Io(error)),
            },
            Ok(Event::Stderr(bytes)) => stderr = Some(bytes),
            Err(_) => {}
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(exited) => status = exited,
                Err(error) => break Err(Failure::Io(error)),
            }
        }
    };
    match outcome {
        Ok(()) => Ok(Bounded {
            status: status.expect("checked by the loop"),
            stdout: stdout.expect("checked by the loop"),
            stderr: stderr.expect("checked by the loop"),
        }),
        Err(failure) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(failure)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn a_command_within_both_bounds_gives_its_status_stdout_and_stderr() {
        let done = run_within(
            sh("cat; echo warn >&2; exit 3"),
            b"fed on stdin\n",
            Duration::from_secs(10),
            1_000,
        )
        .unwrap();
        assert_eq!(done.status.code(), Some(3));
        assert_eq!(done.stdout, b"fed on stdin\n");
        assert_eq!(done.stderr, b"warn\n");
    }

    /// A child still running at the deadline is killed and reported as such,
    /// long before it would have finished.
    #[test]
    fn a_command_past_the_deadline_is_killed_and_reported() {
        let started = Instant::now();
        let failed =
            run_within(sh("sleep 30"), &[], Duration::from_millis(300), 1_000).unwrap_err();
        assert!(
            matches!(failed, Failure::TimedOut(after) if after == Duration::from_millis(300)),
            "{failed:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "killed at the deadline"
        );
    }

    /// Output exactly at the cap passes; one byte more fails and kills a
    /// child that would otherwise write forever.
    #[test]
    fn stdout_over_the_cap_fails_and_stdout_at_the_cap_passes() {
        let at_cap = run_within(sh("printf 1234"), &[], Duration::from_secs(10), 4).unwrap();
        assert_eq!(at_cap.stdout, b"1234");
        let started = Instant::now();
        let failed = run_within(sh("yes"), &[], Duration::from_secs(10), 4_096).unwrap_err();
        assert!(
            matches!(failed, Failure::OutputOverCap(4_096)),
            "{failed:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "killed at the cap"
        );
    }

    /// A chatty stderr past what is kept neither blocks the child nor grows
    /// the message without bound.
    #[test]
    fn stderr_is_kept_up_to_its_cap_and_drained_after() {
        let done = run_within(
            sh("head -c 200000 /dev/zero >&2; echo done"),
            &[],
            Duration::from_secs(10),
            1_000,
        )
        .unwrap();
        assert_eq!(done.stdout, b"done\n");
        assert_eq!(done.stderr.len(), STDERR_KEPT);
    }

    #[test]
    fn a_missing_program_is_an_io_failure() {
        let failed = run_within(
            Command::new("/definitely/not/a/program"),
            &[],
            Duration::from_secs(1),
            10,
        )
        .unwrap_err();
        assert!(matches!(failed, Failure::Io(_)), "{failed:?}");
    }

    #[test]
    fn failures_say_which_bound_they_hit() {
        assert_eq!(
            Failure::TimedOut(Duration::from_millis(1_500)).to_string(),
            "still running after 1.5 s, killed"
        );
        assert_eq!(
            Failure::OutputOverCap(64).to_string(),
            "wrote more than 64 bytes on stdout, killed"
        );
    }
}
