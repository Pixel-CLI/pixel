// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Many blobs through one `git cat-file --batch` process.
//!
//! [`GitRunner::show_blob`] spawns one git per blob, and a caller that also
//! asks the size spawns two. On a tree of about 19 000 files that is some
//! 38 000 processes: 208 s of a laptop for a cold build, most of it system
//! time spent creating processes. One `cat-file --batch` reads every
//! requested object from a single process, streamed as it goes. The spawn
//! itself is [`GitRunner::cat_file_blobs`], in `runner.rs` like every other.
//!
//! [`GitRunner::show_blob`]: crate::GitRunner::show_blob
//! [`GitRunner::cat_file_blobs`]: crate::GitRunner::cat_file_blobs

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use crate::error::GitError;
use crate::redact::redact;
use crate::runner::DEFAULT_MAX_OUTPUT_BYTES;

/// What `git cat-file --batch` answered for one requested object.
#[derive(Debug, PartialEq, Eq)]
pub enum BatchObject<'a> {
    /// A blob no larger than the caller's cap, with its content.
    Blob(&'a [u8]),
    /// A blob over the caller's cap, with the size git reported; its content
    /// was read past, never buffered.
    Oversized(u64),
    /// Git has no such object, the name is ambiguous, or the object is not
    /// a blob.
    Missing,
    /// The spec was never sent: it is empty, or carries a newline, which
    /// the line protocol cannot express. The caller reads it another way.
    Unsendable,
}

/// A parsed `cat-file --batch` header line.
#[derive(Debug, PartialEq, Eq)]
enum Header {
    /// `<oid> <type> <size>`; `blob` is whether the type is `blob`.
    Found { oid: String, blob: bool, size: u64 },
    /// `<spec> missing`, `<spec> ambiguous`, or anything unparseable.
    Missing,
}

/// Parse one header line, without its trailing newline. A found object is
/// `<hex oid> <type> <size>`; anything else is reported as missing: git
/// echoes the requested spec before ` missing`, and a spec may contain
/// spaces, but never reads as a hex oid followed by a type and a size.
fn parse_header(line: &[u8]) -> Header {
    let line = String::from_utf8_lossy(line);
    let mut fields = line.rsplitn(3, ' ');
    let (Some(size), Some(kind), Some(oid)) = (fields.next(), fields.next(), fields.next()) else {
        return Header::Missing;
    };
    let is_oid = !oid.is_empty() && oid.bytes().all(|b| b.is_ascii_hexdigit());
    match size.parse::<u64>() {
        Ok(size) if is_oid => Header::Found {
            oid: oid.to_string(),
            blob: kind == "blob",
            size,
        },
        _ => Header::Missing,
    }
}

/// Whether `spec` can go on one line of `cat-file --batch`'s input. Git
/// strips a carriage return before the newline, so a spec ending in one
/// would be answered for the name without it: another file's content.
fn sendable(spec: &str) -> bool {
    !spec.is_empty() && !spec.contains('\n') && !spec.ends_with('\r')
}

