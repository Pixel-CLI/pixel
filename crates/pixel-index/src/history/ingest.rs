// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `ingest.rs` — the low-priority, checkpointed ingest engine.
//!
//! Three phases, resumable via the `ingest_jobs` cursor:
//!   Phase A — refs, commit metadata and changed paths first, always completes.
//!   Phase B — blob sizes of the changed paths (before any diff is requested).
//!   Phase C — diff text, newest commit first, with skips decided BEFORE
//!             spawning git.
//!
//! Diff text is bounded twice (`HistoryLimits`): commits older than the age
//! window never have their diff fetched, and once the database's used pages
//! pass the size budget the oldest diffs are evicted. Commit metadata is
//! never evicted.
//!
//! The engine yields control back to the caller (the daemon's ingest thread)
//! every tick so queries are never blocked: each tick processes a bounded
//! batch then returns; a ref move just enqueues another tick.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use rusqlite::params;

use pixel_io::GitOptions;

use crate::history::poison::{
    BLOB_CAP_BYTES, COMMIT_TEXT_CAP_BYTES, FILE_TEXT_CAP_BYTES, classify_content, decide_skips,
};
use crate::history::store::{
    DIFF_STATE_EVICTED, DIFF_STATE_INDEXED, DIFF_STATE_PENDING, DIFF_STATE_SKIPPED, FactsStore,
    HistoryLimits, REACH_REFLOG_ONLY, REACH_STASH, Result, SKIP_NOTE_OUTSIDE_WINDOW,
    SKIP_NOTE_OVER_BUDGET, normalize_committed_at,
};

/// Default wall-clock budget per tick (250ms per PLAN.md). Queries never wait
/// on ingest: after this budget the engine returns and the daemon yields.
pub const DEFAULT_TICK_BUDGET_MS: u64 = 250;
/// Fixed phase-A metadata batch size.
const PHASE_A_BATCH: usize = 200;
/// Fixed phase-B/C commit batch size (25 per PLAN.md bounded batching).
const PHASE_B_BATCH: usize = 25;
/// 8MB aggregate output cap for a phase-B/C batch (bounded batching).
const BATCH_OUTPUT_CAP_BYTES: usize = 8_388_608;

/// Options controlling one ingest tick.
#[derive(Debug, Clone)]
pub struct IngestOptions {
    pub tick_budget_ms: u64,
    /// The age window and size budget applied to diff text.
    pub limits: HistoryLimits,
}

impl Default for IngestOptions {
    fn default() -> Self {
        IngestOptions {
            tick_budget_ms: DEFAULT_TICK_BUDGET_MS,
            limits: HistoryLimits::default(),
        }
    }
}

/// Time spent in each ingest stage; full-build loops accumulate all ticks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IngestTimings {
    pub metadata: Duration,
    pub blobs: Duration,
    pub diffs: Duration,
    pub retention: Duration,
}

impl IngestTimings {
    fn add(&mut self, tick: &Self) {
        self.metadata += tick.metadata;
        self.blobs += tick.blobs;
        self.diffs += tick.diffs;
        self.retention += tick.retention;
    }
}

/// Outcome of one tick, reported so the daemon can surface progress.
#[derive(Debug, Clone, PartialEq)]
pub struct TickReport {
    pub phase: String,
    pub timings: IngestTimings,
    pub blob_commits_done: u64,
    pub blob_commits_total: u64,
    pub diff_commits_done: u64,
    pub diff_commits_total: u64,
    pub commits_indexed: u64,
    pub total_commits: u64,
    pub diff_indexed_pct: f64,
    pub fresh: bool,
    pub poisoned_this_tick: u64,
    pub skipped_this_tick: u64,
}

/// One parsed commit from phase A (NUL-delimited git log output).
struct PhaseACommit {
    oid: String,
    parents: Vec<String>,
    author: String,
    committed_at: String,
    message: String,
    reach: i64,
    changes: Vec<Change>,
}

struct Change {
    status: String,
    path: String,
    old_path: Option<String>,
}

/// One parsed file diff from phase C.
#[derive(Clone)]
struct PhaseCFile {
    path: String,
    added: String,
    removed: String,
    truncated: bool,
}

struct PhaseCCommit {
    oid: String,
    files: Vec<PhaseCFile>,
}

/// Run one ingest tick. Returns a report describing what happened and the
/// current index state. This is safe to call from the low-priority ingest
/// thread while queries hit the same db (WAL mode, busy_timeout).
pub fn ingest_tick(store: &mut FactsStore, options: &IngestOptions) -> Result<TickReport> {
    let deadline = Instant::now() + Duration::from_millis(options.tick_budget_ms);
    let mut poisoned = 0u64;
    let mut skipped = 0u64;

    let mut timings = IngestTimings::default();
    let start = Instant::now();
    let a_done = phase_a(store, &deadline)?;
    timings.metadata = start.elapsed();
    if !a_done {
        return tick_report(store, timings, poisoned, skipped);
    }
    let start = Instant::now();
    apply_window(store, &options.limits, now_unix())?;
    timings.retention += start.elapsed();
    let start = Instant::now();
    let (b_done, p) = phase_b(store, &deadline)?;
    timings.blobs = start.elapsed();
    poisoned += p;
    if b_done {
        // Give diff ingestion its own yield budget after metadata/blob work.
        let start = Instant::now();
        let c_deadline = start + Duration::from_millis(options.tick_budget_ms);
        let (_c_done, p, s) = phase_c(store, &c_deadline)?;
        timings.diffs = start.elapsed();
        poisoned += p;
        skipped += s;
        let start = Instant::now();
        enforce_budget(store, options.limits.budget_bytes)?;
        timings.retention += start.elapsed();
    }
    tick_report(store, timings, poisoned, skipped)
}

