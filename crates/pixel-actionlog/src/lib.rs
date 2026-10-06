// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Persistent async action log — a durable, append-only JSONL record of what
//! pixel itself did on each invocation (command, outcome, error, duration),
//! so a session can be self-assessed later without re-deriving it from
//! memory or shell scrollback.
//!
//! Writes go through an unbounded channel to a dedicated background thread:
//! `log()` never blocks the caller on disk I/O. `ActionLog::finish()` gives
//! the writer thread a bounded window (`SHUTDOWN_FLUSH_TIMEOUT`) to drain
//! before the CLI exits, so a slow disk can never hang `pixel`'s exit — a
//! late line is simply lost rather than blocking, which fits an
//! observability log (not a correctness-critical journal).

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

mod metrics;
mod serve;
pub use metrics::{
    ComparisonGap, OperationMetrics, WorkflowEvidence, WorkflowTimeEstimate, format_metrics_line,
    read_tokens, saved_percent, summarize_metrics,
};
pub use serve::{InProcessReason, ServeRoute, ServeStep};

pub const LOG_FILE_NAME: &str = "actions.jsonl";
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
const MAX_KEPT_LINES: usize = 5000;
const ROTATE_CHECK_EVERY: u32 = 50;
/// Upper bound `finish_flush` waits for the writer to drain — kept for the
/// durability callers (tests, anything that reads the log right after).
/// `finish` itself never waits.
const SHUTDOWN_FLUSH_TIMEOUT: Duration = Duration::from_millis(300);
const MAX_ARGS_LEN: usize = 4000;
const MAX_ERROR_LEN: usize = 2000;

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    Error,
}

/// One recorded pixel invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionEvent {
    /// Correlates one invocation, never a global latest-operation pointer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation_id: Option<String>,
    /// Optional task correlation; this best-effort log is never completion evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskCorrelation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<OperationMetrics>,
    pub ts_ms: i64,
    pub pid: u32,
    pub command: String,
    pub args: String,
    pub cwd: String,
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub duration_ms: u64,
    /// Chars of source actually RETURNED to the agent (snippets/context),
    /// when the command is retrieval-shaped and records a snippet cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet_cap_chars: Option<u64>,
    /// Chars of the retrieval pool/corpus the command COULD have returned but
    /// didn't (raw file bytes of the matched files). Together with
    /// `snippet_cap_chars` this lets a `pixel savings` command compute a real
    /// token-reduction ratio instead of a marketing claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool_chars: Option<u64>,
    /// How each daemon-or-in-process request of the invocation was served,
    /// in order, with its phase timings: what `duration_ms` alone cannot say.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub serve: Vec<ServeStep>,
}

/// Links an invocation to the durable task ledger without changing legacy records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskCorrelation {
    pub task_id: String,
    pub attempt_id: String,
    pub span_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_call_id: Option<String>,
}

impl ActionEvent {
    pub fn new(command: impl Into<String>, args: impl Into<String>) -> Self {
        ActionEvent {
            invocation_id: Some(metrics::invocation_id()),
            task: None,
            metrics: None,
            ts_ms: now_ms(),
            pid: std::process::id(),
            command: command.into(),
            args: truncate(&args.into(), MAX_ARGS_LEN),
            cwd: std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            outcome: Outcome::Ok,
            error: None,
            duration_ms: 0,
            snippet_cap_chars: None,
            pool_chars: None,
            serve: Vec::new(),
        }
    }

    /// Attach explicit task IDs supplied by the caller, without reading ambient state.
    pub fn with_task(mut self, task: TaskCorrelation) -> Self {
        self.task = Some(task);
        self
    }

    pub fn with_result(mut self, result: &Result<(), String>, duration: Duration) -> Self {
        self.duration_ms = duration.as_millis() as u64;
        match result {
            Ok(()) => {
                self.outcome = Outcome::Ok;
                self.error = None;
            }
            Err(e) => {
                self.outcome = Outcome::Error;
                self.error = Some(truncate(e, MAX_ERROR_LEN));
            }
        }
        self
    }