/// Split `specs` into the indexes that can be sent and the request bytes
/// for them, one spec per line, answering every other index
/// [`BatchObject::Unsendable`] through `visit`.
pub(crate) fn requests<F>(specs: &[String], visit: &mut F) -> (Vec<usize>, Vec<u8>)
where
    F: FnMut(usize, BatchObject<'_>),
{
    let (sent, unsendable): (Vec<usize>, Vec<usize>) =
        (0..specs.len()).partition(|&i| sendable(&specs[i]));
    for i in unsendable {
        visit(i, BatchObject::Unsendable);
    }
    let mut input = Vec::new();
    for &i in &sent {
        input.extend_from_slice(specs[i].as_bytes());
        input.push(b'\n');
    }
    (sent, input)
}

/// Read a `cat-file --batch-check` answer, one header line per index of
/// `sent`: a blob of at most `max_blob_bytes` is returned as `(index, oid)`
/// to be read, anything else is answered through `visit` right away
/// ([`BatchObject::Oversized`] or [`BatchObject::Missing`]). Sizes come
/// first because `--batch` inflates and pipes every object it is asked
/// for: an asset over the cap would cost its whole size only to be dropped.
///
/// # Errors
///
/// The answer does not hold exactly one line per sent spec.
pub(crate) fn plan_reads<F>(
    check: &[u8],
    sent: &[usize],
    max_blob_bytes: u64,
    visit: &mut F,
) -> std::io::Result<Vec<(usize, String)>>
where
    F: FnMut(usize, BatchObject<'_>),
{
    let Some(body) = check.strip_suffix(b"\n") else {
        return Err(std::io::ErrorKind::UnexpectedEof.into());
    };
    let lines: Vec<&[u8]> = body.split(|&b| b == b'\n').collect();
    if lines.len() != sent.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "cat-file --batch-check: not one answer per request",
        ));
    }
    let mut reads = Vec::new();
    for (&i, line) in sent.iter().zip(lines) {
        match parse_header(line) {
            Header::Found {
                oid,
                blob: true,
                size,
            } if size <= max_blob_bytes => reads.push((i, oid)),
            Header::Found {
                blob: true, size, ..
            } => visit(i, BatchObject::Oversized(size)),
            _ => visit(i, BatchObject::Missing),
        }
    }
    Ok(reads)
}

/// One answer, owned so the reader thread can hand it to the caller's.
enum Answer {
    Blob(Vec<u8>),
    Oversized(u64),
    Missing,
}

impl Answer {
    fn as_object(&self) -> BatchObject<'_> {
        match self {
            Self::Blob(content) => BatchObject::Blob(content),
            Self::Oversized(size) => BatchObject::Oversized(*size),
            Self::Missing => BatchObject::Missing,
        }
    }
}

/// Read one answer per index of `sent` from `stdout`, handing each to `tx`
/// as soon as it is complete. Stops at the first I/O error, a truncated
/// answer or a content not followed by its newline (a stream no longer
/// aligned on headers), and quietly once the receiver is gone.
fn read_answers(
    stdout: impl Read,
    sent: &[usize],
    max_blob_bytes: u64,
    tx: &mpsc::SyncSender<(usize, Answer)>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    for &i in sent {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 || line.pop() != Some(b'\n') {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        let answer = match parse_header(&line) {
            Header::Found { blob, size, .. } => {
                let answer = if blob && size <= max_blob_bytes {
                    let mut content = Vec::new();
                    (&mut reader).take(size).read_to_end(&mut content)?;
                    if content.len() as u64 != size {
                        return Err(std::io::ErrorKind::UnexpectedEof.into());
                    }
                    Answer::Blob(content)
                } else {
                    let skipped =
                        std::io::copy(&mut (&mut reader).take(size), &mut std::io::sink())?;
                    if skipped != size {
                        return Err(std::io::ErrorKind::UnexpectedEof.into());
                    }
                    if blob {
                        Answer::Oversized(size)
                    } else {
                        Answer::Missing
                    }
                };
                // The content is followed by one newline of its own.
                let mut lf = [0u8; 1];
                reader.read_exact(&mut lf)?;
                if lf != *b"\n" {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "cat-file --batch: no newline after an object's content",
                    ));
                }
                answer
            }
            Header::Missing => Answer::Missing,
        };
        if tx.send((i, answer)).is_err() {
            return Ok(());
        }
    }
    Ok(())
}