fn tick_report(
    store: &FactsStore,
    timings: IngestTimings,
    poisoned: u64,
    skipped: u64,
) -> Result<TickReport> {
    let state = store.index_state();
    // Exclude merges and expired diffs from the active work denominator.
    // Already indexed/skipped retained commits count as completed work.
    let (blob_done, total, diff_done): (i64, i64, i64) = store.conn().query_row(
        "SELECT COUNT(CASE WHEN id <= ?1 THEN 1 END), COUNT(*),
                COUNT(CASE WHEN diff_state != ?2 THEN 1 END)
         FROM commits WHERE diff_state != ?3 AND COALESCE(skip_note, '') != 'merge'",
        params![
            phase_b_cursor(store),
            DIFF_STATE_PENDING,
            DIFF_STATE_EVICTED
        ],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    Ok(TickReport {
        phase: state.phase,
        timings,
        blob_commits_done: blob_done as u64,
        blob_commits_total: total as u64,
        diff_commits_done: diff_done as u64,
        diff_commits_total: total as u64,
        commits_indexed: state.commits_indexed,
        total_commits: state.total_commits,
        diff_indexed_pct: state.diff_indexed_pct,
        fresh: state.fresh,
        poisoned_this_tick: poisoned,
        skipped_this_tick: skipped,
    })
}

/// Wall-clock safety net for `ingest_until_fresh`: even with every phase
/// guaranteeing forward progress per tick, an unbounded `loop` waiting on a
/// condition is a footgun on its own — if some future change reintroduces a
/// no-progress tick, this turns a silent hang into an explicit error instead
/// of a livelock indistinguishable from a slow legitimate ingest.
pub const MAX_INGEST_UNTIL_FRESH_WALL_CLOCK: Duration = Duration::from_secs(1800);

/// Convenience: run ticks until fresh or the caller gives up. Bounded by
/// `MAX_INGEST_UNTIL_FRESH_WALL_CLOCK` — see its doc comment.
// One-line delegation with the production cap; every test exercises the
// capped form so no test waits 30 minutes on a broken phase.
#[cfg_attr(test, mutants::skip)]
pub fn ingest_until_fresh(store: &mut FactsStore, options: &IngestOptions) -> Result<TickReport> {
    ingest_until_fresh_within(store, options, MAX_INGEST_UNTIL_FRESH_WALL_CLOCK)
}

/// `ingest_until_fresh` with an explicit wall-clock cap. Tests use a short
/// one so a broken ingest phase surfaces as an error in seconds instead of
/// spinning for the production cap.
pub fn ingest_until_fresh_within(
    store: &mut FactsStore,
    options: &IngestOptions,
    wall_clock: Duration,
) -> Result<TickReport> {
    ingest_until_fresh_reporting(store, options, wall_clock, |_| {})
}

/// Build history while reporting progress after each bounded tick.
///
/// Timings in each callback and the final report accumulate this invocation.
/// Existing CLI and daemon callers without a callback remain silent.
///
/// # Errors
/// Returns the same ingest, no-progress and deadline errors as a full build.
pub fn ingest_until_fresh_with_progress(
    store: &mut FactsStore,
    options: &IngestOptions,
    progress: impl FnMut(&TickReport),
) -> Result<TickReport> {
    ingest_until_fresh_reporting(store, options, MAX_INGEST_UNTIL_FRESH_WALL_CLOCK, progress)
}

fn ingest_until_fresh_reporting(
    store: &mut FactsStore,
    options: &IngestOptions,
    wall_clock: Duration,
    mut progress: impl FnMut(&TickReport),
) -> Result<TickReport> {
    let mut timings = IngestTimings::default();
    let mut n = 0u64;
    let start = Instant::now();
    let mut last_mark: Option<String> = None;
    loop {
        let mut report = ingest_tick(store, options)?;
        timings.add(&report.timings);
        report.timings = timings.clone();
        progress(&report);
        n += 1;
        if report.fresh {
            return Ok(report);
        }
        // Every tick lands at least one batch of whichever phase is running,
        // so a tick that leaves the state as it found it never will: fail
        // now rather than spin until the wall-clock cap.
        let mark = progress_mark(store)?;
        if last_mark.as_ref() == Some(&mark) {
            return Err(crate::history::store::FactsError::Msg(format!(
                "ingest made no progress on tick {n} (state {mark}) — last report: {report:?}"
            )));
        }
        last_mark = Some(mark);
        if start.elapsed() >= wall_clock {
            return Err(crate::history::store::FactsError::Msg(format!(
                "ingest_until_fresh did not converge after {n} ticks / {:?} — last report: {:?}",
                start.elapsed(),
                report
            )));
        }
    }
}

/// What a tick that makes progress changes: the commit count (phase A), the
/// pending diffs (phase C) and each phase's cursor and status (phase A's
/// remaining oids, phase B's measured commit).
fn progress_mark(store: &FactsStore) -> Result<String> {
    Ok(store.conn().query_row(
        "SELECT (SELECT count(*) FROM commits) || '/' ||
                (SELECT count(*) FROM commits WHERE diff_state = ?1) || '/' ||
                COALESCE((SELECT group_concat(job, ' ') FROM (
                    SELECT phase || ':' || COALESCE(cursor, '') || ':' || status AS job
                    FROM ingest_jobs ORDER BY phase)), '')",
        [DIFF_STATE_PENDING],
        |r| r.get(0),
    )?)
}

/// Default wall-clock budget for the lazy query-path ingest loop (~3s).
pub const DEFAULT_LAZY_INGEST_BUDGET_MS: u64 = 3000;

/// Bounded ingest loop for the query path: run ticks until fresh or the given
/// wall-clock budget (ms) is exhausted, then return the last report. Unlike
/// `ingest_until_fresh` this never blocks a query for more than `budget_ms`,
/// so it is safe to call from `op_excavate` / `op_history` / `op_lifecycle`
/// when the index is not fresh. Each tick still uses the normal per-tick
/// budget so queries are never starved.
pub fn ingest_until_fresh_bounded(store: &mut FactsStore, budget_ms: u64) -> Result<TickReport> {
    let options = IngestOptions {
        tick_budget_ms: DEFAULT_TICK_BUDGET_MS,
        ..IngestOptions::default()
    };
    let start = Instant::now();
    let mut last = ingest_tick(store, &options)?;
    while !last.fresh && start.elapsed() < Duration::from_millis(budget_ms) {
        last = ingest_tick(store, &options)?;
    }
    Ok(last)
}

/// Convenience: `ingest_until_fresh_bounded` with the default lazy budget
/// ([`DEFAULT_LAZY_INGEST_BUDGET_MS`], 3 s).
pub fn lazy_ingest(store: &mut FactsStore) -> Result<TickReport> {
    ingest_until_fresh_bounded(store, DEFAULT_LAZY_INGEST_BUDGET_MS)
}

// ---------------------------------------------------------------------------
// Phase A — refs + metadata
// ---------------------------------------------------------------------------

fn phase_a(store: &mut FactsStore, deadline: &Instant) -> Result<bool> {
    if !needs_phase_a(store)? {
        return Ok(true);
    }
    // Enumerate every commit reachable from any ref, plus stash and reflog.
    let known = known_oids(store);
    let oids = enumerate_all_commits(store, &known)?;
    let pending: Vec<String> = oids
        .iter()
        .filter(|o| !known.contains(*o))
        .cloned()
        .collect();
    store_phase_a_cursor(store, &pending)?;
    if pending.is_empty() {
        complete_phase_a(store)?;
        return Ok(true);
    }
    // Guaranteed-progress (do-while) loop: the deadline is a soft yield
    // target, not a license to do zero work. Checking it BEFORE the first
    // batch (as a plain `while`) would let setup cost alone (enumerate_all_commits + known_oids — several git subprocess spawns) eat
    // the entire tick budget under load, so the batch body never runs, no
    // commit is ever inserted, and every subsequent tick repeats the exact
    // same expensive-but-fruitless enumeration forever. That is the same
    // defect class this crate exists to prevent (usable-git's ingest budget
    // checked only at loop-tops): at least one batch must always land.
    let mut idx = 0usize;
    loop {
        let batch_end = (idx + PHASE_A_BATCH).min(pending.len());
        let batch = &pending[idx..batch_end];
        let (commits, reach) = fetch_phase_a_batch(store, batch)?;
        insert_phase_a_batch(store, &commits, &reach)?;
        idx = batch_end;
        if idx >= pending.len() || Instant::now() >= *deadline {
            break;
        }
    }
    let done = idx >= pending.len();
    if done {
        complete_phase_a(store)?;
    } else {
        store_phase_a_cursor(store, &pending[idx..])?;
    }
    Ok(done)
}

fn needs_phase_a(store: &FactsStore) -> Result<bool> {
    // Phase A needs work if there is no 'done' row (fresh DB or interrupted),
    // OR the refs have moved since the last phase-A run (the stored ref_hash
    // no longer matches the current refs). The latter is what fixes the
    // frozen-at-commit-11 class: a 'done' row alone no longer means "never
    // re-run".
    let status: Option<String> = store
        .conn()
        .query_row(
            "SELECT status FROM ingest_jobs WHERE phase = 'A'",
            [],
            |r| r.get(0),
        )
        .ok();
    if !matches!(status.as_deref(), Some("done")) {
        return Ok(true);
    }
    let stored: Option<String> = store
        .conn()
        .query_row(
            "SELECT ref_hash FROM ingest_jobs WHERE phase = 'A'",
            [],
            |r| r.get(0),
        )
        .ok();
    let current = store.refs_hash()?;
    Ok(stored.as_deref() != Some(current.as_str()))
}

fn complete_phase_a(store: &mut FactsStore) -> Result<()> {
    // Record the refs hash at completion so a later ref move is detectable.
    let hash = store.refs_hash()?;
    store.conn().execute(
        "UPDATE ingest_jobs SET status = 'done', ref_hash = ?1, updated_at = ?2 WHERE phase = 'A'",
        params![hash, now_iso()],
    )?;
    let _ = store.conn().execute("DELETE FROM reach_map", []);
    Ok(())
}

fn store_phase_a_cursor(store: &mut FactsStore, rest: &[String]) -> Result<()> {
    let cursor = rest.first().cloned().unwrap_or_default();
    let exists: i64 = store
        .conn()
        .query_row(
            "SELECT count(*) FROM ingest_jobs WHERE phase = 'A'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if exists > 0 {
        store.conn().execute(
            "UPDATE ingest_jobs SET cursor = ?1, status = 'pending', updated_at = ?2 WHERE phase = 'A'",
            params![cursor, now_iso()],
        )?;
    } else {
        store.conn().execute(
            "INSERT INTO ingest_jobs (phase, cursor, status, created_at, updated_at)
             VALUES ('A', ?1, 'pending', ?2, ?2)",
            params![cursor, now_iso()],
        )?;
    }
    Ok(())
}

/// Reach of a commit listed by `rev-list --branches --remotes --tags`:
/// `REACH_BRANCH | REACH_REMOTE | REACH_TAG`, spelled as its value because
/// the three bits are disjoint, so `|`, `^` and `+` agree (a test pins it).
const REACH_LISTED_REFS: i64 = 0b111;

/// The store's runner with the cap of a repository-wide enumeration: one
/// 41-byte line per commit puts `rev-list --branches` past the default 1 MiB
/// at about 25 500 commits, and one line per ref does the same to
/// `for-each-ref` on a repository with thousands of tags.
pub(crate) fn enumeration_runner(store: &FactsStore) -> pixel_io::GitRunner {
    store
        .runner()
        .with_max_output_bytes(Some(pixel_io::ENUMERATION_MAX_OUTPUT_BYTES))
}

/// The output of an enumeration, `None` when git refused the revision itself
/// (no `refs/stash`, no reflog): there is nothing to list. Any other failure
/// (a timeout, an output cap, git missing) is an error, never an empty list:
/// an empty enumeration completes phase A with no commit at all.
fn listed(result: std::result::Result<Vec<u8>, pixel_io::GitError>) -> Result<Option<Vec<u8>>> {
    match result {
        Ok(out) => Ok(Some(out)),
        Err(pixel_io::GitError::NonZeroExit { .. }) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Enumerate every commit that should be indexed: all reachable from refs,
/// plus stash, plus reflog-only commits. Returns oids in reverse (oldest-first)
/// order and records the reach bitmask of each one not in `known` in
/// `reach_map`, which only the insert of a new commit reads.
fn enumerate_all_commits(
    store: &mut FactsStore,
    known: &std::collections::HashSet<String>,
) -> Result<Vec<String>> {
    let runner = enumeration_runner(store);
    let mut all: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut reach: Vec<(String, i64)> = Vec::new();
    let mut take = |out: &[u8], bits: i64| {
        for oid in split_nul_lines(out) {
            if !oid.is_empty() && seen.insert(oid.clone()) {
                if !known.contains(&oid) {
                    reach.push((oid.clone(), bits));
                }
                all.push(oid);
            }
        }
    };

    // Branches + remotes + tags via a single rev-list of all heads/remotes/tags.
    if let Some(out) =
        listed(runner.run(&["rev-list", "--reverse", "--branches", "--remotes", "--tags"]))?
    {
        take(&out, REACH_LISTED_REFS);
    }
    // Stash.
    if let Some(out) = listed(runner.run(&["rev-list", "--reverse", "refs/stash"]))? {
        take(&out, REACH_STASH);
    }
    // Reflog-only commits (reachable via reflogs but not any branch/remote/tag).
    if let Some(out) = listed(runner.run(&[
        "rev-list",
        "--reverse",
        "--reflog",
        "--not",
        "--branches",
        "--remotes",
        "--tags",
    ]))? {
        take(&out, REACH_REFLOG_ONLY);
    }
    write_reach_map(store, &reach)?;
    Ok(all)
}

/// Replace `reach_map` with `entries` in one transaction: one commit per
/// enumeration, not one per oid.
fn write_reach_map(store: &mut FactsStore, entries: &[(String, i64)]) -> Result<()> {
    let tx = store.conn_mut().transaction()?;
    tx.execute("DELETE FROM reach_map", [])?;
    {
        let mut insert = tx.prepare(
            "INSERT INTO reach_map (oid, bits) VALUES (?1, ?2)
             ON CONFLICT (oid) DO UPDATE SET bits = bits | excluded.bits",
        )?;
        for (oid, bits) in entries {
            insert.execute(params![oid, bits])?;
        }
    }
    tx.commit()?;
    Ok(())
}

fn known_oids(store: &FactsStore) -> std::collections::HashSet<String> {
    let mut stmt = store
        .conn()
        .prepare("SELECT oid FROM commits")
        .expect("select oid");
    let rows = stmt.query_map([], |r| r.get::<_, String>(0)).expect("rows");
    let mut set = std::collections::HashSet::new();
    for row in rows.flatten() {
        set.insert(row);
    }
    set
}

/// Fetch one batch of phase-A commit metadata via `git log -z --no-walk`.
/// Uses the NUL-separated format from usable-git's sound parser.
fn fetch_phase_a_batch(
    store: &FactsStore,
    oids: &[String],
) -> Result<(Vec<PhaseACommit>, Vec<String>)> {
    // Higher cap than the default 1MiB: a 200-commit metadata batch with long
    // messages / many changed paths can exceed it. This is bounded by the
    // PHASE_A_BATCH commit count, not by diff text (phase A has no diff text).
    let opts = GitOptions {
        timeout: Some(Duration::from_secs(120)),
        max_output_bytes: Some(BATCH_OUTPUT_CAP_BYTES),
    };
    let runner = pixel_io::GitRunner::with_options(store.root(), opts);
    let mut args: Vec<&str> = vec![
        "log",
        "-z",
        "--no-walk=unsorted",
        "--format=%x1e%H%x00%P%x00%an%x00%aI%x00%B",
        "--name-status",
        "--end-of-options",
    ];
    let oid_refs: Vec<&str> = oids.iter().map(String::as_str).collect();
    args.extend(oid_refs);
    let out = runner.run(&args)?;
    Ok((parse_phase_a(&out), oids.to_vec()))
}

/// Parse phase-A `git log -z --format=%x1e%H%x00%P%x00%an%x00%aI%x00%B --name-status`
/// records. Each record opens with \x1e and carries five NUL-separated
/// metadata fields followed by name-status entries (where the first status
/// token arrives with a leading newline), all NUL-delimited.
fn parse_phase_a(output: &[u8]) -> Vec<PhaseACommit> {
    let mut commits = Vec::new();
    for record in output.split(|&b| b == 0x1e).filter(|r| !r.is_empty()) {
        let fields: Vec<&[u8]> = record.split(|&b| b == 0).collect();
        if fields.len() < 5 {
            continue;
        }
        let str = |f: &[u8]| String::from_utf8_lossy(f).into_owned();
        let oid = str(fields[0]).trim().to_string();
        let parents: Vec<String> = str(fields[1])
            .trim()
            .split(' ')
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .collect();
        let author = str(fields[2]).trim().to_string();
        let committed_at = str(fields[3]).trim().to_string();
        let message = str(fields[4]).trim_end_matches('\n').to_string();
        let mut changes = Vec::new();
        let mut rest = fields[5..].iter();
        while let Some(raw) = rest.next() {
            let raw = str(raw);
            let raw_status = raw.trim_start();
            if raw_status.is_empty() {
                continue;
            }
            let status = raw_status.chars().next().unwrap_or(' ');
            match status {
                'R' | 'C' => {
                    let (Some(old_path), Some(path)) = (rest.next(), rest.next()) else {
                        break;
                    };
                    changes.push(Change {
                        status: status.to_string(),
                        path: str(path),
                        old_path: Some(str(old_path)),
                    });
                }
                _ => {
                    let Some(path) = rest.next() else {
                        break;
                    };
                    changes.push(Change {
                        status: status.to_string(),
                        path: str(path),
                        old_path: None,
                    });
                }
            }
        }
        commits.push(PhaseACommit {
            oid,
            parents,
            author,
            committed_at,
            message,
            reach: 0,
            changes,
        });
    }
    commits
}

fn insert_phase_a_batch(
    store: &mut FactsStore,
    commits: &[PhaseACommit],
    _oid_list: &[String],
) -> Result<()> {
    let tx = store.conn_mut().transaction()?;
    {
        let mut ins = tx.prepare(
            "INSERT INTO commits (oid, parents, author, committed_at, message, reach, diff_state, skip_note)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        let mut sel = tx.prepare("SELECT id FROM commits WHERE oid = ?1")?;
        let mut ins_msg =
            tx.prepare("INSERT INTO messages_fts (rowid, message) VALUES (?1, ?2)")?;
        let mut ins_fc = tx.prepare(
            "INSERT OR IGNORE INTO file_changes (commit_id, path, status, old_path) VALUES (?1, ?2, ?3, ?4)",
        )?;
        let mut ins_path = tx.prepare("INSERT INTO path_fts (rowid, path) VALUES (?1, ?2)")?;
        for c in commits {
            let reach = c.reach;
            // Merge in any reach bits discovered during enumeration.
            let extra: Option<i64> = tx
                .query_row("SELECT bits FROM reach_map WHERE oid = ?1", [&c.oid], |r| {
                    r.get(0)
                })
                .ok();
            let final_reach = reach | extra.unwrap_or(0);
            let is_merge = c.parents.len() > 1;
            let diff_state = if is_merge {
                DIFF_STATE_SKIPPED
            } else {
                DIFF_STATE_PENDING
            };
            let skip_note = if is_merge {
                Some("merge".to_string())
            } else {
                None
            };
            ins.execute(params![
                c.oid,
                c.parents.join(" "),
                c.author,
                normalize_committed_at(&tx, &c.committed_at)?,
                c.message,
                final_reach,
                diff_state,
                skip_note
            ])?;
            if let Ok(id) = sel.query_row([&c.oid], |r| r.get::<_, i64>(0)) {
                ins_msg.execute(params![id, c.message])?;
                for ch in &c.changes {
                    // Index the path only when the row is new: `OR IGNORE`
                    // skips a (commit, path) pair already recorded, and a
                    // second index entry for its rowid would be a duplicate.
                    if ins_fc.execute(params![id, ch.path, ch.status, ch.old_path])? == 1 {
                        ins_path.execute(params![tx.last_insert_rowid(), ch.path])?;
                    }
                }
            }
        }
    }
    tx.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Phase B — blob sizes of the paths phase A recorded (before any diff)
// ---------------------------------------------------------------------------

/// Returns (phaseB_done, poisoned_this).
fn phase_b(store: &mut FactsStore, deadline: &Instant) -> Result<(bool, u64)> {
    // Phase B is "complete" when no commit remains to be blob-measured. This is
    // re-evaluated every tick, so new commits (ref moves, incremental) get
    // measured naturally. Cursor = last commit id measured.
    let mut poisoned = 0u64;
    let mut cursor: i64 = phase_b_cursor(store);
    loop {
        let batch = next_phase_b_commits(store, cursor)?;
        let cid = match batch.last() {
            Some(c) => *c,
            None => {
                complete_phase_b(store)?;
                return Ok((true, poisoned));
            }
        };
        let poisoned_batch = measure_commits_blobs(store, &batch)?;
        poisoned += poisoned_batch;
        // checkpoint cursor (upsert: the B row may not exist on first tick)
        store.conn().execute(
            "INSERT INTO ingest_jobs (phase, cursor, status, created_at, updated_at)
             VALUES ('B', ?1, 'running', ?2, ?2)
             ON CONFLICT (phase) DO UPDATE SET cursor = excluded.cursor, updated_at = excluded.updated_at",
            params![cid.to_string(), now_iso()],
        )?;
        cursor = cid;
        if Instant::now() >= *deadline {
            return Ok((false, poisoned));
        }
    }
}

fn phase_b_cursor(store: &FactsStore) -> i64 {
    store
        .conn()
        .query_row(
            "SELECT cursor FROM ingest_jobs WHERE phase = 'B'",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn next_phase_b_commits(store: &FactsStore, after_cursor: i64) -> Result<Vec<i64>> {
    let mut stmt = store
        .conn()
        .prepare("SELECT id FROM commits WHERE id > ?1 AND diff_state = ?2 ORDER BY id LIMIT ?3")?;
    let rows = stmt.query_map(
        params![after_cursor, DIFF_STATE_PENDING, PHASE_B_BATCH as i64],
        |r| r.get(0),
    )?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Measure several commits with one size-only Git protocol, before checkpointing
/// any of them. Fall back to the bounded tree reader if the protocol fails or
/// cannot represent a filename (newline or trailing carriage return).
fn measure_commits_blobs(store: &mut FactsStore, commits: &[i64]) -> Result<u64> {
    let mut pairs = Vec::new();
    for cid in commits {
        let oid: String =
            store
                .conn()
                .query_row("SELECT oid FROM commits WHERE id = ?1", [cid], |r| r.get(0))?;
        for path in changed_paths_for_commit(store, *cid)? {
            let previous =
                parent_path_for(store, &oid, &path).map(|(parent, old)| format!("{parent}:{old}"));
            pairs.push((format!("{oid}:{path}"), previous, path));
        }
    }
    let specs: Vec<&str> = pairs
        .iter()
        .flat_map(|(new, old, _)| std::iter::once(new.as_str()).chain(old.as_deref()))
        .collect();
    let runner = pixel_io::GitRunner::with_options(
        store.root(),
        GitOptions {
            timeout: Some(Duration::from_secs(30)),
            max_output_bytes: Some(1_048_576),
        },
    );
    let Some(sizes) = batch_blob_sizes(&runner, &specs) else {
        return commits.iter().try_fold(0, |total, cid| {
            Ok(total + measure_commit_blobs(store, *cid)?)
        });
    };
    let mut sizes = sizes.into_iter();
    let mut poisoned = 0;
    for (_, old, path) in pairs {
        let added = sizes.next().expect("one answer per requested blob");
        let removed = if old.is_some() {
            sizes.next().expect("one answer per parent blob")
        } else {
            0
        };
        if added > BLOB_CAP_BYTES as u64 || removed > BLOB_CAP_BYTES as u64 {
            store.learn_poison(&path, &format!("blob over {BLOB_CAP_BYTES}B cap"))?;
            poisoned += 1;
        }
    }
    Ok(poisoned)
}

/// Only metadata is requested: even a multi-gigabyte object is never inflated.
fn batch_blob_sizes(runner: &pixel_io::GitRunner, specs: &[&str]) -> Option<Vec<u64>> {
    if specs.iter().any(|s| s.contains('\n') || s.ends_with('\r')) {
        return None;
    }
    let mut sizes = Vec::with_capacity(specs.len());
    for batch in specs.chunks(TREE_BATCH_PATHS) {
        let mut input = batch.join("\n");
        input.push('\n');
        let output = runner
            .run_with_stdin(
                &["cat-file", "--batch-check=%(objecttype) %(objectsize)"],
                input.as_bytes(),
            )
            .ok()?;
        if !output.success() {
            return None;
        }
        let body = output.stdout.strip_suffix(b"\n")?;
        let lines: Vec<&[u8]> = body.split(|b| *b == b'\n').collect();
        if lines.len() != batch.len() {
            return None;
        }
        for (line, spec) in lines.into_iter().zip(batch) {
            let line = std::str::from_utf8(line).ok()?;
            let size = if let Some(size) = line.strip_prefix("blob ") {
                size.parse().ok()?
            } else if line == format!("{spec} missing")
                || ["tree ", "commit ", "tag "]
                    .iter()
                    .any(|prefix| line.starts_with(prefix))
            {
                0
            } else {
                return None;
            };
            sizes.push(size);
        }
    }
    Some(sizes)
}

fn complete_phase_b(store: &mut FactsStore) -> Result<()> {
    store.conn().execute(
        "UPDATE ingest_jobs SET status = 'done', updated_at = ?1 WHERE phase = 'B'",
        [now_iso()],
    )?;
    Ok(())
}

/// For one commit, gather its changed paths, measure blob sizes via
/// bounded `ls-tree` batches, and learn any over-cap paths as poison forever.
/// Returns the number of newly-poisoned paths.
fn measure_commit_blobs(store: &mut FactsStore, cid: i64) -> Result<u64> {
    let paths = changed_paths_for_commit(store, cid)?;
    let oid: String =
        store
            .conn()
            .query_row("SELECT oid FROM commits WHERE id = ?1", [cid], |r| r.get(0))?;
    let mut poisoned = 0u64;
    let sizes = measure_blob_sizes(store, &oid, &paths)?;
    for (path, (size_add, size_rem)) in &sizes {
        if *size_add > BLOB_CAP_BYTES as u64 || *size_rem > BLOB_CAP_BYTES as u64 {
            store.learn_poison(path, &format!("blob over {BLOB_CAP_BYTES}B cap"))?;
            poisoned += 1;
        }
    }
    Ok(poisoned)
}

fn changed_paths_for_commit(store: &FactsStore, cid: i64) -> Result<Vec<String>> {
    let mut stmt = store
        .conn()
        .prepare("SELECT path FROM file_changes WHERE commit_id = ?1")?;
    let rows = stmt.query_map([cid], |r| r.get::<_, String>(0))?;
    let mut v = Vec::new();
    for row in rows {
        v.push(row?);
    }
    Ok(v)
}

/// Measure changed paths in bounded batches, including rename sources in the
/// parent tree. Keep the result in input order; deleted/non-blob entries are 0.
fn measure_blob_sizes(
    store: &FactsStore,
    oid: &str,
    paths: &[String],
) -> Result<Vec<(String, (u64, u64))>> {
    let opts = GitOptions {
        timeout: Some(Duration::from_secs(30)),
        max_output_bytes: Some(1_048_576),
    };
    let cmd_runner = pixel_io::GitRunner::with_options(store.root(), opts);
    let current: Vec<&str> = paths.iter().map(String::as_str).collect();
    let new_sizes = blob_sizes(&cmd_runner, oid, &current);
    let old_paths: Vec<Option<String>> = paths
        .iter()
        .map(|path| parent_path_for(store, oid, path).map(|(_, old)| old))
        .collect();
    let renamed: Vec<&str> = old_paths.iter().filter_map(Option::as_deref).collect();
    let old_sizes = blob_sizes(&cmd_runner, &format!("{oid}^"), &renamed);
    Ok(paths
        .iter()
        .zip(old_paths)
        .map(|(path, old)| {
            let added = new_sizes.get(path.as_str()).copied().unwrap_or(0);
            let removed = old
                .as_deref()
                .and_then(|p| old_sizes.get(p))
                .copied()
                .unwrap_or(0);
            (path.to_owned(), (added, removed))
        })
        .collect())
}

/// Bound argv below platform limits and output below the runner's 1 MiB cap.
const TREE_BATCH_PATHS: usize = 128;
const TREE_BATCH_PATH_BYTES: usize = 32_768;

/// Number of paths fitting one argv batch; even an oversized first path advances.
fn tree_batch_count(paths: &[&str]) -> usize {
    let mut bytes = 0;
    paths
        .iter()
        .take(TREE_BATCH_PATHS)
        .take_while(|path| {
            bytes += path.len() + 1;
            bytes <= TREE_BATCH_PATH_BYTES
        })
        .count()
        .max(1)
}

fn blob_sizes(runner: &pixel_io::GitRunner, rev: &str, paths: &[&str]) -> HashMap<String, u64> {
    let mut sizes = HashMap::with_capacity(paths.len());
    let mut remaining = paths;
    while !remaining.is_empty() {
        let count = tree_batch_count(remaining);
        let (batch, rest) = remaining.split_at(count);
        let mut args = vec![
            "--literal-pathspecs",
            "ls-tree",
            "-l",
            "-z",
            "--end-of-options",
            rev,
            "--",
        ];
        args.extend_from_slice(batch);
        if let Ok(output) = runner.run(&args) {
            for record in output.split(|b| *b == 0) {
                if let Some(tab) = record.iter().position(|b| *b == b'\t') {
                    let path = String::from_utf8_lossy(&record[tab + 1..]).into_owned();
                    let size = parse_ls_tree_size(&String::from_utf8_lossy(&record[..tab]));
                    sizes.insert(path, size);
                }
            }
        } else {
            // A failed/capped batch must not hide a large blob that a single
            // bounded lookup can still measure.
            for path in batch {
                sizes.insert((*path).to_owned(), blob_size(runner, rev, path));
            }
        }
        remaining = rest;
    }
    sizes
}

/// Size of the blob at `path` in `rev`'s tree; 0 when the entry is missing
/// or is not a blob (a directory, a submodule). Fallback for a failed batch.
fn blob_size(runner: &pixel_io::GitRunner, rev: &str, path: &str) -> u64 {
    match runner.run(&[
        "--literal-pathspecs",
        "ls-tree",
        "-l",
        "--end-of-options",
        rev,
        "--",
        path,
    ]) {
        Ok(bytes) => parse_ls_tree_size(&String::from_utf8_lossy(&bytes)),
        Err(_) => 0,
    }
}

/// `<mode> <type> <oid> <size>\t<path>` from `ls-tree -l`; the size column
/// is `-` for anything but a blob.
fn parse_ls_tree_size(line: &str) -> u64 {
    let mut fields = line.split_whitespace();
    let (Some(_mode), Some(kind), Some(_oid), Some(size)) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return 0;
    };
    if kind != "blob" {
        return 0;
    }
    size.parse().unwrap_or(0)
}

/// Both sides of a retained diff must be checked, including modifications
/// and deletions whose large parent belongs to an expired commit. Merge diffs
/// are skipped in phase A; ordinary commits have at most one parent.
fn parent_path_for(store: &FactsStore, oid: &str, path: &str) -> Option<(String, String)> {
    let (parents, old): (String, String) = store
        .conn()
        .query_row(
            "SELECT c.parents, COALESCE(f.old_path, f.path) FROM file_changes f
         JOIN commits c ON c.id = f.commit_id WHERE c.oid = ?1 AND f.path = ?2",
            params![oid, path],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok()?;
    Some((parents.split_whitespace().next()?.to_owned(), old))
}

// ---------------------------------------------------------------------------
// Phase C — diff text with pre-decided skips
// ---------------------------------------------------------------------------

fn phase_c(store: &mut FactsStore, deadline: &Instant) -> Result<(bool, u64, u64)> {
    let pending = pending_phase_c(store)?;
    if pending.is_empty() {
        return Ok((true, 0, 0));
    }
    let mut poisoned = 0u64;
    let mut skipped = 0u64;
    // Same guaranteed-progress (do-while) shape as phase_a: the deadline is
    // checked AFTER a batch runs, never before the first one, so a tick
    // whose earlier phases (A/B) already consumed most of the shared budget
    // still lands at least one phase-C batch instead of looping forever with
    // zero forward progress.
    let mut idx = 0usize;
    loop {
        let batch_end = (idx + PHASE_B_BATCH).min(pending.len());
        let batch = &pending[idx..batch_end];
        let (p, s) = ingest_diff_batch(store, batch)?;
        poisoned += p;
        skipped += s;
        idx = batch_end;
        if idx >= pending.len() || Instant::now() >= *deadline {
            break;
        }
    }
    Ok((idx >= pending.len(), poisoned, skipped))
}

/// Commits whose diff is still to fetch, newest first: the diffs a size
/// budget keeps are the recent ones, so fetching them first means an
/// eviction never throws away work just done on an old commit.
fn pending_phase_c(store: &FactsStore) -> Result<Vec<i64>> {
    let mut stmt = store.conn().prepare(
        "SELECT id FROM commits
         WHERE diff_state = ?1
         ORDER BY unixepoch(committed_at) DESC, id DESC
         LIMIT 100000",
    )?;
    let rows = stmt.query_map([DIFF_STATE_PENDING], |r| r.get::<_, i64>(0))?;
    let mut v = Vec::new();
    for row in rows {
        v.push(row?);
    }
    Ok(v)
}

/// Ingest diff text for a batch of commits, applying the pre-decided skip plan
/// (poison + structural excludes) so git never emits poison blobs.
fn ingest_diff_batch(store: &mut FactsStore, batch: &[i64]) -> Result<(u64, u64)> {
    // Gather touched paths across the batch and build the exclude list BEFORE
    // spawning git.
    let oids: Vec<String> = batch
        .iter()
        .map(|cid| {
            store
                .conn()
                .query_row("SELECT oid FROM commits WHERE id = ?1", [cid], |r| r.get(0))
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut touched: Vec<String> = Vec::new();
    for cid in batch {
        touched.extend(changed_paths_for_commit(store, *cid)?);
    }
    let plan = decide_skips(store, &touched);
    let mut skip_ledger = plan.skipped.clone();

    let opts = GitOptions {
        timeout: Some(Duration::from_secs(60)),
        max_output_bytes: Some(BATCH_OUTPUT_CAP_BYTES),
    };
    let runner = pixel_io::GitRunner::with_options(store.root(), opts);
    // Pathspec excludes come AFTER the `--` separator so git treats them as
    // pathspecs (never revisions/options). With only negative pathspecs git
    // shows every file except the excluded ones — poison blobs never emitted.
    let mut args: Vec<String> = vec![
        "show".to_string(),
        "-U0".to_string(),
        "--no-color".to_string(),
        "--format=%x1e%H".to_string(),
        "--diff-filter=AMDRT".to_string(),
        "--find-renames".to_string(),
    ];
    args.push("--end-of-options".to_string());
    for oid in &oids {
        args.push(oid.clone());
    }
    args.push("--".to_string());
    for ex in &plan.excludes {
        args.push(ex.clone());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    let result = runner.run(&arg_refs);
    match result {
        Ok(bytes) => {
            let commits = parse_phase_c(&bytes);
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for c in &commits {
                seen.insert(c.oid.clone());
                insert_phase_c_commit(store, c)?;
            }
            // `git show` prints NOTHING for a commit whose every changed path
            // was excluded via pathspec — not even the `%x1e<oid>` marker. Such
            // a commit is silently absent from `commits` above; left alone its
            // diff_state would stay PENDING forever (this is exactly the class
            // of "ingest never converges" bug this crate exists to prevent).
            // It has nothing to index, so mark it explicitly, never silently.
            for oid in &oids {
                if !seen.contains(oid) {
                    store.conn().execute(
                        "UPDATE commits SET diff_state = ?1, skip_note = 'all-paths-skipped' WHERE oid = ?2 AND diff_state = ?3",
                        params![DIFF_STATE_SKIPPED, oid, DIFF_STATE_PENDING],
                    )?;
                }
            }
            Ok((0, skip_ledger.len() as u64))
        }
        Err(pixel_io::GitError::OutputTooLarge { .. }) => {
            // Bounded batching: overflow → process one-at-a-time with own caps.
            // At worst a single commit lands as skipped:over-cap.
            for oid in &oids {
                ingest_diff_single(store, oid)?;
            }
            Ok((0, skip_ledger.len() as u64))
        }
        Err(_) => {
            // A commit's diff couldn't be produced (e.g. gc-pruned oid): skip.
            for oid in &oids {
                store.conn().execute(
                    "UPDATE commits SET diff_state = ?1, skip_note = 'unresolvable' WHERE oid = ?2 AND diff_state = ?3",
                    params![DIFF_STATE_SKIPPED, oid, DIFF_STATE_PENDING],
                )?;
                skip_ledger.push((oid.clone(), "unresolvable".to_string()));
            }
            Ok((0, skip_ledger.len() as u64))
        }
    }
}

/// Process one commit's diff with its own caps (single mode, no batch cap).
fn ingest_diff_single(store: &mut FactsStore, oid: &str) -> Result<()> {
    // Re-decide skips for this single commit so poison paths are still excluded.
    let cid: Option<i64> = store
        .conn()
        .query_row("SELECT id FROM commits WHERE oid = ?1", [oid], |r| r.get(0))
        .ok();
    let mut touched: Vec<String> = Vec::new();
    if let Some(cid) = cid {
        touched = changed_paths_for_commit(store, cid)?;
    }
    let plan = decide_skips(store, &touched);
    let opts = GitOptions {
        timeout: Some(Duration::from_secs(60)),
        max_output_bytes: Some(COMMIT_TEXT_CAP_BYTES + FILE_TEXT_CAP_BYTES + 4096),
    };
    let runner = pixel_io::GitRunner::with_options(store.root(), opts);
    let mut args = vec![
        "show".to_string(),
        "-U0".to_string(),
        "--no-color".to_string(),
        "--format=%x1e%H".to_string(),
        "--diff-filter=AMDRT".to_string(),
        "--find-renames".to_string(),
        "--end-of-options".to_string(),
        oid.to_string(),
        "--".to_string(),
    ];
    for ex in &plan.excludes {
        args.push(ex.clone());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    match runner.run(&arg_refs) {
        Ok(bytes) => {
            let commits = parse_phase_c(&bytes);
            let mut found = false;
            for c in &commits {
                if c.oid == oid {
                    found = true;
                }
                insert_phase_c_commit(store, c)?;
            }
            // Same silent-absence case as the batch path: a commit whose every
            // changed path was excluded produces no `%x1e<oid>` record at all.
            if !found {
                store.conn().execute(
                    "UPDATE commits SET diff_state = ?1, skip_note = 'all-paths-skipped' WHERE oid = ?2 AND diff_state = ?3",
                    params![DIFF_STATE_SKIPPED, oid, DIFF_STATE_PENDING],
                )?;
            }
            Ok(())
        }
        Err(_) => {
            store.conn().execute(
                "UPDATE commits SET diff_state = ?1, skip_note = 'over-cap' WHERE oid = ?2 AND diff_state = ?3",
                params![DIFF_STATE_SKIPPED, oid, DIFF_STATE_PENDING],
            )?;
            Ok(())
        }
    }
}

fn parse_phase_c(output: &[u8]) -> Vec<PhaseCCommit> {
    let text = match std::str::from_utf8(output) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let records: Vec<&str> = text.split('\x1e').filter(|r| !r.is_empty()).collect();
    let mut commits = Vec::new();
    for record in records {
        let newline = record.find('\n').unwrap_or(record.len());
        let oid = record[..newline].trim().to_string();
        let body = &record[newline..];
        let mut files = Vec::new();
        let mut current: Option<PhaseCFile> = None;
        for line in body.split('\n') {
            if line.starts_with("diff --git ") {
                // Flush the previous file (with all its accumulated added/
                // removed text) before starting the next one. Pushing a clone
                // of `current` right here (before any content lines for THIS
                // file have been seen) instead of on flush was the original
                // bug: every subsequent `c.added`/`c.removed` mutation landed
                // on `current` alone and was never reflected back into
                // `files`, so every hunk was inserted with empty text and
                // diff_grams never got a single posting — search/excavate over
                // diff content silently returned nothing for real content.
                if let Some(prev) = current.take() {
                    files.push(prev);
                }
                // parse a/path b/path
                let mut path = String::new();
                if let Some(idx) = line.find(" b/") {
                    path = line[idx + 3..].to_string();
                }
                current = Some(PhaseCFile {
                    path,
                    added: String::new(),
                    removed: String::new(),
                    truncated: false,
                });
                continue;
            }
            if let Some(c) = current.as_mut() {
                if line.starts_with("Binary files ") || line == "GIT binary patch" {
                    // mark binary by clearing text; a binary file's text is noise.
                    c.added.clear();
                    c.removed.clear();
                    continue;
                }
                if line.starts_with("+++") || line.starts_with("---") {
                    continue;
                }
                // `added_len + removed_len` is already the file's total
                // accumulated text so far — the cap check must compare against
                // that total alone. The previous `buf.len() + added_len +
                // removed_len` added `buf.len()` on top, double-counting
                // whichever side `buf` aliases (it IS `c.added.len()` again on
                // a '+' line, `c.removed.len()` again on a '-' line), so the
                // effective cap was silently half of FILE_TEXT_CAP_BYTES for
                // any file whose diff leans to one side — still a bound, but
                // not the documented one.
                let added_len = c.added.len();
                let removed_len = c.removed.len();
                let target = if line.starts_with('+') {
                    Some(&mut c.added)
                } else if line.starts_with('-') {
                    Some(&mut c.removed)
                } else {
                    None
                };
                if let Some(buf) = target {
                    // Predictive, not reactive: check whether THIS line's
                    // write would cross the cap before writing it, not
                    // whether the buffer already crossed it after a previous
                    // write. A reactive check (comparing the pre-write total
                    // to the cap) still lets one more line's worth of bytes
                    // land past the boundary every time — exactly the
                    // "budget checked but not enforced during the write"
                    // defect class this crate exists to close. `+1` accounts
                    // for the trailing '\n' this push always adds.
                    let incoming = line.len().saturating_sub(1) + 1;
                    if added_len + removed_len + incoming > FILE_TEXT_CAP_BYTES {
                        c.truncated = true;
                        continue;
                    }
                    buf.push_str(&line[1..]);
                    buf.push('\n');
                }
            }
        }
        // Flush the last file in the record (no trailing "diff --git" line
        // follows it to trigger the flush above).
        if let Some(last) = current.take() {
            files.push(last);
        }
        commits.push(PhaseCCommit { oid, files });
    }
    commits
}

fn insert_phase_c_commit(store: &mut FactsStore, commit: &PhaseCCommit) -> Result<()> {
    let cid: Option<i64> = store
        .conn()
        .query_row(
            "SELECT id FROM commits WHERE oid = ?1",
            [&commit.oid],
            |r| r.get(0),
        )
        .ok();
    let cid = match cid {
        Some(id) => id,
        None => return Ok(()), // metadata not yet indexed; phase A will re-run
    };
    // Precompute skip decisions BEFORE opening the transaction (avoids holding
    // a conn borrow while also mutating it). Excluded paths never appear in the
    // diff output (pathspec magic), but a poison learned mid-batch still needs
    // recording. Content heuristics run on the first 4KB of the combined text.
    let skip: Vec<bool> = commit
        .files
        .iter()
        .map(|f| {
            if store.skip_reason(&f.path).is_some() {
                return true;
            }
            let probe: Vec<u8> = f
                .added
                .as_bytes()
                .iter()
                .chain(f.removed.as_bytes())
                .take(crate::history::poison::CONTENT_PROBE_BYTES)
                .copied()
                .collect();
            let kind = classify_content(&probe, &f.path);
            !matches!(kind, crate::history::poison::ContentKind::Text)
        })
        .collect();
    let tx = store.conn_mut().transaction()?;
    {
        let mut ins = tx.prepare(
            "INSERT INTO hunks (commit_id, path, added, removed, truncated)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        let mut ins_text = tx.prepare("INSERT INTO diff_fts (rowid, text) VALUES (?1, ?2)")?;
        let mut mark =
            tx.prepare("UPDATE commits SET diff_state = ?1 WHERE id = ?2 AND diff_state = ?3")?;
        let mut commit_bytes = 0usize;
        let mut over_cap = false;
        for (i, file) in commit.files.iter().enumerate() {
            let path = &file.path;
            // A hunk without text (a binary file, a pure rename or mode
            // change) has nothing to search: no row, so a hunk row without
            // text only ever means lost text (what `doctor` reports).
            if skip[i] || (file.added.is_empty() && file.removed.is_empty()) {
                continue;
            }
            if commit_bytes >= COMMIT_TEXT_CAP_BYTES {
                over_cap = true;
                break;
            }
            let id: i64 = ins.insert(params![
                cid,
                path,
                file.added,
                file.removed,
                if file.truncated { 1 } else { 0 }
            ])?;
            // Index the added+removed text under the hunk's rowid.
            ins_text.execute(params![id, format!("{}\n{}", file.added, file.removed)])?;
            commit_bytes += file.added.len() + file.removed.len();
        }
        mark.execute(params![
            if over_cap {
                DIFF_STATE_SKIPPED
            } else {
                DIFF_STATE_INDEXED
            },
            cid,
            DIFF_STATE_PENDING
        ])?;
        if over_cap {
            // record the over-cap note
            tx.execute(
                "UPDATE commits SET skip_note = 'over-cap' WHERE id = ?1 AND diff_state = ?2",
                params![cid, DIFF_STATE_SKIPPED],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

fn now_iso() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or_else(|_| "0".to_string(), |d| d.as_secs().to_string())
}

fn split_nul_lines(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    text.split('\n').map(ToString::to_string).collect()
}

// ---------------------------------------------------------------------------
// Limits — age window and size budget
// ---------------------------------------------------------------------------

/// Commits evicted between two measures of the database's used size.
const EVICT_STEP: i64 = 25;

/// SQL predicate: commit `c` deleted a path, so its removed text is what
/// excavate restores a dropped file from. Such commits are evicted last.
const RESCUE_PREDICATE: &str =
    "EXISTS (SELECT 1 FROM file_changes fc WHERE fc.commit_id = c.id AND fc.status = 'D')";

/// Unix seconds before which a commit is outside a window of `days` days.
fn window_cutoff(now_unix: i64, days: u64) -> i64 {
    let span = i64::try_from(days)
        .unwrap_or(i64::MAX)
        .saturating_mul(86_400);
    now_unix.saturating_sub(span)
}

/// Drop the diff text of every commit authored before the window, and mark
/// the pending ones so their diff is never fetched. A commit exactly at the
/// cutoff stays. A window widened or lifted since takes back the commits it
/// now covers: they are pending again (a diff evicted for size is not, so a
/// restored diff that does not fit is evicted once more and stays out).
/// Returns the number of commits taken out of the diff index.
pub fn apply_window(store: &mut FactsStore, limits: &HistoryLimits, now_unix: i64) -> Result<u64> {
    let cutoff = limits
        .window_days
        .map_or(i64::MIN, |days| window_cutoff(now_unix, days));
    let restored: Option<i64> = store.conn().query_row(
        "SELECT MIN(id) FROM commits WHERE diff_state = ?1 AND skip_note = ?2
         AND unixepoch(committed_at) >= ?3",
        params![DIFF_STATE_EVICTED, SKIP_NOTE_OUTSIDE_WINDOW, cutoff],
        |r| r.get(0),
    )?;
    if let Some(first) = restored {
        // A widened window can restore IDs behind the saved B cursor. They
        // must pass blob checks before phase C is allowed to read their diffs.
        let cursor = phase_b_cursor(store).min(first - 1).to_string();
        store.conn().execute(
            "UPDATE ingest_jobs SET cursor = ?1, status = 'running' WHERE phase = 'B'",
            [cursor],
        )?;
    }
    store.conn().execute(
        "UPDATE commits SET diff_state = ?1, skip_note = NULL
         WHERE diff_state = ?2 AND skip_note = ?3 AND unixepoch(committed_at) >= ?4",
        params![
            DIFF_STATE_PENDING,
            DIFF_STATE_EVICTED,
            SKIP_NOTE_OUTSIDE_WINDOW,
            cutoff
        ],
    )?;
    let indexed = commits_with_hunks_before(store, cutoff)?;
    let mut evicted = evict_commits(store, &indexed, SKIP_NOTE_OUTSIDE_WINDOW)?;
    // What is left before the cutoff: pending commits, whose diff is never
    // fetched, and indexed commits without a hunk (a binary-only change
    // writes none), which would otherwise stay indexed outside the window
    // and hold `diff_coverage_since` back.
    evicted += store.conn().execute(
        "UPDATE commits SET diff_state = ?1, skip_note = ?2
         WHERE diff_state IN (?3, ?4) AND unixepoch(committed_at) < ?5",
        params![
            DIFF_STATE_EVICTED,
            SKIP_NOTE_OUTSIDE_WINDOW,
            DIFF_STATE_PENDING,
            DIFF_STATE_INDEXED,
            cutoff
        ],
    )? as u64;
    reclaim(store)?;
    Ok(evicted)
}

/// Evict the oldest diffs until the database's used pages fit
/// `budget_bytes`: commits that deleted no path first, then the rescue
/// commits, oldest first within each. Once a commit was evicted for size,
/// every pending non-rescue commit as old or older is marked instead of
/// fetched, since its diff would be the next one out. Metadata stays.
/// Returns the number of commits taken out of the diff index.
pub fn enforce_budget(store: &mut FactsStore, budget_bytes: u64) -> Result<u64> {
    let mut evicted = 0u64;
    let mut floor: Option<i64> = None;
    let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
    // `used_bytes` leaves the free list out, so each step's eviction shows
    // at the next measure without reclaiming the pages first.
    while store.used_bytes()? > budget_bytes {
        // Only commits not yet seen: one coming back means its eviction did
        // not take, and stopping beats spinning on it. No commit left with
        // diff text ends the loop too.
        let victims: Vec<Victim> = oldest_victims(store)?
            .into_iter()
            .filter(|v| seen.insert(v.id))
            .collect();
        if victims.is_empty() {
            break;
        }
        let ids: Vec<i64> = victims.iter().map(|v| v.id).collect();
        evicted += evict_commits(store, &ids, SKIP_NOTE_OVER_BUDGET)?;
        for v in victims.iter().filter(|v| !v.rescue) {
            floor = floor.max(v.authored);
        }
    }
    if let Some(floor) = floor {
        evicted += store.conn().execute(
            &format!(
                "UPDATE commits SET diff_state = ?1, skip_note = ?2
                 WHERE id IN (SELECT c.id FROM commits c
                              WHERE c.diff_state = ?3
                                AND unixepoch(c.committed_at) <= ?4
                                AND NOT {RESCUE_PREDICATE})"
            ),
            params![
                DIFF_STATE_EVICTED,
                SKIP_NOTE_OVER_BUDGET,
                DIFF_STATE_PENDING,
                floor
            ],
        )? as u64;
    }
    reclaim(store)?;
    Ok(evicted)
}

/// A commit holding diff text, as the budget sees it.
struct Victim {
    id: i64,
    /// Authored time in unix seconds; `None` when unparseable.
    authored: Option<i64>,
    rescue: bool,
}

/// The next `EVICT_STEP` commits holding diff text, in eviction order.
fn oldest_victims(store: &FactsStore) -> Result<Vec<Victim>> {
    let mut stmt = store.conn().prepare(&format!(
        "SELECT c.id, unixepoch(c.committed_at), {RESCUE_PREDICATE} AS rescue
         FROM commits c
         WHERE c.diff_state IN (?1, ?2)
           AND EXISTS (SELECT 1 FROM hunks h WHERE h.commit_id = c.id)
         ORDER BY rescue, unixepoch(c.committed_at), c.id
         LIMIT ?3"
    ))?;
    let rows = stmt.query_map(
        params![DIFF_STATE_INDEXED, DIFF_STATE_SKIPPED, EVICT_STEP],
        |r| {
            Ok(Victim {
                id: r.get(0)?,
                authored: r.get(1)?,
                rescue: r.get(2)?,
            })
        },
    )?;
    let mut v = Vec::new();
    for row in rows {
        v.push(row?);
    }
    Ok(v)
}

/// Ids of the commits holding diff text authored before `cutoff`.
fn commits_with_hunks_before(store: &FactsStore, cutoff: i64) -> Result<Vec<i64>> {
    let mut stmt = store.conn().prepare(
        "SELECT c.id FROM commits c
         WHERE c.diff_state IN (?1, ?2)
           AND EXISTS (SELECT 1 FROM hunks h WHERE h.commit_id = c.id)
           AND unixepoch(c.committed_at) < ?3
         ORDER BY c.id",
    )?;
    let rows = stmt.query_map(
        params![DIFF_STATE_INDEXED, DIFF_STATE_SKIPPED, cutoff],
        |r| r.get::<_, i64>(0),
    )?;
    let mut v = Vec::new();
    for row in rows {
        v.push(row?);
    }
    Ok(v)
}

/// Remove the diff text of `ids` (hunks and their index entries) in one
/// transaction and mark each commit evicted with `note`. Returns the count.
fn evict_commits(store: &mut FactsStore, ids: &[i64], note: &str) -> Result<u64> {
    let tx = store.conn_mut().transaction()?;
    {
        let mut drop_text = tx.prepare(
            "DELETE FROM diff_fts WHERE rowid IN (SELECT id FROM hunks WHERE commit_id = ?1)",
        )?;
        let mut drop_hunks = tx.prepare("DELETE FROM hunks WHERE commit_id = ?1")?;
        let mut mark =
            tx.prepare("UPDATE commits SET diff_state = ?1, skip_note = ?2 WHERE id = ?3")?;
        for id in ids {
            drop_text.execute([id])?;
            drop_hunks.execute([id])?;
            mark.execute(params![DIFF_STATE_EVICTED, note, id])?;
        }
    }
    tx.commit()?;
    Ok(ids.len() as u64)
}

/// Hand the pages an eviction freed back to the file system; with an empty
/// free list it costs one statement and frees nothing.
///
/// The pragma frees one page per result row it yields, so it is stepped to
/// the end: `execute_batch` steps once and would free a single page.
fn reclaim(store: &FactsStore) -> Result<()> {
    let mut stmt = store.conn().prepare("PRAGMA incremental_vacuum")?;
    let mut rows = stmt.query([])?;
    while rows.next()?.is_some() {}
    Ok(())
}

pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::history::search::{SearchFacet, search};
    use crate::history::testutil::{
        commit, commit_at, commit_raw_date, days_ago, git, init_repo, two_commit_repo,
    };
    use crate::history::text_index::{CANDIDATE_CAP, matching_hunks};

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(600)
    }

    /// Full ingest with a short cap: a two-commit repo is fresh well under a
    /// second, and a broken phase errors out instead of spinning. The cap
    /// stays under cargo-mutants' automatic timeout (5× the baseline, at
    /// least 20 s) so a hung mutant is reported as caught, not as a timeout.
    pub(crate) const TEST_WALL_CLOCK: Duration = Duration::from_secs(5);

    pub(crate) fn ingest_within(store: &mut FactsStore) -> TickReport {
        ingest_until_fresh_within(store, &IngestOptions::default(), TEST_WALL_CLOCK)
            .expect("ingest until fresh")
    }

    /// A tick budget of zero still lands one batch per phase per tick, so
    /// the loop needs several ticks: the wall-clock cap must let them run
    /// and only fire once it is really exceeded.
    #[test]
    fn full_build_progress_accumulates_timings_and_reports_completed_work() {
        let (dir, _, _) = two_commit_repo();
        let mut store = FactsStore::open(dir.path()).unwrap();
        let options = IngestOptions {
            tick_budget_ms: 0,
            ..IngestOptions::default()
        };
        let mut reports = Vec::new();
        let result =
            ingest_until_fresh_reporting(&mut store, &options, TEST_WALL_CLOCK, |report| {
                reports.push(report.clone()); // Keep owned snapshots after the callback returns.
            })
            .unwrap();
        assert!(reports.len() >= 2, "zero-budget ingest must yield");
        assert_eq!(reports.last(), Some(&result));
        assert_eq!(
            reports[0].timings.diffs,
            Duration::ZERO,
            "phase C has not run yet"
        );
        assert_eq!(reports[0].diff_commits_done, 0);
        assert_eq!(result.blob_commits_done, 2);
        assert_eq!(result.blob_commits_total, 2);
        assert_eq!(result.diff_commits_done, 2);
        assert_eq!(result.diff_commits_total, 2);
        assert!(result.fresh);
        for pair in reports.windows(2) {
            assert!(pair[1].timings.metadata >= pair[0].timings.metadata);
            assert!(pair[1].timings.blobs >= pair[0].timings.blobs);
            assert!(pair[1].timings.diffs >= pair[0].timings.diffs);
            assert!(pair[1].timings.retention >= pair[0].timings.retention);
        }
    }

    #[test]
    fn ingest_until_fresh_within_keeps_ticking_until_fresh_under_a_tiny_tick_budget() {
        let (dir, _, _) = two_commit_repo();
        let mut store = FactsStore::open(dir.path()).unwrap();
        let options = IngestOptions {
            tick_budget_ms: 0,
            ..IngestOptions::default()
        };
        let report = ingest_until_fresh_within(&mut store, &options, TEST_WALL_CLOCK).unwrap();
        assert!(report.fresh, "{report:?}");
        assert_eq!(report.commits_indexed, 2);
    }

    fn commit_id(store: &FactsStore, oid: &str) -> i64 {
        store
            .conn()
            .query_row("SELECT id FROM commits WHERE oid = ?1", [oid], |r| r.get(0))
            .expect("commit row")
    }

    fn hunks_for(store: &FactsStore, oid: &str) -> i64 {
        store
            .conn()
            .query_row(
                "SELECT count(*) FROM hunks WHERE commit_id = (SELECT id FROM commits WHERE oid = ?1)",
                [oid],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// Phase A and B on a fresh store, leaving phase C (diff text) pending.
    fn metadata_only(root: &std::path::Path) -> FactsStore {
        let mut store = FactsStore::open(root).unwrap();
        assert!(phase_a(&mut store, &far()).unwrap());
        assert!(phase_b(&mut store, &far()).unwrap().0);
        store
    }

    /// A repository of `n` commits on one branch, oldest first.
    fn linear_repo(n: usize) -> (tempfile::TempDir, Vec<String>) {
        let dir = init_repo();
        let oids = (0..n)
            .map(|i| {
                let body = format!("fn step_{i}() {{}}\n");
                commit(
                    dir.path(),
                    &[("lib.rs", body.as_bytes())],
                    &format!("step {i}"),
                )
            })
            .collect();
        (dir, oids)
    }

    fn reach_rows(store: &FactsStore) -> Vec<(String, i64)> {
        let mut stmt = store
            .conn()
            .prepare("SELECT oid, bits FROM reach_map ORDER BY oid")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    /// A ref move re-enumerates every commit, but only a commit not yet in
    /// the store gets a `reach_map` row: the insert of a new commit is its
    /// only reader, so a row for a known one is a write for nothing.
    #[test]
    fn enumeration_should_record_reach_only_for_commits_not_yet_indexed() {
        let (dir, oids) = linear_repo(5);
        let mut store = FactsStore::open(dir.path()).unwrap();
        ingest_within(&mut store);
        let new = commit(dir.path(), &[("lib.rs", b"fn moved() {}\n")], "move");

        let known = known_oids(&store);
        let all = enumerate_all_commits(&mut store, &known).unwrap();
        let mut expected = oids;
        expected.push(new.clone());
        assert_eq!(
            all, expected,
            "every commit is still enumerated, oldest first"
        );
        assert_eq!(reach_rows(&store), [(new, REACH_LISTED_REFS)]);
    }

    /// The reach rows of one enumeration land in one transaction: the WAL
    /// grows by a few page frames, not by one commit frame per oid.
    #[test]
    fn enumeration_should_write_reach_map_in_one_transaction() {
        let (dir, oids) = linear_repo(40);
        let mut store = FactsStore::open(dir.path()).unwrap();
        let wal_frames = |store: &FactsStore, mode: &str| -> i64 {
            store
                .conn()
                .query_row(&format!("PRAGMA wal_checkpoint({mode})"), [], |r| r.get(1))
                .unwrap()
        };
        assert_eq!(wal_frames(&store, "TRUNCATE"), 0);

        enumerate_all_commits(&mut store, &std::collections::HashSet::new()).unwrap();
        assert_eq!(reach_rows(&store).len(), oids.len());
        let frames = wal_frames(&store, "PASSIVE");
        assert!(
            frames < 10,
            "{frames} WAL frames for {} rows: one commit per row",
            oids.len()
        );
    }

    /// A repository of `n` commits on `main`, written by one `git fast-import`
    /// (tens of thousands of commits in about a second).
    fn imported_repo(n: usize) -> tempfile::TempDir {
        use std::io::Write as _;
        let dir = init_repo();
        let mut stream = String::new();
        for i in 0..n {
            let body = format!("fn step_{i}() {{}}\n");
            let message = format!("step {i}\n");
            stream.push_str(&format!(
                "commit refs/heads/main\nmark :{}\ncommitter t <t@example.com> {} +0000\ndata {}\n{message}",
                i + 1,
                1_700_000_000 + i,
                message.len()
            ));
            if i > 0 {
                stream.push_str(&format!("from :{i}\n"));
            }
            stream.push_str(&format!(
                "M 644 inline lib.rs\ndata {}\n{body}\n",
                body.len()
            ));
        }
        let mut child = std::process::Command::new("git")
            .args(["fast-import", "--quiet"])
            .current_dir(dir.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stream.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success(), "fast-import");
        git(dir.path(), &["reset", "-q", "--hard", "main"]);
        dir
    }

    /// One 41-byte line per commit put `rev-list --branches` past the
    /// runner's default 1 MiB at about 25 500 commits; the error was taken
    /// for an empty list, and phase A completed with no commit at all.
    #[test]
    fn enumeration_should_list_every_commit_past_the_default_output_cap() {
        const COMMITS: usize = 26_000;
        let dir = imported_repo(COMMITS);
        let mut store = FactsStore::open(dir.path()).unwrap();
        let oids = enumerate_all_commits(&mut store, &std::collections::HashSet::new()).unwrap();
        let listing = git(dir.path(), &["rev-list", "--branches"]);
        assert!(
            listing.len() > pixel_io::DEFAULT_MAX_OUTPUT_BYTES,
            "the fixture must cross the default cap: {} bytes",
            listing.len()
        );
        assert_eq!(oids.len(), COMMITS);
        assert_eq!(
            oids.first(),
            Some(
                &git(dir.path(), &["rev-list", "--max-parents=0", "main"])
                    .trim()
                    .to_string()
            )
        );
    }

    /// A revision git refuses (no `refs/stash`, no reflog) lists nothing; a
    /// capped, timed-out or failed run is an error, never an empty list.
    #[test]
    fn listed_should_tolerate_a_refused_revision_and_report_any_other_failure() {
        let args = vec!["rev-list".to_string()];
        let refused = pixel_io::GitError::NonZeroExit {
            args: args.clone(),
            code: Some(128),
            stderr: "fatal: bad revision 'refs/stash'".into(),
        };
        assert!(listed(Err(refused)).unwrap().is_none());
        assert_eq!(
            listed(Ok(b"abc\n".to_vec())).unwrap(),
            Some(b"abc\n".to_vec())
        );
        let capped = pixel_io::GitError::OutputTooLarge {
            args: args.clone(),
            cap: 1_048_576,
        };
        assert!(listed(Err(capped)).is_err());
        assert!(listed(Err(pixel_io::GitError::Timeout { args })).is_err());
    }

    #[test]
    fn fetch_phase_a_batch_returns_one_parsed_commit_per_requested_oid() {
        let (dir, first, second) = two_commit_repo();
        let store = FactsStore::open(dir.path()).unwrap();
        let oids = vec![second.clone(), first.clone()];
        let (commits, reach) = fetch_phase_a_batch(&store, &oids).unwrap();
        assert_eq!(reach, oids);
        assert_eq!(commits.len(), 2);
        let by_oid = |oid: &str| commits.iter().find(|c| c.oid == oid).expect(oid);
        let c1 = by_oid(&first);
        assert!(c1.parents.is_empty());
        assert_eq!(c1.message, "Add main with hello world greeting");
        assert_eq!(c1.author, "t");
        assert!(c1.committed_at.starts_with("20"), "{}", c1.committed_at);
        assert_eq!(c1.changes.len(), 1);
        assert_eq!(
            (c1.changes[0].status.as_str(), c1.changes[0].path.as_str()),
            ("A", "src/main.rs")
        );
        let c2 = by_oid(&second);
        assert_eq!(c2.parents, vec![first.clone()]);
        assert_eq!(
            (c2.changes[0].status.as_str(), c2.changes[0].path.as_str()),
            ("M", "src/main.rs")
        );
    }

    #[test]
    fn parse_phase_a_keeps_a_record_with_no_changes_and_drops_a_short_one() {
        let bare = b"\x1eabc\x00\x00Ann\x002026-01-01T00:00:00Z\x00subject

body
";
        let parsed = parse_phase_a(bare);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].oid, "abc");
        assert!(parsed[0].parents.is_empty());
        assert_eq!(
            parsed[0].message,
            "subject

body"
        );
        assert!(parsed[0].changes.is_empty());
        assert!(parse_phase_a(b"\x1eabc\x00\x00Ann\x00").is_empty());
        assert!(parse_phase_a(b"").is_empty());
    }

    #[test]
    fn parse_phase_a_should_skip_blank_status_fields_and_pair_renames() {
        let rec = b"\x1eabc\x00p1 p2\x00Ann\x00t\x00msg\x00\x00 \x00M\x00a.txt\x00R100\x00old.rs\x00new.rs\x00D\x00gone";
        let parsed = parse_phase_a(rec);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].parents, vec!["p1", "p2"]);
        let changes: Vec<(String, String, Option<String>)> = parsed[0]
            .changes
            .iter()
            .map(|c| (c.status.clone(), c.path.clone(), c.old_path.clone()))
            .collect();
        assert_eq!(
            changes,
            vec![
                ("M".to_string(), "a.txt".to_string(), None),
                (
                    "R".to_string(),
                    "new.rs".to_string(),
                    Some("old.rs".to_string())
                ),
                ("D".to_string(), "gone".to_string(), None),
            ]
        );
        // A truncated trailing change is dropped, the earlier ones kept.
        let cut = b"\x1eabc\x00\x00Ann\x00t\x00msg\x00M\x00a.txt\x00R100\x00old.rs";
        assert_eq!(parse_phase_a(cut)[0].changes.len(), 1);
        let cut = b"\x1eabc\x00\x00Ann\x00t\x00msg\x00M\x00a.txt\x00M";
        assert_eq!(parse_phase_a(cut)[0].changes.len(), 1);
    }

    #[test]
    fn parse_phase_c_should_drop_the_text_of_a_binary_file() {
        for marker in ["Binary files a/x and b/x differ", "GIT binary patch"] {
            let out = format!("\x1eabc\ndiff --git a/x b/x\n+text\n-gone\n{marker}\n");
            let parsed = parse_phase_c(out.as_bytes());
            assert_eq!(parsed[0].files.len(), 1, "{marker}");
            assert_eq!(parsed[0].files[0].path, "x");
            assert_eq!(parsed[0].files[0].added, "", "{marker}");
            assert_eq!(parsed[0].files[0].removed, "", "{marker}");
        }
    }

    #[test]
    fn ingest_until_fresh_bounded_should_return_at_once_when_fresh() {
        let (dir, _, _) = two_commit_repo();
        let mut store = FactsStore::open(dir.path()).unwrap();
        ingest_within(&mut store);
        let start = Instant::now();
        let report = ingest_until_fresh_bounded(&mut store, 8_000).unwrap();
        assert!(report.fresh, "{report:?}");
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn a_fresh_index_should_need_no_phase_a_and_record_its_reach() {
        let (dir, _, _) = two_commit_repo();
        let mut store = FactsStore::open(dir.path()).unwrap();
        ingest_within(&mut store);
        assert!(!needs_phase_a(&store).unwrap());
        let reach: Vec<i64> = {
            let mut stmt = store.conn().prepare("SELECT reach FROM commits").unwrap();
            let rows = stmt.query_map([], |r| r.get(0)).unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        assert_eq!(reach, vec![REACH_LISTED_REFS; 2]);
        assert_eq!(
            REACH_LISTED_REFS,
            crate::history::store::REACH_BRANCH
                | crate::history::store::REACH_REMOTE
                | crate::history::store::REACH_TAG
        );
    }

    #[test]
    fn size_only_batch_resolves_revisions_and_missing_paths_without_reading_content() {
        let dir = init_repo();
        let first = commit(dir.path(), &[("a", b"small")], "small");
        let second = commit(
            dir.path(),
            &[("a", &vec![b'x'; BLOB_CAP_BYTES + 1]), ("dir/file", b"x")],
            "large",
        );
        let specs = [
            format!("{first}:a"),
            format!("{second}:a"),
            format!("{second}:missing"),
            format!("{second}:dir"),
            second,
        ];
        let refs: Vec<&str> = specs.iter().map(String::as_str).collect();
        // The response fits in 512 bytes although the blob is over the cap.
        let runner = pixel_io::GitRunner::with_options(
            dir.path(),
            GitOptions {
                max_output_bytes: Some(512),
                ..GitOptions::default()
            },
        );
        assert_eq!(
            batch_blob_sizes(&runner, &refs),
            Some(vec![5, (BLOB_CAP_BYTES + 1) as u64, 0, 0, 0])
        );
        assert_eq!(batch_blob_sizes(&runner, &[]), Some(vec![]));
        assert_eq!(batch_blob_sizes(&runner, &["HEAD:line\nname"]), None);
        assert_eq!(batch_blob_sizes(&runner, &["HEAD:carriage\r"]), None);
        let capped = pixel_io::GitRunner::with_options(
            dir.path(),
            GitOptions {
                max_output_bytes: Some(1),
                ..GitOptions::default()
            },
        );
        assert_eq!(batch_blob_sizes(&capped, &refs), None);
    }

    #[test]
    fn cross_commit_batch_keeps_poison_detection_for_unrepresentable_paths() {
        let dir = init_repo();
        let first = commit(
            dir.path(),
            &[("first", &vec![b'x'; BLOB_CAP_BYTES + 1])],
            "large",
        );
        let second = commit(
            dir.path(),
            &[("line\nname", &vec![b'y'; BLOB_CAP_BYTES + 1])],
            "unusual large",
        );
        let mut store = FactsStore::open(dir.path()).unwrap();
        assert!(phase_a(&mut store, &far()).unwrap());
        let ids = [commit_id(&store, &first), commit_id(&store, &second)];
        assert_eq!(measure_commits_blobs(&mut store, &ids).unwrap(), 2);
        assert_eq!(store.poison_paths().unwrap(), vec!["first", "line\nname"]);
    }

    #[test]
    fn phase_b_checkpoints_a_bounded_commit_batch_before_yielding() {
        let dir = init_repo();
        for i in 0..PHASE_B_BATCH + 1 {
            commit(dir.path(), &[("a", i.to_string().as_bytes())], "change");
        }
        let mut store = FactsStore::open(dir.path()).unwrap();
        assert!(phase_a(&mut store, &far()).unwrap());
        let batch = next_phase_b_commits(&store, 0).unwrap();
        assert_eq!(batch.len(), PHASE_B_BATCH);
        assert_eq!(phase_b(&mut store, &Instant::now()).unwrap(), (false, 0));
        assert_eq!(phase_b_cursor(&store), *batch.last().unwrap());
        assert_eq!(
            next_phase_b_commits(&store, phase_b_cursor(&store))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(phase_b(&mut store, &far()).unwrap(), (true, 0));
    }

    #[test]
    fn expired_history_skips_blob_checks_but_a_wider_window_rewinds_them() {
        let dir = init_repo();
        let old = commit_at(
            dir.path(),
            &[("big", &vec![b'x'; BLOB_CAP_BYTES + 1])],
            "old large",
            days_ago(400),
        );
        commit_at(dir.path(), &[("recent", b"small")], "recent", days_ago(1));
        let mut store = FactsStore::open(dir.path()).unwrap();
        ingest_with(
            &mut store,
            HistoryLimits {
                window_days: Some(365),
                ..no_limits()
            },
        );
        assert!(
            store.poison_paths().unwrap().is_empty(),
            "expired changes must not be measured"
        );
        assert_eq!(
            state_of(&store, &old),
            (
                DIFF_STATE_EVICTED,
                Some(SKIP_NOTE_OUTSIDE_WINDOW.to_owned())
            )
        );
        let cursor = phase_b_cursor(&store);
        assert!(cursor >= commit_id(&store, &old));
        drop(store);
        let mut store = FactsStore::open(dir.path()).unwrap();
        ingest_with(&mut store, no_limits());
        assert_eq!(store.poison_paths().unwrap(), vec!["big"]);
        assert_eq!(hunks_for(&store, &old), 0);
        assert_eq!(state_of(&store, &old).0, DIFF_STATE_SKIPPED);
    }

    #[test]
    fn retained_changes_check_large_parents_even_when_the_parent_commit_expired() {
        for (path, operation) in [
            ("large", "modify"),
            ("large", "delete"),
            ("line\nname", "modify"),
            ("line\nname", "delete"),
            ("large", "rename"),
        ] {
            let dir = init_repo();
            let old = commit_at(
                dir.path(),
                &[(path, &vec![b'x'; BLOB_CAP_BYTES + 1])],
                "old large",
                days_ago(400),
            );
            let target = if operation == "rename" {
                "renamed"
            } else {
                path
            };
            if operation == "delete" {
                git(dir.path(), &["rm", "--", path]);
            }
            if operation == "rename" {
                git(dir.path(), &["mv", "--", path, target]);
            }
            let recent = if operation == "delete" {
                commit_at(dir.path(), &[], "delete", days_ago(1))
            } else {
                commit_at(
                    dir.path(),
                    &[(target, &vec![b'x'; BLOB_CAP_BYTES])],
                    "trim below poison threshold",
                    days_ago(1),
                )
            };
            let mut store = FactsStore::open(dir.path()).unwrap();
            ingest_with(
                &mut store,
                HistoryLimits {
                    window_days: Some(365),
                    ..no_limits()
                },
            );
            assert_eq!(
                store.poison_paths().unwrap(),
                vec![target],
                "{operation} {path:?}"
            );
            assert_eq!(
                hunks_for(&store, &recent),
                0,
                "large removals must never reach the diff index"
            );
            assert_eq!(state_of(&store, &old).0, DIFF_STATE_EVICTED);
        }
    }

    #[test]
    fn phase_b_should_count_the_paths_it_poisons() {
        let dir = init_repo();
        commit(
            dir.path(),
            &[("big.bin", &vec![b'x'; BLOB_CAP_BYTES + 1])],
            "big",
        );
        let mut store = FactsStore::open(dir.path()).unwrap();
        assert!(phase_a(&mut store, &far()).unwrap());
        assert_eq!(phase_b(&mut store, &far()).unwrap(), (true, 1));
    }

    #[test]
    fn ingest_diff_single_should_index_a_commit_just_under_its_output_cap() {
        let dir = init_repo();
        let line = format!("{}\n", "x".repeat(99));
        let body = line.repeat(287).into_bytes();
        let names: Vec<String> = (0..10).map(|n| format!("f{n}.txt")).collect();
        let files: Vec<(&str, &[u8])> = names
            .iter()
            .map(|n| (n.as_str(), body.as_slice()))
            .collect();
        let oid = commit(dir.path(), &files, "ten files");
        let shown = git(
            dir.path(),
            &["show", "-U0", "--no-color", "--format=%x1e%H", &oid],
        );
        let cap = COMMIT_TEXT_CAP_BYTES + FILE_TEXT_CAP_BYTES;
        assert!(
            shown.len() > cap - 4096 && shown.len() < cap,
            "{}",
            shown.len()
        );
        let mut store = metadata_only(dir.path());
        ingest_diff_single(&mut store, &oid).unwrap();
        assert!(hunks_for(&store, &oid) >= 1);
    }

    #[test]
    fn ingest_until_fresh_reports_a_fresh_index_with_every_commit_counted() {
        let (dir, _, _) = two_commit_repo();
        let mut store = FactsStore::open(dir.path()).unwrap();
        let report = ingest_within(&mut store);
        assert!(report.fresh, "{report:?}");
        assert_eq!(report.total_commits, 2);
        assert_eq!(report.commits_indexed, 2);
    }

    #[test]
    fn ingest_diff_batch_indexes_the_hunks_and_counts_skipped_poison_paths() {
        let (dir, first, second) = two_commit_repo();
        let mut store = metadata_only(dir.path());
        let pending = pending_phase_c(&store).unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(ingest_diff_batch(&mut store, &pending).unwrap(), (0, 0));
        assert!(hunks_for(&store, &first) >= 1);
        assert!(hunks_for(&store, &second) >= 1);
        assert!(pending_phase_c(&store).unwrap().is_empty());

        // A poisoned path is excluded from git's output and counted as skipped.
        let dir = init_repo();
        let oid = commit(
            dir.path(),
            &[
                (
                    "gen.lock", b"x
",
                ),
                (
                    "a.txt", b"y
",
                ),
            ],
            "two files",
        );
        let mut store = metadata_only(dir.path());
        store.learn_poison("gen.lock", "test").unwrap();
        let pending = pending_phase_c(&store).unwrap();
        assert_eq!(ingest_diff_batch(&mut store, &pending).unwrap(), (0, 1));
        let paths: Vec<String> = {
            let mut stmt = store
                .conn()
                .prepare("SELECT path FROM hunks WHERE commit_id = (SELECT id FROM commits WHERE oid = ?1)")
                .unwrap();
            let rows = stmt.query_map([&oid], |r| r.get::<_, String>(0)).unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        assert_eq!(paths, vec!["a.txt".to_string()]);
    }

    #[test]
    fn ingest_diff_single_indexes_one_commit_and_marks_it_done() {
        let (dir, first, second) = two_commit_repo();
        let mut store = metadata_only(dir.path());
        ingest_diff_single(&mut store, &second).unwrap();
        assert!(hunks_for(&store, &second) >= 1);
        assert_eq!(hunks_for(&store, &first), 0);
        assert_eq!(
            pending_phase_c(&store).unwrap(),
            vec![commit_id(&store, &first)]
        );
    }

    #[test]
    fn now_iso_is_the_current_unix_epoch_in_seconds() {
        let ts: i64 = now_iso().parse().expect("digits");
        assert!(ts > 1_577_836_800, "{ts}"); // 2020-01-01T00:00:00Z
    }

    #[test]
    fn split_nul_lines_splits_on_newlines_keeping_the_trailing_empty_line() {
        assert_eq!(
            split_nul_lines(
                b"a
b
"
            ),
            vec!["a", "b", ""]
        );
        assert_eq!(split_nul_lines(b""), vec![""]);
    }

    #[test]
    fn parse_ls_tree_size_reads_the_blob_size_and_ignores_other_entries() {
        assert_eq!(
            parse_ls_tree_size(
                "100644 blob 71f83363ae56390921b5f7cdc6c6bf89561bfefb    4241\tCargo.toml\n"
            ),
            4241
        );
        assert_eq!(
            parse_ls_tree_size(
                "040000 tree 3d9efb2fd665a069e066cf78245c2423a0c23eff       -\tcrates\n"
            ),
            0
        );
        assert_eq!(parse_ls_tree_size(""), 0);
        assert_eq!(parse_ls_tree_size("100644 blob abc"), 0);
    }

    #[test]
    fn blob_size_reads_the_tree_of_the_requested_revision() {
        let dir = init_repo();
        let root = dir.path();
        let first = commit(root, &[("a.txt", b"hello\n")], "five bytes plus newline");
        let second = commit(
            root,
            &[("a.txt", b"hello world\n"), ("dir/b.txt", b"x")],
            "grown",
        );
        let runner = pixel_io::GitRunner::with_options(root, GitOptions::default());
        assert_eq!(blob_size(&runner, &first, "a.txt"), 6);
        assert_eq!(blob_size(&runner, &second, "a.txt"), 12);
        assert_eq!(blob_size(&runner, &second, "dir/b.txt"), 1);
        assert_eq!(
            blob_size(&runner, &second, "dir"),
            0,
            "a tree is not a blob"
        );
        assert_eq!(blob_size(&runner, &first, "missing.txt"), 0);
        assert_eq!(blob_size(&runner, "not-a-rev", "a.txt"), 0);
    }

    #[test]
    fn batched_blob_sizes_preserve_exact_paths_across_batches_and_revisions() {
        let dir = init_repo();
        let root = dir.path();
        let mut names: Vec<String> = (0..TREE_BATCH_PATHS + 1)
            .map(|i| format!("dir/file-{i}"))
            .collect();
        names.extend(
            [
                "space name",
                "tab\tname",
                "line\nname",
                "literal*",
                "literal-other",
                "-dash",
            ]
            .map(str::to_owned),
        );
        let contents: Vec<Vec<u8>> = names
            .iter()
            .enumerate()
            .map(|(i, _)| vec![b'x'; i + 1])
            .collect();
        let files: Vec<(&str, &[u8])> = names
            .iter()
            .zip(&contents)
            .map(|(p, bytes)| (p.as_str(), bytes.as_slice()))
            .collect();
        let first = commit(root, &files, "distinct sizes including unusual paths");
        git(root, &["rm", "--", "space name"]);
        let second = commit(root, &[("literal*", b"updated")], "delete and change");
        let mut paths: Vec<&str> = names.iter().map(String::as_str).collect();
        paths.extend(["missing", "dir"]);
        let runner = pixel_io::GitRunner::with_options(root, GitOptions::default());
        let before = blob_sizes(&runner, &first, &paths);
        let after = blob_sizes(&runner, &second, &paths);
        for (i, name) in names.iter().enumerate() {
            assert_eq!(before.get(name).copied(), Some((i + 1) as u64), "{name:?}");
            let expected = match name.as_str() {
                "space name" => None,
                "literal*" => Some(7),
                _ => Some((i + 1) as u64),
            };
            assert_eq!(after.get(name).copied(), expected, "{name:?}");
        }
        assert_eq!(before.get("dir").copied().unwrap_or(0), 0);
        assert_eq!(before.get("missing"), None);
        assert!(blob_sizes(&runner, &first, &[]).is_empty());
        assert_eq!(
            blob_sizes(&runner, "missing-rev", &["literal*"]).get("literal*"),
            Some(&0)
        );
    }

    #[test]
    fn capped_tree_batch_falls_back_without_losing_blob_sizes() {
        let dir = init_repo();
        let root = dir.path();
        let oid = commit(root, &[("a", b"short"), ("b", b"longer")], "two blobs");
        // Each record fits separately, but their combined output exceeds 90B.
        let runner = pixel_io::GitRunner::with_options(
            root,
            GitOptions {
                max_output_bytes: Some(90),
                ..GitOptions::default()
            },
        );
        let sizes = blob_sizes(&runner, &oid, &["a", "b"]);
        assert_eq!(sizes.get("a"), Some(&5));
        assert_eq!(sizes.get("b"), Some(&6));
    }

    #[test]
    fn measure_commit_blobs_poisons_only_paths_strictly_over_the_cap() {
        let dir = init_repo();
        let root = dir.path();
        let small = commit(root, &[("a.txt", b"hello\n")], "small");
        let edge = commit(
            root,
            &[("edge.bin", &vec![b'x'; BLOB_CAP_BYTES])],
            "at the cap",
        );
        let big = commit(
            root,
            &[("big.bin", &vec![b'x'; BLOB_CAP_BYTES + 1])],
            "over the cap",
        );
        let mut store = FactsStore::open(root).unwrap();
        assert!(phase_a(&mut store, &far()).unwrap());
        let (small, edge, big) = (
            commit_id(&store, &small),
            commit_id(&store, &edge),
            commit_id(&store, &big),
        );
        assert_eq!(measure_commit_blobs(&mut store, small).unwrap(), 0);
        assert_eq!(measure_commit_blobs(&mut store, edge).unwrap(), 0);
        assert_eq!(measure_commit_blobs(&mut store, big).unwrap(), 1);
        assert_eq!(store.poison_paths().unwrap(), vec!["big.bin".to_string()]);
    }

    /// A rename whose old blob was over the cap carries that blob's removal
    /// in its diff, so the path is poison even when the new blob fits.
    #[test]
    fn measure_commit_blobs_poisons_a_rename_whose_old_blob_was_over_the_cap() {
        let dir = init_repo();
        let root = dir.path();
        commit(
            root,
            &[("big.bin", &vec![b'x'; BLOB_CAP_BYTES + 1])],
            "over the cap",
        );
        git(root, &["mv", "big.bin", "moved.bin"]);
        let renamed = commit(
            root,
            &[("moved.bin", &vec![b'x'; BLOB_CAP_BYTES])],
            "rename and trim",
        );
        let mut store = FactsStore::open(root).unwrap();
        assert!(phase_a(&mut store, &far()).unwrap());
        assert_eq!(
            parent_path_for(&store, &renamed, "moved.bin")
                .map(|(_, old)| old)
                .as_deref(),
            Some("big.bin"),
            "git must report the rename"
        );
        let cid = commit_id(&store, &renamed);
        assert_eq!(measure_commit_blobs(&mut store, cid).unwrap(), 1);
        assert!(
            store
                .poison_paths()
                .unwrap()
                .contains(&"moved.bin".to_string())
        );

        // An old blob exactly at the cap is not over it.
        let dir = init_repo();
        let root = dir.path();
        commit(
            root,
            &[("edge.bin", &vec![b'y'; BLOB_CAP_BYTES])],
            "at the cap",
        );
        git(root, &["mv", "edge.bin", "edge2.bin"]);
        let renamed = commit(
            root,
            &[("edge2.bin", &vec![b'y'; BLOB_CAP_BYTES - 1])],
            "rename and trim",
        );
        let mut store = FactsStore::open(root).unwrap();
        assert!(phase_a(&mut store, &far()).unwrap());
        assert_eq!(
            parent_path_for(&store, &renamed, "edge2.bin")
                .map(|(_, old)| old)
                .as_deref(),
            Some("edge.bin")
        );
        let cid = commit_id(&store, &renamed);
        assert_eq!(measure_commit_blobs(&mut store, cid).unwrap(), 0);
        assert!(store.poison_paths().unwrap().is_empty());
    }

    /// End to end: the over-cap blob's diff never reaches the index, the
    /// small file's does.
    #[test]
    fn ingest_skips_the_diff_of_an_over_cap_blob() {
        let dir = init_repo();
        let root = dir.path();
        commit(
            root,
            &[
                ("big.bin", &vec![b'x'; BLOB_CAP_BYTES + 1]),
                ("a.txt", b"needle_in_small\n"),
            ],
            "one big one small",
        );
        let mut store = FactsStore::open(root).unwrap();
        let report = ingest_within(&mut store);
        assert!(report.fresh, "{report:?}");
        assert_eq!(store.poison_paths().unwrap(), vec!["big.bin".to_string()]);
        let paths: Vec<String> = {
            let mut stmt = store.conn().prepare("SELECT path FROM hunks").unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        assert_eq!(paths, vec!["a.txt".to_string()]);
    }

    pub(crate) fn no_limits() -> HistoryLimits {
        HistoryLimits {
            budget_bytes: u64::MAX,
            window_days: None,
        }
    }

    pub(crate) fn ingest_with(store: &mut FactsStore, limits: HistoryLimits) -> TickReport {
        let options = IngestOptions {
            limits,
            ..IngestOptions::default()
        };
        ingest_until_fresh_within(store, &options, TEST_WALL_CLOCK).expect("ingest until fresh")
    }

    fn state_of(store: &FactsStore, oid: &str) -> (i64, Option<String>) {
        store
            .conn()
            .query_row(
                "SELECT diff_state, skip_note FROM commits WHERE oid = ?1",
                [oid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }

    fn diff_hits(store: &FactsStore, needle: &str) -> usize {
        matching_hunks(store.conn(), &[needle.to_string()], CANDIDATE_CAP)
            .unwrap()
            .unwrap()
            .len()
    }

    /// A file without diff text (binary here) leaves no hunk row: every
    /// stored hunk has text, which is what `doctor`'s poison check reads.
    #[test]
    fn ingest_writes_no_hunk_row_for_a_file_without_text() {
        let dir = init_repo();
        let root = dir.path();
        let oid = commit(
            root,
            &[
                ("logo.bin", b"\x00\x01\x02binary\x00"),
                ("a.txt", b"words\n"),
            ],
            "binary and text",
        );
        let mut store = FactsStore::open(root).unwrap();
        ingest_within(&mut store);
        assert_eq!(state_of(&store, &oid), (DIFF_STATE_INDEXED, None));
        let paths: Vec<(String, i64)> = {
            let mut stmt = store
                .conn()
                .prepare("SELECT path, length(added) + length(removed) FROM hunks")
                .unwrap();
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        assert_eq!(paths, vec![("a.txt".to_string(), 6)]);
    }

    /// The mark moves with each tick's progress and holds still otherwise;
    /// `ingest_until_fresh_within` reads a mark that held still as a stall.
    #[test]
    fn progress_mark_moves_with_a_tick_and_holds_still_without_one() {
        let (dir, _, _) = two_commit_repo();
        let mut store = FactsStore::open(dir.path()).unwrap();
        let empty = progress_mark(&store).unwrap();
        assert_eq!(empty, "0/0/");
        assert_eq!(progress_mark(&store).unwrap(), empty, "nothing ran");
        let options = IngestOptions {
            tick_budget_ms: 0,
            ..IngestOptions::default()
        };
        ingest_tick(&mut store, &options).unwrap();
        let after = progress_mark(&store).unwrap();
        assert_ne!(after, empty);
        assert!(after.starts_with("2/"), "{after}");
    }

    #[test]
    fn now_unix_reads_the_clock_in_seconds() {
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let got = now_unix();
        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(
            got >= before && got <= after,
            "{before} <= {got} <= {after}"
        );
        assert!(got > 1_577_836_800, "{got}"); // 2020-01-01T00:00:00Z
    }

    #[test]
    fn window_cutoff_is_now_minus_the_window_in_seconds() {
        assert_eq!(window_cutoff(1_000_000, 1), 913_600);
        assert_eq!(window_cutoff(1_000_000, 0), 1_000_000);
        assert_eq!(
            window_cutoff(0, u64::MAX),
            -i64::MAX,
            "saturates, never wraps"
        );
    }

    /// The diffs a budget keeps are the recent ones, so phase C fetches
    /// the newest commit first.
    #[test]
    fn pending_phase_c_lists_the_newest_commit_first() {
        let dir = init_repo();
        let root = dir.path();
        let old = commit_at(root, &[("a.txt", b"a\n")], "old", days_ago(3));
        let mid = commit_at(root, &[("b.txt", b"b\n")], "mid", days_ago(2));
        let new = commit_at(root, &[("c.txt", b"c\n")], "new", days_ago(1));
        let store = metadata_only(root);
        let ids: Vec<i64> = [&new, &mid, &old]
            .iter()
            .map(|oid| commit_id(&store, oid))
            .collect();
        assert_eq!(pending_phase_c(&store).unwrap(), ids);
    }

    /// A commit older than the window keeps its message and paths but never
    /// has its diff fetched; the index state says how many and since when.
    #[test]
    fn ingest_leaves_the_diff_of_a_commit_older_than_the_window_out() {
        let dir = init_repo();
        let root = dir.path();
        let old = commit_at(
            root,
            &[("old.txt", b"ancient_marker\n")],
            "ancient work",
            days_ago(400),
        );
        let new = commit_at(
            root,
            &[("new.txt", b"recent_marker\n")],
            "recent work",
            days_ago(1),
        );
        let mut store = FactsStore::open(root).unwrap();
        let report = ingest_with(
            &mut store,
            HistoryLimits {
                window_days: Some(365),
                ..no_limits()
            },
        );
        assert!(report.fresh, "{report:?}");
        assert_eq!(
            state_of(&store, &old),
            (
                DIFF_STATE_EVICTED,
                Some(SKIP_NOTE_OUTSIDE_WINDOW.to_string())
            )
        );
        assert_eq!(hunks_for(&store, &old), 0, "never fetched");
        assert_eq!(state_of(&store, &new), (DIFF_STATE_INDEXED, None));
        assert_eq!(diff_hits(&store, "ancient_marker"), 0);
        assert_eq!(diff_hits(&store, "recent_marker"), 1);
        let by_message = search(&store, "ancient", SearchFacet::Message, 10).unwrap();
        assert_eq!(by_message.candidates.len(), 1, "metadata stays searchable");
        let state = store.index_state();
        assert_eq!(state.diffs_evicted, 1);
        let new_at: String = store
            .conn()
            .query_row(
                "SELECT committed_at FROM commits WHERE oid = ?1",
                [&new],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state.diff_coverage_since, Some(new_at));
    }

    /// Issue #512: a commit whose git object holds an offset SQLite cannot
    /// read (`+51800`, from psf/requests) was stored verbatim, so
    /// `unixepoch(committed_at)` was NULL, the window never took it out and
    /// `diff_coverage_since` claimed its 2011 date. Stored as the same
    /// instant in UTC, it ages out like any other commit.
    #[test]
    fn a_commit_with_an_unreadable_offset_ages_out_of_the_window() {
        let dir = init_repo();
        let root = dir.path();
        let base = commit_at(root, &[("a.rs", b"a\n")], "base", days_ago(30));
        let garbled = commit_raw_date(
            root,
            &[("b.rs", b"b\n")],
            "malformed offset",
            "1313584730 +51800",
        );
        let recent = commit_at(root, &[("c.rs", b"c\n")], "recent", days_ago(1));
        assert_eq!(
            git(root, &["log", "-1", "--format=%aI", &garbled]),
            "2011-09-08T02:38:50+518:00"
        );
        let mut store = FactsStore::open(root).unwrap();
        ingest_with(
            &mut store,
            HistoryLimits {
                window_days: Some(365),
                ..no_limits()
            },
        );
        let stored: String = store
            .conn()
            .query_row(
                "SELECT committed_at FROM commits WHERE oid = ?1",
                [&garbled],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, "2011-08-17T12:38:50+00:00");
        assert_eq!(
            state_of(&store, &garbled),
            (
                DIFF_STATE_EVICTED,
                Some(SKIP_NOTE_OUTSIDE_WINDOW.to_string())
            )
        );
        assert_eq!(state_of(&store, &recent), (DIFF_STATE_INDEXED, None));
        let state = store.index_state();
        assert_eq!(state.diffs_evicted, 1);
        let base_at: String = store
            .conn()
            .query_row(
                "SELECT committed_at FROM commits WHERE oid = ?1",
                [&base],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state.diff_coverage_since, Some(base_at));
    }

    /// The window also drops diffs that were indexed while still inside
    /// it, index entries included; a commit exactly at the cutoff stays.
    #[test]
    fn apply_window_evicts_indexed_diffs_that_aged_out_and_keeps_the_cutoff() {
        let dir = init_repo();
        let root = dir.path();
        let t = 1_700_000_000;
        let older = commit_at(root, &[("a.txt", b"older_marker\n")], "older", t - 1);
        let edge = commit_at(root, &[("b.txt", b"edge_marker\n")], "edge", t);
        let mut store = FactsStore::open(root).unwrap();
        ingest_with(&mut store, no_limits());
        assert_eq!(diff_hits(&store, "older_marker"), 1);
        let limits = HistoryLimits {
            window_days: Some(1),
            ..no_limits()
        };
        assert_eq!(apply_window(&mut store, &limits, t + 86_400).unwrap(), 1);
        assert_eq!(
            state_of(&store, &older),
            (
                DIFF_STATE_EVICTED,
                Some(SKIP_NOTE_OUTSIDE_WINDOW.to_string())
            )
        );
        assert_eq!(hunks_for(&store, &older), 0);
        assert_eq!(
            diff_hits(&store, "older_marker"),
            0,
            "index entries dropped too"
        );
        assert_eq!(state_of(&store, &edge), (DIFF_STATE_INDEXED, None));
        assert_eq!(diff_hits(&store, "edge_marker"), 1);
        let unlimited = no_limits();
        assert_eq!(
            apply_window(&mut store, &unlimited, t + 10 * 86_400).unwrap(),
            0
        );
        assert_eq!(state_of(&store, &edge), (DIFF_STATE_INDEXED, None));
    }

    /// Widening the window takes back the commits it now covers, lifting it
    /// takes back all of them, and a commit evicted for size stays out.
    #[test]
    fn apply_window_restores_commits_a_wider_or_lifted_window_covers() {
        let dir = init_repo();
        let root = dir.path();
        let t = 1_700_000_000;
        let old = commit_at(root, &[("a.txt", b"a\n")], "old", t - 10 * 86_400);
        let mid = commit_at(root, &[("b.txt", b"b\n")], "mid", t - 3 * 86_400);
        let sized = commit_at(root, &[("c.txt", b"c\n")], "sized", t - 3 * 86_400);
        let mut store = metadata_only(root);
        store
            .conn()
            .execute(
                "UPDATE commits SET diff_state = ?1, skip_note = ?2 WHERE oid = ?3",
                params![DIFF_STATE_EVICTED, SKIP_NOTE_OVER_BUDGET, sized],
            )
            .unwrap();
        let window = |days| HistoryLimits {
            window_days: Some(days),
            ..no_limits()
        };
        assert_eq!(apply_window(&mut store, &window(1), t).unwrap(), 2);
        let outside = (
            DIFF_STATE_EVICTED,
            Some(SKIP_NOTE_OUTSIDE_WINDOW.to_string()),
        );
        assert_eq!(state_of(&store, &mid), outside);

        assert_eq!(apply_window(&mut store, &window(5), t).unwrap(), 0);
        assert_eq!(
            state_of(&store, &mid),
            (DIFF_STATE_PENDING, None),
            "back inside"
        );
        assert_eq!(state_of(&store, &old), outside, "still outside");

        assert_eq!(apply_window(&mut store, &no_limits(), t).unwrap(), 0);
        assert_eq!(
            state_of(&store, &old),
            (DIFF_STATE_PENDING, None),
            "window lifted"
        );
        assert_eq!(
            state_of(&store, &sized),
            (DIFF_STATE_EVICTED, Some(SKIP_NOTE_OVER_BUDGET.to_string())),
            "a size eviction is not the window's to undo"
        );
    }

    /// A binary-only commit is indexed without a hunk; once it ages out it
    /// is marked outside the window like the others, so the coverage date
    /// moves with the window.
    #[test]
    fn apply_window_ages_out_an_indexed_commit_without_hunks() {
        let dir = init_repo();
        let root = dir.path();
        let t = 1_700_000_000;
        let binary = commit_at(root, &[("logo.bin", b"\x00\x01binary\x00")], "logo", t - 1);
        let text = commit_at(root, &[("a.txt", b"words\n")], "text", t);
        let mut store = FactsStore::open(root).unwrap();
        ingest_with(&mut store, no_limits());
        assert_eq!(state_of(&store, &binary), (DIFF_STATE_INDEXED, None));
        assert_eq!(hunks_for(&store, &binary), 0);
        let limits = HistoryLimits {
            window_days: Some(1),
            ..no_limits()
        };
        assert_eq!(apply_window(&mut store, &limits, t + 86_400).unwrap(), 1);
        assert_eq!(
            state_of(&store, &binary),
            (
                DIFF_STATE_EVICTED,
                Some(SKIP_NOTE_OUTSIDE_WINDOW.to_string())
            )
        );
        assert_eq!(state_of(&store, &text), (DIFF_STATE_INDEXED, None));
        let text_at: String = store
            .conn()
            .query_row(
                "SELECT committed_at FROM commits WHERE oid = ?1",
                [&text],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(store.index_state().diff_coverage_since, Some(text_at));
    }

    /// Pending commits before the cutoff are marked, the one at it is not.
    #[test]
    fn apply_window_marks_pending_commits_before_the_cutoff() {
        let dir = init_repo();
        let root = dir.path();
        let t = 1_700_000_000;
        let older = commit_at(root, &[("a.txt", b"a\n")], "older", t - 1);
        let edge = commit_at(root, &[("b.txt", b"b\n")], "edge", t);
        let mut store = metadata_only(root);
        let limits = HistoryLimits {
            window_days: Some(1),
            ..no_limits()
        };
        assert_eq!(apply_window(&mut store, &limits, t + 86_400).unwrap(), 1);
        assert_eq!(
            state_of(&store, &older),
            (
                DIFF_STATE_EVICTED,
                Some(SKIP_NOTE_OUTSIDE_WINDOW.to_string())
            )
        );
        assert_eq!(state_of(&store, &edge), (DIFF_STATE_PENDING, None));
    }

    /// `marker` on the first line, then ~16 KiB of distinct lines: enough
    /// diff text per commit to fill whole pages, so evicting it frees them.
    fn bulky(marker: &str) -> Vec<u8> {
        let mut text = format!("{marker}\n");
        for i in 0..600 {
            text.push_str(&format!("{marker} line {i} of filler text\n"));
        }
        text.into_bytes()
    }

    /// Four commits with diff text: two that only add or modify, a rescue
    /// commit (it deletes a path) and a newest one.
    fn budget_repo() -> (tempfile::TempDir, [String; 4]) {
        let dir = init_repo();
        let root = dir.path();
        let first = commit_at(
            root,
            &[("a.txt", &bulky("first_marker")), ("gone.txt", b"doomed\n")],
            "first",
            days_ago(4),
        );
        let second = commit_at(
            root,
            &[("b.txt", &bulky("second_marker"))],
            "second",
            days_ago(3),
        );
        std::fs::remove_file(root.join("gone.txt")).unwrap();
        let rescue = commit_at(
            root,
            &[("c.txt", &bulky("rescue_marker"))],
            "rescue",
            days_ago(2),
        );
        let newest = commit_at(
            root,
            &[("d.txt", &bulky("newest_marker"))],
            "newest",
            days_ago(1),
        );
        (dir, [first, second, rescue, newest])
    }

    #[test]
    fn oldest_victims_orders_plain_commits_oldest_first_then_rescue_commits() {
        let (dir, [first, second, rescue, newest]) = budget_repo();
        let mut store = FactsStore::open(dir.path()).unwrap();
        ingest_with(&mut store, no_limits());
        let victims = oldest_victims(&store).unwrap();
        let got: Vec<(i64, bool)> = victims.iter().map(|v| (v.id, v.rescue)).collect();
        assert_eq!(
            got,
            vec![
                (commit_id(&store, &first), false),
                (commit_id(&store, &second), false),
                (commit_id(&store, &newest), false),
                (commit_id(&store, &rescue), true),
            ]
        );
        let first_at: i64 = store
            .conn()
            .query_row(
                "SELECT unixepoch(committed_at) FROM commits WHERE oid = ?1",
                [&first],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(victims[0].authored, Some(first_at));
    }

    /// A database already within budget loses nothing, even at the exact
    /// budget.
    #[test]
    fn enforce_budget_keeps_everything_within_the_budget() {
        let (dir, oids) = budget_repo();
        let mut store = FactsStore::open(dir.path()).unwrap();
        ingest_with(&mut store, no_limits());
        let used = store.used_bytes().unwrap();
        assert_eq!(enforce_budget(&mut store, used).unwrap(), 0);
        for oid in &oids {
            assert_eq!(state_of(&store, oid), (DIFF_STATE_INDEXED, None), "{oid}");
        }
    }

    /// Over budget, every diff goes if it must, and the loop ends when no
    /// diff is left even though metadata alone still exceeds the budget.
    #[test]
    fn enforce_budget_evicts_diffs_but_never_metadata_and_terminates() {
        let (dir, oids) = budget_repo();
        let mut store = FactsStore::open(dir.path()).unwrap();
        ingest_with(&mut store, no_limits());
        let pages_before: i64 = store
            .conn()
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .unwrap();
        assert_eq!(enforce_budget(&mut store, 0).unwrap(), 4);
        for oid in &oids {
            assert_eq!(
                state_of(&store, oid),
                (DIFF_STATE_EVICTED, Some(SKIP_NOTE_OVER_BUDGET.to_string())),
                "{oid}"
            );
            assert_eq!(hunks_for(&store, oid), 0, "{oid}");
        }
        assert_eq!(diff_hits(&store, "newest_marker"), 0);
        let commits: i64 = store
            .conn()
            .query_row("SELECT count(*) FROM commits", [], |r| r.get(0))
            .unwrap();
        assert_eq!(commits, 4, "metadata is never evicted");
        let freelist: i64 = store
            .conn()
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))
            .unwrap();
        assert_eq!(freelist, 0, "freed pages handed back to the file system");
        let pages_after: i64 = store
            .conn()
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .unwrap();
        assert!(pages_after < pages_before, "{pages_after} < {pages_before}");
    }

    /// Once a plain commit went for size, pending plain commits as old or
    /// older are marked instead of fetched; a pending rescue commit is not.
    #[test]
    fn enforce_budget_marks_older_pending_plain_commits_but_not_rescue_ones() {
        let (dir, [first, second, rescue, newest]) = budget_repo();
        let mut store = metadata_only(dir.path());
        ingest_diff_single(&mut store, &newest).unwrap();
        assert_eq!(enforce_budget(&mut store, 0).unwrap(), 3);
        let over = (DIFF_STATE_EVICTED, Some(SKIP_NOTE_OVER_BUDGET.to_string()));
        assert_eq!(state_of(&store, &newest), over);
        assert_eq!(state_of(&store, &first), over);
        assert_eq!(state_of(&store, &second), over);
        assert_eq!(state_of(&store, &rescue), (DIFF_STATE_PENDING, None));
    }

    /// End to end: a budget below the metadata's own size still converges,
    /// with no diff left and the history still answering by message.
    #[test]
    fn ingest_under_a_tiny_budget_converges_with_metadata_only() {
        let (dir, oids) = budget_repo();
        let mut store = FactsStore::open(dir.path()).unwrap();
        let report = ingest_with(
            &mut store,
            HistoryLimits {
                budget_bytes: 0,
                window_days: None,
            },
        );
        assert!(report.fresh, "{report:?}");
        let hunks: i64 = store
            .conn()
            .query_row("SELECT count(*) FROM hunks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(hunks, 0);
        assert_eq!(store.index_state().diffs_evicted, oids.len() as u64);
        assert_eq!(store.index_state().diff_coverage_since, None);
        let by_message = search(&store, "rescue", SearchFacet::Message, 10).unwrap();
        assert_eq!(by_message.candidates.len(), 1);
    }

    #[test]
    fn audited_ingest_timings_should_accumulate_every_phase_instead_of_replacing_it() {
        let mut total = IngestTimings {
            metadata: Duration::from_millis(1),
            blobs: Duration::from_millis(2),
            diffs: Duration::from_millis(3),
            retention: Duration::from_millis(4),
        };
        total.add(&IngestTimings {
            metadata: Duration::from_millis(10),
            blobs: Duration::from_millis(20),
            diffs: Duration::from_millis(30),
            retention: Duration::from_millis(40),
        });
        assert_eq!(
            total,
            IngestTimings {
                metadata: Duration::from_millis(11),
                blobs: Duration::from_millis(22),
                diffs: Duration::from_millis(33),
                retention: Duration::from_millis(44)
            }
        );
    }

    #[test]
    fn audited_tree_batches_should_include_exact_byte_and_path_caps_and_always_advance() {
        let exact = "x".repeat(32_767);
        let over = "x".repeat(32_768);
        assert_eq!(tree_batch_count(&[&exact, "next"]), 1);
        assert_eq!(tree_batch_count(&["a", &exact]), 1);
        assert_eq!(tree_batch_count(&[&over, "next"]), 1);
        assert_eq!(tree_batch_count(&vec!["a"; 129]), 128);
        let first = "x".repeat(16_383);
        assert_eq!(tree_batch_count(&[&first, &first, "next"]), 2);
    }

    #[test]
    fn audited_blob_caps_should_allow_exact_cap_on_both_sides_and_poison_only_overflow() {
        let dir = init_repo();
        let exact = vec![b'x'; BLOB_CAP_BYTES];
        let overflow = vec![b'y'; BLOB_CAP_BYTES + 1];
        let first = commit(
            dir.path(),
            &[("exact", &exact), ("overflow", &overflow)],
            "sized blobs",
        );
        let second = commit(
            dir.path(),
            &[("exact", b"small"), ("overflow", b"small")],
            "small blobs",
        );
        let mut store = FactsStore::open(dir.path()).unwrap();
        assert!(phase_a(&mut store, &far()).unwrap());
        for oid in [&first, &second] {
            let cid = commit_id(&store, oid);
            assert_eq!(measure_commits_blobs(&mut store, &[cid]).unwrap(), 1);
        }
        assert_eq!(store.poison_paths().unwrap(), vec!["overflow"]);
    }
}