    /// Attach retrieval volume so a token-savings metric can be derived.
    /// `snippet` = chars actually returned; `pool` = chars the caller would
    /// have had to read without retrieval. Returns `self` for chaining.
    pub fn with_savings(mut self, snippet_cap_chars: u64, pool_chars: u64) -> Self {
        self.snippet_cap_chars = Some(snippet_cap_chars);
        self.pool_chars = Some(pool_chars);
        self
    }

    /// Fraction of the retrieval pool the agent did NOT have to read,
    /// i.e. token savings = 1 − snippet/pool. Returns `None` when either
    /// volume is missing (command was not retrieval-shaped) or pool is 0
    /// (no pool to save against).
    pub fn savings_ratio(&self) -> Option<f64> {
        let snippet = self.snippet_cap_chars?;
        let pool = self.pool_chars?;
        if pool == 0 {
            return None;
        }
        let ratio = 1.0 - (snippet as f64 / pool as f64);
        Some(ratio.clamp(0.0, 1.0))
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}… (truncated)", &s[..cut])
}

/// Async, best-effort, persistent action log. Every constructor path returns
/// a usable handle — setup failures (unwritable dir, etc.) degrade to a
/// no-op logger rather than surfacing an error, since observability must
/// never block or fail the command it is observing.
pub struct ActionLog {
    tx: Option<Sender<ActionEvent>>,
    done_rx: Option<mpsc::Receiver<()>>,
    handle: Option<JoinHandle<()>>,
}

impl ActionLog {
    /// The action log path for a given repo/project root: `<root>/.pixel/actions.jsonl`.
    pub fn path_for_root(root: &Path) -> PathBuf {
        pixel_git::sidecar::dir(root).join(LOG_FILE_NAME)
    }

    /// Spawn the background writer for `<root>/.pixel/actions.jsonl`,
    /// creating the owner-only directory if needed. Never fails outwardly:
    /// on any setup error, a `.pixel` that is a link included, returns a
    /// no-op logger.
    pub fn spawn_for_root(root: &Path) -> ActionLog {
        if pixel_git::sidecar::private_dir(&pixel_git::sidecar::dir(root)).is_err() {
            return ActionLog::noop();
        }
        Self::spawn_at(Self::path_for_root(root))
    }

    /// Spawn the background writer for an explicit log file path.
    pub fn spawn_at(path: PathBuf) -> ActionLog {
        let (tx, rx) = mpsc::channel::<ActionEvent>();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let handle = std::thread::Builder::new()
            .name("pixel-actionlog".to_string())
            .spawn(move || writer_loop(path, rx, done_tx))
            .ok();
        if handle.is_none() {
            return ActionLog::noop();
        }
        ActionLog {
            tx: Some(tx),
            done_rx: Some(done_rx),
            handle,
        }
    }

    /// A logger that discards every event. Used when logging cannot be set
    /// up; callers never need to branch on availability.
    pub fn noop() -> ActionLog {
        ActionLog {
            tx: None,
            done_rx: None,
            handle: None,
        }
    }

    /// Enqueue an event. Never blocks: the channel is unbounded and a full
    /// or torn-down receiver is silently ignored.
    pub fn log(&self, event: ActionEvent) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(event);
        }
    }

    /// Close the channel and return WITHOUT waiting for the writer to
    /// drain: fire-and-forget. The writer thread keeps running and still
    /// flushes whatever it manages before process teardown; a line that
    /// arrives after exit is simply lost — the documented contract of this
    /// observability log, and the reason CLI exit no longer pays up to
    /// `SHUTDOWN_FLUSH_TIMEOUT` on a slow disk.
    ///
    /// Callers that must observe the events on disk (tests, anything that
    /// reads the log after finishing) use [`Self::finish_flush`], which
    /// keeps the old bounded-wait behavior.
    pub fn finish(&mut self) {
        self.tx.take(); // drop the sender: closes the channel
        // Do not wait on done_rx and do not join: the point of this path
        // is that exit cost is zero. The channel close still orders every
        // already-queued event ahead of the writer's own drain.
        self.done_rx.take();
        self.handle.take();
    }

    /// [`Self::finish`] plus a bounded wait (≤ `SHUTDOWN_FLUSH_TIMEOUT`)
    /// for the writer to drain and flush — the old `finish` behavior, for
    /// callers that need the events durably on disk before returning.
    pub fn finish_flush(&mut self) {
        self.tx.take(); // drop the sender: closes the channel
        if let Some(done_rx) = self.done_rx.take() {
            let _ = done_rx.recv_timeout(SHUTDOWN_FLUSH_TIMEOUT);
        }
        // Deliberately do not join(): a stuck disk must never hang exit.
        // The process terminating will tear down the writer thread anyway.
        self.handle.take();
    }
}