/// The protocol behind [`GitRunner::cat_file_blobs`], over any command that
/// speaks it, so the timeout and early-exit paths can be tested against a
/// plain `sh` instead of a contrived git hang.
pub(crate) fn batch_session<F>(
    mut cmd: Command,
    specs: &[String],
    max_blob_bytes: u64,
    idle_timeout: Option<Duration>,
    mut visit: F,
) -> Result<(), GitError>
where
    F: FnMut(usize, BatchObject<'_>),
{
    let args = vec!["cat-file".to_string(), "--batch".to_string()];
    let (sent, input) = requests(specs, &mut visit);
    if sent.is_empty() {
        return Ok(());
    }

    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    // Written from its own thread, and never joined: git answers while it
    // reads, so a request list larger than the pipe buffer would otherwise
    // deadlock against an unread answer, and a git that stops reading
    // leaves the writer blocked until the kill below breaks the pipe.
    std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr
            .take(DEFAULT_MAX_OUTPUT_BYTES as u64)
            .read_to_end(&mut buf);
        buf
    });
    // The answers are read on their own thread and handed over one at a
    // time. The wait for the next one is then the only place the caller
    // blocks on git: the idle timeout bounds it even when a grandchild
    // holds stdout open past git's death, and time spent in `visit` is
    // never counted as git's silence.
    let (tx, rx) = mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || read_answers(stdout, &sent, max_blob_bytes, &tx));
    loop {
        let next = match idle_timeout {
            Some(timeout) => rx.recv_timeout(timeout),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match next {
            Ok((i, answer)) => visit(i, answer.as_object()),
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                let _ = child.kill();
                let _ = child.wait();
                // The reader and stderr threads are left to end on their
                // own, as in `runner.rs`: a grandchild that inherited a pipe
                // can hold it open past the deadline, and joining them would
                // hand it the caller.
                return Err(GitError::Timeout { args });
            }
        }
    }
    let outcome = reader
        .join()
        .unwrap_or_else(|_| Err(std::io::Error::other("batch reader thread panicked")));
    let Some(status) = wait_within(&mut child, idle_timeout)? else {
        // Detached for the same reason as the timeout above.
        return Err(GitError::Timeout { args });
    };
    let stderr = stderr_reader.join().unwrap_or_default();
    if !status.success() {
        return Err(GitError::NonZeroExit {
            args,
            code: status.code(),
            stderr: redact(String::from_utf8_lossy(&stderr).trim()),
        });
    }
    outcome.map_err(GitError::Io)
}

/// Collect `child`'s exit status, or `None` once it has been killed for not
/// exiting within `timeout`: git exits on the end of its input, and one
/// that does not must not hold the caller.
fn wait_within(
    child: &mut Child,
    timeout: Option<Duration>,
) -> Result<Option<std::process::ExitStatus>, GitError> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if timeout.is_some_and(|t| start.elapsed() >= t) {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{GitOptions, GitRunner};
    use std::path::{Path, PathBuf};

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "pixel-git-batch-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 1_000_000
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// The idle timeout of a test whose process is expected to end on its
    /// own: a mutant that stops waiting for the right event then fails in
    /// seconds instead of hanging the suite.
    const TEST_IDLE: Option<Duration> = Some(Duration::from_secs(2));

    /// A runner whose idle timeout a broken batch loop reaches in seconds,
    /// not the production two minutes.
    fn runner(root: impl Into<PathBuf>) -> GitRunner {
        GitRunner::with_options(
            root,
            GitOptions {
                timeout: TEST_IDLE,
                max_output_bytes: None,
            },
        )
    }

    /// A repository holding every shape a batch answer takes, and its HEAD.
    fn fixture(tag: &str) -> (PathBuf, String) {
        let dir = tmpdir(tag);
        git(&dir, &["init", "-q"]);
        std::fs::create_dir_all(dir.join("dir")).unwrap();
        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.join("with space.rs"), "fn s(){}\n").unwrap();
        std::fs::write(dir.join("empty.txt"), "").unwrap();
        std::fs::write(dir.join("ten.txt"), "0123456789").unwrap();
        std::fs::write(dir.join("eleven.txt"), "0123456789A").unwrap();
        std::fs::write(dir.join("dir/b.rs"), "fn b() {}\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "one"]);
        let head = git(&dir, &["rev-parse", "HEAD"]);
        (dir, head)
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Owned {
        Blob(Vec<u8>),
        Oversized(u64),
        Missing,
        Unsendable,
    }

    fn collect(runner: &GitRunner, specs: &[String], max: u64) -> Vec<(usize, Owned)> {
        let mut seen = Vec::new();
        runner
            .cat_file_blobs(specs, max, |i, obj| {
                seen.push((
                    i,
                    match obj {
                        BatchObject::Blob(b) => Owned::Blob(b.to_vec()),
                        BatchObject::Oversized(n) => Owned::Oversized(n),
                        BatchObject::Missing => Owned::Missing,
                        BatchObject::Unsendable => Owned::Unsendable,
                    },
                ));
            })
            .unwrap();
        // Answers come in no particular order: sorted, the list shows every
        // index answered exactly once.
        seen.sort_by_key(|(i, _)| *i);
        seen
    }

    /// Every spec is answered once, on its own index: content for a blob
    /// under the cap (the edge included), the size of one over it, missing
    /// for a tree or an absent path, with the blobs around them intact.
    #[test]
    fn each_spec_gets_its_own_answer_and_skipped_content_keeps_the_stream_aligned() {
        let (dir, head) = fixture("answers");
        let specs: Vec<String> = [
            "a.rs",
            "dir",
            "with space.rs",
            "ghost.rs",
            "eleven.txt",
            "empty.txt",
            "ten.txt",
            "dir/b.rs",
        ]
        .iter()
        .map(|p| format!("{head}:{p}"))
        .collect();
        let seen = collect(&runner(&dir), &specs, 10);
        assert_eq!(
            seen,
            vec![
                (0, Owned::Blob(b"fn a() {}\n".to_vec())),
                (1, Owned::Missing),
                (2, Owned::Blob(b"fn s(){}\n".to_vec())),
                (3, Owned::Missing),
                (4, Owned::Oversized(11)),
                (5, Owned::Blob(Vec::new())),
                (6, Owned::Blob(b"0123456789".to_vec())),
                (7, Owned::Blob(b"fn b() {}\n".to_vec())),
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The stream itself, below the size check: a tree and an oversized blob
    /// asked of `--batch` directly are read past to the byte, so every blob
    /// after them still reads as itself (the check keeps them away from it
    /// in production; the stream must not depend on that to stay aligned).
    #[test]
    fn the_stream_reads_past_trees_and_oversized_blobs_and_stays_aligned() {
        let (dir, head) = fixture("stream");
        let specs: Vec<String> = ["a.rs", "dir", "eleven.txt", "ten.txt", "dir/b.rs"]
            .iter()
            .map(|p| format!("{head}:{p}"))
            .collect();
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&dir).args(["cat-file", "--batch"]);
        let mut seen = Vec::new();
        batch_session(cmd, &specs, 10, TEST_IDLE, |i, obj| {
            seen.push(format!("{i} {obj:?}"));
        })
        .unwrap();
        assert_eq!(
            seen,
            [
                format!("0 {:?}", BatchObject::Blob(b"fn a() {}\n")),
                "1 Missing".to_string(),
                "2 Oversized(11)".to_string(),
                format!("3 {:?}", BatchObject::Blob(b"0123456789")),
                format!("4 {:?}", BatchObject::Blob(b"fn b() {}\n")),
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A spec the line protocol cannot carry is reported unsendable without
    /// shifting the others, and a list of only such specs spawns nothing
    /// (the runner's root does not even exist). A trailing carriage return
    /// is one: git strips it and would answer `a.rs` for `a.rs\r`.
    #[test]
    fn unsendable_specs_are_reported_and_do_not_shift_the_rest() {
        let (dir, head) = fixture("unsendable");
        let specs = vec![
            format!("{head}:a\nb.rs"),
            format!("{head}:a.rs"),
            String::new(),
            format!("{head}:a.rs\r"),
        ];
        let seen = collect(&runner(&dir), &specs, 1024);
        assert_eq!(
            seen,
            vec![
                (0, Owned::Unsendable),
                (1, Owned::Blob(b"fn a() {}\n".to_vec())),
                (2, Owned::Unsendable),
                (3, Owned::Unsendable),
            ]
        );
        let nowhere = runner(dir.join("no-such-dir"));
        assert_eq!(
            collect(&nowhere, &[String::new()], 1024),
            vec![(0, Owned::Unsendable)]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn outside_a_repository_is_an_error_not_a_list_of_missing_blobs() {
        let dir = tmpdir("not-a-repo");
        let mut visited = 0;
        let result = runner(&dir).cat_file_blobs(&["HEAD:a.rs".to_string()], 1024, |_, _| {
            visited += 1;
        });
        assert!(
            matches!(result, Err(GitError::NonZeroExit { .. })),
            "{result:?}"
        );
        assert_eq!(visited, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A process that stops answering is killed at the idle timeout, well
    /// before it would have exited on its own.
    #[test]
    fn a_silent_process_is_killed_at_the_idle_timeout() {
        let mut cmd = Command::new("sleep");
        cmd.arg("6");
        let start = Instant::now();
        let result = batch_session(
            cmd,
            &["x".to_string()],
            1024,
            Some(Duration::from_millis(200)),
            |_, _| {},
        );
        assert!(
            matches!(result, Err(GitError::Timeout { .. })),
            "{result:?}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{:?}",
            start.elapsed()
        );
    }

    /// Progress resets the idle clock: answers spaced under the timeout
    /// complete even when the whole batch takes longer than it.
    #[test]
    fn the_timeout_is_idle_time_not_total_time() {
        let mut cmd = Command::new("sh");
        let oid = "a".repeat(40);
        cmd.args([
            "-c",
            &format!("for i in 1 2 3 4; do read x; sleep 0.3; printf '{oid} blob 1\\nz\\n'; done"),
        ]);
        let specs: Vec<String> = (0..4).map(|i| i.to_string()).collect();
        let mut blobs = 0;
        let result = batch_session(
            cmd,
            &specs,
            1024,
            Some(Duration::from_millis(1_000)),
            |_, obj| {
                assert_eq!(obj, BatchObject::Blob(b"z"));
                blobs += 1;
            },
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(blobs, 4);
    }

    /// A process that ends before answering everything reports its exit,
    /// with its stderr; one that exits 0 early is a truncated answer.
    #[test]
    fn an_early_exit_is_an_error_carrying_the_exit_code_and_stderr() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "cat >/dev/null; echo boom >&2; exit 3"]);
        let result = batch_session(cmd, &["x".to_string()], 1024, TEST_IDLE, |_, _| {});
        match result {
            Err(GitError::NonZeroExit { code, stderr, .. }) => {
                assert_eq!(code, Some(3));
                assert_eq!(stderr, "boom");
            }
            other => panic!("expected NonZeroExit, got {other:?}"),
        }
        let mut cmd = Command::new("sh");
        let oid = "b".repeat(40);
        cmd.args(["-c", &format!("cat >/dev/null; printf '{oid} blob 5\\nab'")]);
        let result = batch_session(cmd, &["x".to_string()], 1024, TEST_IDLE, |_, _| {});
        assert!(
            matches!(&result, Err(GitError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof),
            "{result:?}"
        );
        // The same truncation on a blob read past (over the cap).
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            &format!("cat >/dev/null; printf '{oid} blob 5000\\nab'"),
        ]);
        let result = batch_session(cmd, &["x".to_string()], 1024, TEST_IDLE, |_, _| {});
        assert!(
            matches!(&result, Err(GitError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof),
            "{result:?}"
        );
    }

    /// A process that takes a moment to exit after its last answer is
    /// waited for, not killed: only the timeout ends it.
    #[test]
    fn a_process_exiting_shortly_after_its_answers_succeeds() {
        let mut cmd = Command::new("sh");
        let oid = "d".repeat(40);
        cmd.args([
            "-c",
            &format!("read x; printf '{oid} blob 1\\nz\\n'; sleep 0.3; exit 0"),
        ]);
        let mut blobs = 0;
        let result = batch_session(
            cmd,
            &["x".to_string()],
            1024,
            Some(Duration::from_secs(3)),
            |_, _| blobs += 1,
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(blobs, 1);
    }

    /// The idle clock stops while `visit` runs: a consumer slower than the
    /// timeout on every answer still gets them all.
    #[test]
    fn a_slow_visit_is_not_counted_as_git_being_idle() {
        let mut cmd = Command::new("sh");
        let oid = "e".repeat(40);
        cmd.args([
            "-c",
            &format!("printf '{oid} blob 1\\nz\\n{oid} blob 1\\nz\\n'; cat >/dev/null"),
        ]);
        let mut blobs = 0;
        let result = batch_session(
            cmd,
            &["x".to_string(), "y".to_string()],
            1024,
            Some(Duration::from_millis(200)),
            |_, _| {
                std::thread::sleep(Duration::from_millis(500));
                blobs += 1;
            },
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(blobs, 2);
    }

    /// A process that closes its stdout without reading its stdin leaves the
    /// request writer blocked on a full pipe; the idle timeout still ends it
    /// instead of the caller waiting for the process to exit on its own.
    #[test]
    fn a_writer_blocked_on_an_unread_stdin_is_released_by_the_timeout() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exec 1>&-; sleep 6"]);
        // Far past a pipe buffer (64 KiB on Linux and macOS).
        let specs: Vec<String> = (0..4_000).map(|i| format!("{i:0>100}")).collect();
        let start = Instant::now();
        let result = batch_session(
            cmd,
            &specs,
            1024,
            Some(Duration::from_millis(300)),
            |_, _| {},
        );
        assert!(
            matches!(result, Err(GitError::Timeout { .. })),
            "{result:?}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{:?}",
            start.elapsed()
        );
    }

    /// A process that answers everything and then does not exit is killed
    /// once the timeout passes, instead of holding the caller in `wait`.
    #[test]
    fn a_process_lingering_after_its_answers_is_killed() {
        let mut cmd = Command::new("sh");
        let oid = "c".repeat(40);
        cmd.args([
            "-c",
            &format!("read x; printf '{oid} blob 1\\nz\\n'; exec sleep 10"),
        ]);
        let start = Instant::now();
        let mut blobs = 0;
        let result = batch_session(
            cmd,
            &["x".to_string()],
            1024,
            Some(Duration::from_millis(300)),
            |_, _| blobs += 1,
        );
        assert!(
            matches!(result, Err(GitError::Timeout { .. })),
            "{result:?}"
        );
        assert_eq!(blobs, 1);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
    }

    /// A grandchild that inherited stdout keeps the pipe open after git is
    /// killed; the caller is still released at the idle timeout instead of
    /// reading until the grandchild exits.
    #[test]
    fn a_grandchild_holding_stdout_does_not_outlive_the_timeout() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 5 & exec sleep 5"]);
        let start = Instant::now();
        let result = batch_session(
            cmd,
            &["x".to_string()],
            1024,
            Some(Duration::from_millis(200)),
            |_, _| {},
        );
        assert!(
            matches!(result, Err(GitError::Timeout { .. })),
            "{result:?}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{:?}",
            start.elapsed()
        );
    }

    /// Content not followed by its newline means the stream is no longer
    /// aligned on headers: the batch fails instead of reading the next
    /// content as a header and answering the following specs from it.
    #[test]
    fn content_without_its_newline_is_an_error_not_a_shifted_stream() {
        let mut cmd = Command::new("sh");
        let oid = "f".repeat(40);
        cmd.args([
            "-c",
            &format!("cat >/dev/null; printf '{oid} blob 1\\nzX{oid} blob 1\\nz\\n'"),
        ]);
        let mut seen = Vec::new();
        let result = batch_session(
            cmd,
            &["x".to_string(), "y".to_string()],
            1024,
            TEST_IDLE,
            |i, _| seen.push(i),
        );
        assert!(
            matches!(&result, Err(GitError::Io(e)) if e.kind() == std::io::ErrorKind::InvalidData),
            "{result:?}"
        );
        assert_eq!(seen, Vec::<usize>::new());
    }

    /// The size check sends only the blobs under the cap to be read, by the
    /// oid it resolved, and answers the others itself: an oversized blob is
    /// never asked of `--batch`, so git never inflates it.
    #[test]
    fn plan_reads_keeps_only_blobs_under_the_cap_for_the_content_pass() {
        let small = "1".repeat(40);
        let edge = "2".repeat(40);
        let big = "3".repeat(40);
        let tree = "4".repeat(40);
        let check = [
            format!("{small} blob 3"),
            format!("{edge} blob 10"),
            format!("{big} blob 11"),
            format!("{tree} tree 30"),
            "HEAD:ghost.rs missing".to_string(),
            String::new(),
        ]
        .join("\n");
        let mut answered = Vec::new();
        let reads = plan_reads(check.as_bytes(), &[5, 6, 7, 8, 9], 10, &mut |i, obj| {
            answered.push(format!("{i} {obj:?}"));
        })
        .unwrap();
        assert_eq!(reads, vec![(5, small), (6, edge)]);
        assert_eq!(answered, ["7 Oversized(11)", "8 Missing", "9 Missing"]);
    }

    /// A check that does not answer every request, or ends mid-line, is an
    /// error: pairing its lines with the requests would shift every answer.
    #[test]
    fn plan_reads_refuses_a_check_that_does_not_answer_every_request() {
        let oid = "5".repeat(40);
        let mut visits = 0;
        let mut visit = |_: usize, _: BatchObject<'_>| visits += 1;
        let one = format!("{oid} blob 3\n");
        let short = plan_reads(one.as_bytes(), &[0, 1], 10, &mut visit);
        assert!(
            matches!(&short, Err(e) if e.kind() == std::io::ErrorKind::InvalidData),
            "{short:?}"
        );
        let long = plan_reads(format!("{one}{one}").as_bytes(), &[0], 10, &mut visit);
        assert!(
            matches!(&long, Err(e) if e.kind() == std::io::ErrorKind::InvalidData),
            "{long:?}"
        );
        let cut = plan_reads(format!("{oid} blob 3").as_bytes(), &[0], 10, &mut visit);
        assert!(
            matches!(&cut, Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof),
            "{cut:?}"
        );
        assert_eq!(visits, 0);
    }

    #[test]
    fn a_header_is_found_only_as_hex_oid_type_and_size() {
        let oid = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            parse_header(format!("{oid} blob 12").as_bytes()),
            Header::Found {
                oid: oid.to_string(),
                blob: true,
                size: 12
            }
        );
        assert_eq!(
            parse_header(format!("{oid} tree 30").as_bytes()),
            Header::Found {
                oid: oid.to_string(),
                blob: false,
                size: 30
            }
        );
        assert_eq!(
            parse_header(format!("{oid}:with space blob 3 missing").as_bytes()),
            Header::Missing
        );
        assert_eq!(
            parse_header(format!("{oid}:x.rs blob 3").as_bytes()),
            Header::Missing
        );
        assert_eq!(parse_header(b"HEAD ambiguous"), Header::Missing);
        assert_eq!(
            parse_header(format!("{oid} blob").as_bytes()),
            Header::Missing
        );
        assert_eq!(parse_header(b" blob 3"), Header::Missing);
    }
}