impl Drop for ActionLog {
    fn drop(&mut self) {
        self.finish();
    }
}

fn writer_loop(path: PathBuf, rx: mpsc::Receiver<ActionEvent>, done_tx: Sender<()>) {
    let mut file = open_log_file(&path);
    let mut since_rotate_check: u32 = 0;
    while let Ok(event) = rx.recv() {
        if let Some(f) = file.as_mut()
            && let Ok(line) = serde_json::to_string(&event)
        {
            // Encode the complete line before appending: formatter writes can
            // interleave across concurrent CLI invocations.
            let mut bytes = line.into_bytes();
            bytes.push(b'\n');
            let _ = f.write_all(&bytes);
            let _ = f.flush();
        }
        since_rotate_check += 1;
        if since_rotate_check >= ROTATE_CHECK_EVERY {
            since_rotate_check = 0;
            let _ = rotate_if_needed(&path);
            file = open_log_file(&path);
        }
    }
    let _ = done_tx.send(());
}

/// Open the log file for appending, creating it with 0600 permissions if
/// it doesn't exist yet. The action log may contain fill values (passwords,
/// OTPs) from flow replay, so it must not be world-readable. A link at the
/// log's name is refused, never written or chmodded through: a repository
/// can commit one under `.pixel/`.
fn open_log_file(path: &Path) -> Option<File> {
    pixel_git::nofollow::open_append(path).ok()
}

/// Bound memory on pathological inputs: once `lines` holds more than twice
/// `keep`, drop all but its last `keep` entries.
fn bound_to_tail(lines: &mut Vec<String>, keep: usize) {
    if lines.len() > keep * 2 {
        lines.drain(0..lines.len() - keep);
    }
}

/// If the log has grown past `MAX_LOG_BYTES`, rewrite it keeping only the
/// most recent `MAX_KEPT_LINES` lines. Best-effort: any failure just leaves
/// the file as-is (an unbounded log is still preferable to losing the file).
fn rotate_if_needed(path: &Path) -> std::io::Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(()),
    };
    if meta.len() <= MAX_LOG_BYTES {
        return Ok(());
    }
    let file = pixel_git::nofollow::open_read(path)?;
    let reader = BufReader::new(file);
    let mut lines: Vec<String> = Vec::new();
    for line in reader.lines() {
        lines.push(line?);
        bound_to_tail(&mut lines, MAX_KEPT_LINES);
    }
    let start = lines.len().saturating_sub(MAX_KEPT_LINES);
    let mut kept = Vec::new();
    for line in &lines[start..] {
        writeln!(kept, "{line}")?;
    }
    pixel_git::nofollow::write_replace(path, &kept, pixel_git::nofollow::PRIVATE_MODE)
}

/// Read back the last `limit` events from the log at `path` (oldest first
/// within the returned window), for `pixel log`. Malformed lines are
/// skipped rather than failing the whole read.
pub fn tail(path: &Path, limit: usize) -> std::io::Result<Vec<ActionEvent>> {
    let file = match pixel_git::nofollow::open_read(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let reader = BufReader::new(file);
    let mut ring: std::collections::VecDeque<ActionEvent> =
        std::collections::VecDeque::with_capacity(limit.min(4096));
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<ActionEvent>(&line) {
            ring.push_back(event);
            // Drop the oldest once the ring holds more than `limit`, so a
            // `limit` of 0 keeps nothing.
            if ring.len() > limit {
                ring.pop_front();
            }
        }
    }
    Ok(ring.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `ActionEvent::new` stamps `ts_ms` from here and readers of
    /// `actions.jsonl` compare it with wall-clock time; a placeholder
    /// (0, -1, a constant) would date every event to 1970.
    #[test]
    fn now_ms_is_the_current_unix_epoch_in_milliseconds() {
        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after 1970")
            .as_millis() as i64;
        let ts = now_ms();
        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after 1970")
            .as_millis() as i64;
        assert!(ts >= before && ts <= after, "{before} <= {ts} <= {after}");
        // 2020-01-01T00:00:00Z: a real clock is past it, a placeholder is not.
        assert!(ts > 1_577_836_800_000, "{ts}");
    }
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn log_then_finish_persists_events() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("actions.jsonl");
        let mut log = ActionLog::spawn_at(path.clone());
        log.log(ActionEvent::new("search", "pattern=foo"));
        log.log(
            ActionEvent::new("rescue", "problem=bar")
                .with_result(&Err("boom".to_string()), Duration::from_millis(12)),
        );
        log.finish_flush();

        let events = tail(&path, 10).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].command, "search");
        assert_eq!(events[0].outcome, Outcome::Ok);
        assert_eq!(events[1].command, "rescue");
        assert_eq!(events[1].outcome, Outcome::Error);
        assert_eq!(events[1].error.as_deref(), Some("boom"));
        assert_eq!(events[1].duration_ms, 12);
    }

    #[test]
    fn spawn_for_root_creates_pixel_dir_and_is_readable_by_path_for_root() {
        let dir = tempdir().unwrap();
        let mut log = ActionLog::spawn_for_root(dir.path());
        log.log(ActionEvent::new("targets", "task=x"));
        log.finish_flush();

        let path = ActionLog::path_for_root(dir.path());
        assert!(path.exists());
        let events = tail(&path, 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].command, "targets");
    }

    #[test]
    fn noop_logger_never_writes_and_never_blocks() {
        let mut log = ActionLog::noop();
        log.log(ActionEvent::new("search", "x"));
        log.finish();
        // No assertion beyond "this returns" — the point is it can't panic
        // or hang when there is nowhere to write.
    }

    #[test]
    fn tail_respects_limit_and_keeps_most_recent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("actions.jsonl");
        let mut log = ActionLog::spawn_at(path.clone());
        for i in 0..5 {
            log.log(ActionEvent::new("search", format!("n={i}")));
        }
        log.finish_flush();

        let events = tail(&path, 2).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].args, "n=3");
        assert_eq!(events[1].args, "n=4");
    }

    /// `tail(path, 0)` asks for no event: it used to return the whole log,
    /// because the ring only dropped its oldest entry when its length equalled
    /// the limit, which a non-empty ring never does for 0 (#788).
    #[test]
    fn tail_with_a_zero_limit_returns_no_event() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("actions.jsonl");
        let mut log = ActionLog::spawn_at(path.clone());
        for i in 0..3 {
            log.log(ActionEvent::new("search", format!("n={i}")));
        }
        log.finish_flush();

        assert_eq!(tail(&path, 3).unwrap().len(), 3, "the log holds 3 events");
        assert!(tail(&path, 0).unwrap().is_empty());
    }

    #[test]
    fn tail_skips_malformed_lines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("actions.jsonl");
        fs::write(&path, "not json\n{\"bad\":true}\n").unwrap();
        let events = tail(&path, 10).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn savings_ratio_computes_token_reduction_from_captured_volumes() {
        // 1000-char pool, 200 chars actually returned → saved 80%.
        let ev = ActionEvent::new("search", "x").with_savings(200, 1000);
        assert_eq!(ev.snippet_cap_chars, Some(200));
        assert_eq!(ev.pool_chars, Some(1000));
        assert_eq!(ev.savings_ratio(), Some(0.8));

        // No retrieval volumes → not computable, not a claim.
        let plain = ActionEvent::new("publish", "x");
        assert_eq!(plain.savings_ratio(), None);

        // Full-pool return → zero savings (snippet == pool).
        let full = ActionEvent::new("search", "x").with_savings(500, 500);
        assert_eq!(full.savings_ratio(), Some(0.0));

        // Zero pool guards division.
        let zero = ActionEvent::new("search", "x").with_savings(0, 0);
        assert_eq!(zero.savings_ratio(), None);
    }

    #[test]
    fn old_records_without_savings_fields_still_parse() {
        // Backward compatibility: a pre-savings-schema line (no snippet/
        // pool fields) must deserialize to an ActionEvent with None volumes.
        let line = "{\"ts_ms\":1,\"pid\":2,\"command\":\"search\",\"args\":\"x\",\"cwd\":\"/tmp\",\
             \"outcome\":\"ok\",\"duration_ms\":3}";
        let ev: ActionEvent = serde_json::from_str(line).unwrap();
        assert_eq!(ev.command, "search");
        assert_eq!(ev.task, None);
        assert_eq!(ev.snippet_cap_chars, None);
        assert_eq!(ev.pool_chars, None);
        assert_eq!(ev.savings_ratio(), None);
        assert!(ev.serve.is_empty());
    }

    #[test]
    fn task_correlation_should_survive_the_writer_without_changing_legacy_rows() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("actions.jsonl");
        let task = TaskCorrelation {
            task_id: "task-1".into(),
            attempt_id: "attempt-2".into(),
            span_id: "span-3".into(),
            parent_span_id: Some("span-parent".into()),
            host_call_id: Some("host-call".into()),
        };
        let mut log = ActionLog::spawn_at(path.clone());
        log.log(ActionEvent::new("impact", "symbol").with_task(task.clone()));
        log.log(ActionEvent::new("status", "."));
        log.finish_flush();
        let events = tail(&path, 5).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].task, Some(task));
        assert_eq!(events[1].task, None);
        let raw = fs::read_to_string(path).unwrap();
        assert!(!raw.lines().nth(1).unwrap().contains("\"task\""));
    }

    /// The serve steps are what tells a slow cold start from a slow query;
    /// they must survive the writer and `tail` in order.
    #[test]
    fn serve_steps_round_trip_through_the_log() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("actions.jsonl");
        let mut started = ServeStep::new(ServeRoute::DaemonStarted);
        started.start_ms = Some(4_200);
        let mut in_process = ServeStep::in_process(InProcessReason::NoDaemon);
        in_process.open_ms = Some(900);
        in_process.handle_ms = Some(15);
        let mut event = ActionEvent::new("ready", "x");
        event.serve = vec![started.clone(), in_process.clone()];
        let mut log = ActionLog::spawn_at(path.clone());
        log.log(event);
        log.log(ActionEvent::new("status", "y"));
        log.finish_flush();

        let events = tail(&path, 10).unwrap();
        assert_eq!(events[0].serve, vec![started, in_process]);
        assert!(events[1].serve.is_empty());
        let raw = fs::read_to_string(&path).unwrap();
        let second = raw.lines().nth(1).unwrap();
        assert!(
            !second.contains("\"serve\""),
            "empty list omitted: {second}"
        );
    }

    #[test]
    fn rotate_if_needed_keeps_tail_when_over_budget() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("actions.jsonl");
        {
            let mut f = File::create(&path).unwrap();
            for i in 0..(MAX_KEPT_LINES + 500) {
                let ev = ActionEvent::new("search", format!("n={i}"));
                writeln!(f, "{}", serde_json::to_string(&ev).unwrap()).unwrap();
            }
        }
        // Force rotation regardless of actual byte size for a fast test by
        // shrinking the effective threshold via a tiny file substitute is not
        // possible (const), so just call rotate directly and check behavior
        // matches "no-op under budget" here, and exercise the over-budget
        // path in the size-based test below.
        let before = fs::metadata(&path).unwrap().len();
        rotate_if_needed(&path).unwrap();
        let after = fs::metadata(&path).unwrap().len();
        if before > MAX_LOG_BYTES {
            assert!(after <= before);
            let events = tail(&path, usize::MAX).unwrap();
            assert!(events.len() <= MAX_KEPT_LINES);
            assert_eq!(
                events.last().unwrap().args,
                format!("n={}", MAX_KEPT_LINES + 499)
            );
        }
    }

    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
    }

    /// A file outside the repository, `0644`, that a committed link targets.
    fn sentinel(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("victim.txt");
        fs::write(&path, "victim content\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        path
    }

    #[test]
    fn writer_should_leave_the_target_of_a_linked_log_untouched() {
        let base = tempdir().unwrap();
        let victim = sentinel(base.path());
        let root = base.path().join("repo");
        fs::create_dir_all(root.join(".pixel")).unwrap();
        std::os::unix::fs::symlink(&victim, ActionLog::path_for_root(&root)).unwrap();

        let mut log = ActionLog::spawn_for_root(&root);
        log.log(ActionEvent::new("search", "pattern=main"));
        log.finish_flush();

        assert_eq!(fs::read_to_string(&victim).unwrap(), "victim content\n");
        assert_eq!(mode_of(&victim), 0o644);
    }

    #[test]
    fn spawn_for_root_should_neither_write_nor_chmod_through_a_linked_pixel_dir() {
        use std::os::unix::fs::PermissionsExt;
        let base = tempdir().unwrap();
        let elsewhere = base.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o755)).unwrap();
        let root = base.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join(".pixel")).unwrap();

        let mut log = ActionLog::spawn_for_root(&root);
        log.log(ActionEvent::new("search", "pattern=main"));
        log.finish_flush();

        assert_eq!(mode_of(&elsewhere), 0o755);
        assert!(fs::read_dir(&elsewhere).unwrap().next().is_none());
    }

    #[test]
    fn rotate_if_needed_should_keep_the_newest_lines_in_an_owner_only_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("actions.jsonl");
        let padding = "x".repeat(1024);
        {
            let mut f = File::create(&path).unwrap();
            for i in 0..(MAX_KEPT_LINES + 1500) {
                let ev = ActionEvent::new("search", format!("n={i} {padding}"));
                writeln!(f, "{}", serde_json::to_string(&ev).unwrap()).unwrap();
            }
        }
        assert!(fs::metadata(&path).unwrap().len() > MAX_LOG_BYTES);
        rotate_if_needed(&path).unwrap();
        let events = tail(&path, usize::MAX).unwrap();
        assert_eq!(events.len(), MAX_KEPT_LINES);
        assert_eq!(events[0].args, format!("n=1500 {padding}"));
        assert_eq!(mode_of(&path), 0o600);
    }

    #[test]
    fn tail_should_not_read_through_a_link() {
        let base = tempdir().unwrap();
        let target = base.path().join("other.jsonl");
        let mut log = ActionLog::spawn_at(target.clone());
        log.log(ActionEvent::new("search", "x"));
        log.finish_flush();
        let link = base.path().join("actions.jsonl");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(tail(&link, 10).is_err());
    }

    #[test]
    fn truncate_should_back_off_to_a_char_boundary_and_keep_the_prefix() {
        assert_eq!(truncate("abc", 1), "a… (truncated)");
        assert_eq!(truncate("éé", 1), "… (truncated)");
        assert_eq!(truncate("ab", 2), "ab");
    }

    #[test]
    fn bound_to_tail_should_keep_only_the_newest_entries_past_twice_the_budget() {
        let mut lines: Vec<String> = (0..21).map(|i| i.to_string()).collect();
        bound_to_tail(&mut lines, 10);
        assert_eq!(lines.len(), 10);
        assert_eq!(lines[0], "11");
        let mut short: Vec<String> = (0..20).map(|i| i.to_string()).collect();
        bound_to_tail(&mut short, 10);
        assert_eq!(short.len(), 20);
    }

    /// Thousands of short lines under `MAX_LOG_BYTES` are not rotated:
    /// the budget is bytes, and a few KB are far below it.
    #[test]
    fn rotate_if_needed_should_leave_a_small_log_with_many_lines_alone() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("actions.jsonl");
        let body = "x\n".repeat(MAX_KEPT_LINES + 1000);
        fs::write(&path, &body).unwrap();
        rotate_if_needed(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), body);
    }

    /// The writer checks rotation every `ROTATE_CHECK_EVERY` events, not on
    /// the first one: a single event appended to an oversized log leaves
    /// every older line in place.
    #[test]
    fn writer_should_not_rotate_before_the_check_interval() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("actions.jsonl");
        let line = format!("{}\n", "y".repeat(1024));
        let lines = MAX_KEPT_LINES + 1500;
        fs::write(&path, line.repeat(lines)).unwrap();
        assert!(fs::metadata(&path).unwrap().len() > MAX_LOG_BYTES);
        let mut log = ActionLog::spawn_at(path.clone());
        log.log(ActionEvent::new("search", "one"));
        log.finish_flush();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), lines + 1);
    }
}

#[cfg(test)]
mod contract_tests;
