//! `ingest.rs` — the low-priority, checkpointed ingest engine.
//!
//! Three phases, resumable via the `ingest_jobs` cursor:
//!   Phase A — refs + commit metadata first, always completes.
//!   Phase B — path changes + blob sizes (before any diff is requested).
//!   Phase C — diff text, with skips decided BEFORE spawning git.
//!
//! The engine yields control back to the caller (the daemon's ingest thread)
//! every tick so queries are never blocked: each tick processes a bounded
//! batch then returns; a ref move just enqueues another tick.

use std::time::{Duration, Instant};

use rusqlite::params;

use pixel_git::GitOptions;

use crate::poison::{
    BLOB_CAP_BYTES, COMMIT_TEXT_CAP_BYTES, FILE_TEXT_CAP_BYTES, classify_content, decide_skips,
};
use crate::store::{
    DIFF_STATE_EVICTED, DIFF_STATE_INDEXED, DIFF_STATE_PENDING, DIFF_STATE_SKIPPED, FactsStore,
    REACH_BRANCH, REACH_REFLOG_ONLY, REACH_REMOTE, REACH_STASH, REACH_TAG,
    Result,
};

/// Default wall-clock budget per tick (250ms per PLAN.md). Queries never wait
/// on ingest: after this budget the engine returns and the daemon yields.
pub const DEFAULT_TICK_BUDGET_MS: u64 = 250;
/// Fixed phase-A metadata batch size.
const PHASE_A_BATCH: usize = 200;
/// Fixed phase-B/C commit batch size (25 per PLAN.md bounded batching).
const PHASE_B_BATCH: usize = 25;
/// 8MB aggregate output cap for a phase-B/C batch (bounded batching).
const BATCH_OUTPUT_CAP_BYTES: usize = 8 * 1024 * 1024;

/// Options controlling one ingest tick.
#[derive(Debug, Clone)]
pub struct IngestOptions {
    pub tick_budget_ms: u64,
}

impl Default for IngestOptions {
    fn default() -> Self {
        IngestOptions {
            tick_budget_ms: DEFAULT_TICK_BUDGET_MS,
        }
    }
}

/// Outcome of one tick, reported so the daemon can surface progress.
#[derive(Debug, Clone, PartialEq)]
pub struct TickReport {
    pub phase: String,
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

    // Phase A: ensure metadata is complete first (always completes before B/C).
    let a_done = phase_a(store, &deadline)?;
    if !a_done {
        let state = store.index_state();
        return Ok(TickReport {
            phase: state.phase,
            commits_indexed: state.commits_indexed,
            total_commits: state.total_commits,
            diff_indexed_pct: state.diff_indexed_pct,
            fresh: state.fresh,
            poisoned_this_tick: 0,
            skipped_this_tick: 0,
        });
    }
    let (b_done, p) = phase_b(store, &deadline)?;
    poisoned += p;
    if b_done {
        let (_c_done, p, s) = phase_c(store, &deadline)?;
        poisoned += p;
        skipped += s;
    }

    let state = store.index_state();
    Ok(TickReport {
        phase: state.phase,
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
pub const MAX_INGEST_UNTIL_FRESH_WALL_CLOCK: Duration = Duration::from_secs(600);

/// Convenience: run ticks until fresh or the caller gives up. Bounded by
/// `MAX_INGEST_UNTIL_FRESH_WALL_CLOCK` — see its doc comment.
pub fn ingest_until_fresh(store: &mut FactsStore, options: &IngestOptions) -> Result<TickReport> {
    let mut n = 0u64;
    let start = Instant::now();
    let dbg = std::env::var("PIXEL_FACTS_DEBUG_TICKS").is_ok();
    loop {
        let report = ingest_tick(store, options)?;
        n += 1;
        if dbg {
            eprintln!("tick {n}: {:?}", report);
        }
        if report.fresh {
            return Ok(report);
        }
        if start.elapsed() >= MAX_INGEST_UNTIL_FRESH_WALL_CLOCK {
            return Err(crate::store::FactsError::Msg(format!(
                "ingest_until_fresh did not converge after {n} ticks / {:?} — last report: {:?}",
                start.elapsed(),
                report
            )));
        }
    }
}

/// Default wall-clock budget for the lazy query-path ingest loop (~3s).
pub const DEFAULT_LAZY_INGEST_BUDGET_MS: u64 = 3000;

/// Env-tunable lazy-ingest budget: `PIXEL_FACTS_QUERY_BUDGET_MS` (canonical)
/// with `PIXEL_FACTS_LAZY_BUDGET_MS` accepted as an alias.
pub fn lazy_ingest_budget_ms() -> u64 {
    std::env::var("PIXEL_FACTS_QUERY_BUDGET_MS")
        .or_else(|_| std::env::var("PIXEL_FACTS_LAZY_BUDGET_MS"))
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_LAZY_INGEST_BUDGET_MS)
}

/// Bounded ingest loop for the query path: run ticks until fresh or the given
/// wall-clock budget (ms) is exhausted, then return the last report. Unlike
/// `ingest_until_fresh` this never blocks a query for more than `budget_ms`,
/// so it is safe to call from `op_excavate` / `op_history` / `op_lifecycle`
/// when the index is not fresh. Each tick still uses the normal per-tick
/// budget so queries are never starved.
pub fn ingest_until_fresh_bounded(store: &mut FactsStore, budget_ms: u64) -> Result<TickReport> {
    let options = IngestOptions {
        tick_budget_ms: DEFAULT_TICK_BUDGET_MS,
    };
    let start = Instant::now();
    let mut last = ingest_tick(store, &options)?;
    while !last.fresh && start.elapsed() < Duration::from_millis(budget_ms) {
        last = ingest_tick(store, &options)?;
    }
    Ok(last)
}

/// Convenience: `ingest_until_fresh_bounded` with the env-tunable default
/// budget (`PIXEL_FACTS_LAZY_BUDGET_MS`, default 3s).
pub fn lazy_ingest(store: &mut FactsStore) -> Result<TickReport> {
    ingest_until_fresh_bounded(store, lazy_ingest_budget_ms())
}

// ---------------------------------------------------------------------------
// Phase A — refs + metadata
// ---------------------------------------------------------------------------

fn phase_a(store: &mut FactsStore, deadline: &Instant) -> Result<bool> {
    if !needs_phase_a(store)? {
        return Ok(true);
    }
    refresh_refs(store)?;
    // Enumerate every commit reachable from any ref, plus stash and reflog.
    let oids = enumerate_all_commits(store)?;
    let known = known_oids(store);
    let pending: Vec<String> = oids
        .iter()
        .filter(|o| !known.contains(*o))
        .cloned()
        .collect();
    let dbg = std::env::var("PIXEL_FACTS_DEBUG_TICKS").is_ok();
    if dbg {
        eprintln!("phase_a: oids={} known={} pending={}", oids.len(), known.len(), pending.len());
    }
    store_phase_a_cursor(store, &pending)?;
    if pending.is_empty() {
        complete_phase_a(store)?;
        return Ok(true);
    }
    // Guaranteed-progress (do-while) loop: the deadline is a soft yield
    // target, not a license to do zero work. Checking it BEFORE the first
    // batch (as a plain `while`) would let setup cost alone (refresh_refs +
    // enumerate_all_commits + known_oids — five git subprocess spawns) eat
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
        if dbg {
            eprintln!("phase_a: batch [{idx}..{batch_end}) fetched {} parsed commits", commits.len());
        }
        insert_phase_a_batch(store, &commits, &reach)?;
        idx = batch_end;
        if idx >= pending.len() || Instant::now() >= *deadline {
            break;
        }
    }
    let done = idx >= pending.len();
    if dbg {
        eprintln!("phase_a: idx={idx} pending.len()={} done={done}", pending.len());
    }
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
    let current = store.current_refs_hash()?;
    Ok(stored.as_deref() != Some(current.as_str()))
}

fn complete_phase_a(store: &mut FactsStore) -> Result<()> {
    // Record the refs hash at completion so a later ref move is detectable.
    let hash = store.current_refs_hash()?;
    store.conn().execute(
        "UPDATE ingest_jobs SET status = 'done', ref_hash = ?1, updated_at = ?2 WHERE phase = 'A'",
        params![hash, now_iso()],
    )?;
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

fn refresh_refs(store: &mut FactsStore) -> Result<()> {
    let runner = store.runner();
    // heads / remotes / tags
    let refs = runner.run(&["for-each-ref", "--format=%(refname)%00%(objectname)"])?;
    let lines = split_nul_lines(&refs);
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, '\0');
        let (refname, oid) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        if refname.is_empty() || oid.is_empty() {
            continue;
        }
        let kind = if refname.starts_with("refs/heads/") {
            "branch"
        } else if refname.starts_with("refs/remotes/") {
            "remote"
        } else if refname.starts_with("refs/tags/") {
            "tag"
        } else {
            "other"
        };
        store.conn().execute(
            "INSERT INTO refs (ref, oid, kind, indexed_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (ref) DO UPDATE SET oid = excluded.oid, indexed_at = excluded.indexed_at",
            params![refname, oid, kind, now_iso()],
        )?;
    }
    // refs/stash + stash reflog (first-class reach).
    let stash = runner.run(&["for-each-ref", "--format=%(refname)%00%(objectname)", "refs/stash"]);
    if let Ok(out) = stash {
        for line in split_nul_lines(&out) {
            if line.is_empty() {
                continue;
            }
            let mut parts = line.splitn(2, '\0');
            let (refname, oid) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            if !refname.is_empty() && !oid.is_empty() {
                store.conn().execute(
                    "INSERT INTO refs (ref, oid, kind, indexed_at) VALUES (?1, ?2, 'stash', ?3)
                     ON CONFLICT (ref) DO UPDATE SET oid = excluded.oid, indexed_at = excluded.indexed_at",
                    params![refname, oid, now_iso()],
                )?;
            }
        }
    }
    Ok(())
}

/// Enumerate every commit that should be indexed: all reachable from refs,
/// plus stash, plus reflog-only commits. Returns oids in reverse (oldest-first)
/// order and computes the reach bitmask per oid.
fn enumerate_all_commits(store: &FactsStore) -> Result<Vec<String>> {
    let runner = store.runner();
    let mut all: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Branches + remotes + tags via a single rev-list of all heads/remotes/tags.
    let args = vec!["rev-list", "--reverse", "--branches", "--remotes", "--tags"];
    if let Ok(out) = runner.run(&args) {
        let reach = REACH_BRANCH | REACH_REMOTE | REACH_TAG;
        for oid in split_nul_lines(&out) {
            if !oid.is_empty() && seen.insert(oid.clone()) {
                all.push(oid.clone());
                set_reach(store, &oid, reach)?;
            }
        }
    }

    // Stash.
    if let Ok(out) = runner.run(&["rev-list", "--reverse", "refs/stash"]) {
        for oid in split_nul_lines(&out) {
            if !oid.is_empty() && seen.insert(oid.clone()) {
                all.push(oid.clone());
                set_reach(store, &oid, REACH_STASH)?;
            }
        }
    }

    // Reflog-only commits (reachable via reflogs but not any branch/remote/tag).
    if let Ok(out) = runner.run(&[
        "rev-list",
        "--reverse",
        "--reflog",
        "--not",
        "--branches",
        "--remotes",
        "--tags",
    ]) {
        for oid in split_nul_lines(&out) {
            if !oid.is_empty() && seen.insert(oid.clone()) {
                all.push(oid.clone());
                set_reach(store, &oid, REACH_REFLOG_ONLY)?;
            }
        }
    }

    Ok(all)
}

fn set_reach(store: &FactsStore, oid: &str, bits: i64) -> Result<()> {
    // If the commit row already exists, OR the bits; else defer (phase A insert
    // will set them). We store the reach map in a temp table keyed by oid.
    store.conn().execute(
        "INSERT INTO reach_map (oid, bits) VALUES (?1, ?2)
         ON CONFLICT (oid) DO UPDATE SET bits = bits | excluded.bits",
        params![oid, bits],
    )?;
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
fn fetch_phase_a_batch(store: &FactsStore, oids: &[String]) -> Result<(Vec<PhaseACommit>, Vec<String>)> {
    // Higher cap than the default 1MiB: a 200-commit metadata batch with long
    // messages / many changed paths can exceed it. This is bounded by the
    // PHASE_A_BATCH commit count, not by diff text (phase A has no diff text).
    let opts = GitOptions {
        timeout: Some(Duration::from_secs(120)),
        max_output_bytes: Some(BATCH_OUTPUT_CAP_BYTES),
    };
    let runner = pixel_git::GitRunner::with_options(store.root(), opts);
    let mut args: Vec<&str> = vec![
        "log",
        "-z",
        "--no-walk=unsorted",
        "--format=%x1e%H%x00%P%x00%an%x00%aI%x00%B",
        "--name-status",
        "--end-of-options",
    ];
    let oid_refs: Vec<&str> = oids.iter().map(|s| s.as_str()).collect();
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
            .map(|s| s.to_string())
            .collect();
        let author = str(fields[2]).trim().to_string();
        let committed_at = str(fields[3]).trim().to_string();
        let message = str(fields[4]).trim_end_matches('\n').to_string();
        let mut changes = Vec::new();
        let mut index = 5;
        while index < fields.len() {
            let raw = str(fields[index]);
            let raw_status = raw.trim_start();
            if raw_status.is_empty() {
                index += 1;
                continue;
            }
            let status = raw_status.chars().next().unwrap_or(' ');
            match status {
                'R' | 'C' => {
                    if index + 2 >= fields.len() {
                        break;
                    }
                    let old_path = str(fields[index + 1]);
                    let path = str(fields[index + 2]);
                    changes.push(Change {
                        status: status.to_string(),
                        path,
                        old_path: Some(old_path),
                    });
                    index += 3;
                }
                _ => {
                    if index + 1 >= fields.len() {
                        break;
                    }
                    let path = str(fields[index + 1]);
                    changes.push(Change {
                        status: status.to_string(),
                        path,
                        old_path: None,
                    });
                    index += 2;
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
        let mut ins_msg = tx.prepare("INSERT INTO messages_fts (rowid, message) VALUES (?1, ?2)")?;
        let mut ins_fc = tx.prepare(
            "INSERT OR IGNORE INTO file_changes (commit_id, path, status, old_path) VALUES (?1, ?2, ?3, ?4)",
        )?;
        let mut sel_fc = tx.prepare("SELECT id FROM file_changes WHERE commit_id = ?1 AND path = ?2")?;
        let mut ins_pgram = tx.prepare("INSERT INTO path_grams (hash, change_id) VALUES (?1, ?2)")?;
        for c in commits {
            let reach = c.reach;
            // Merge in any reach bits discovered during enumeration.
            let extra: Option<i64> = tx
                .query_row(
                    "SELECT bits FROM reach_map WHERE oid = ?1",
                    [&c.oid],
                    |r| r.get(0),
                )
                .ok();
            let final_reach = reach | extra.unwrap_or(0);
            let is_merge = c.parents.len() > 1;
            let diff_state = if is_merge { DIFF_STATE_SKIPPED } else { DIFF_STATE_PENDING };
            let skip_note = if is_merge { Some("merge".to_string()) } else { None };
            ins.execute(params![
                c.oid,
                c.parents.join(" "),
                c.author,
                c.committed_at,
                c.message,
                final_reach,
                diff_state,
                skip_note
            ])?;
            if let Ok(id) = sel.query_row([&c.oid], |r| r.get::<_, i64>(0)) {
                ins_msg.execute(params![id, c.message])?;
                for ch in &c.changes {
                    ins_fc.execute(params![id, ch.path, ch.status, ch.old_path])?;
                    // Emit path trigrams for path search (rowid-in-path trick).
                    if let Ok(fc_id) =
                        sel_fc.query_row(params![id, ch.path], |r| r.get::<_, i64>(0))
                    {
                        emit_path_grams(&mut ins_pgram, &ch.path, fc_id)?;
                    }
                }
            }
        }
    }
    tx.commit()?;
    Ok(())
}

/// Emit trigram hashes for a path into `path_grams`, tagged with the
/// file_changes rowid.
fn emit_path_grams(ins: &mut rusqlite::Statement, path: &str, change_id: i64) -> Result<()> {
    use pixel_index::{GramExtractor, TrigramExtractor};
    let extractor = TrigramExtractor;
    let mut hits = Vec::new();
    extractor.grams(path.as_bytes(), &mut hits);
    for h in hits {
        ins.execute(params![h.hash as i64, change_id])?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Phase B — path changes + blob sizes (before any diff)
// ---------------------------------------------------------------------------

/// Returns (phaseB_done, poisoned_this).
fn phase_b(store: &mut FactsStore, deadline: &Instant) -> Result<(bool, u64)> {
    // Phase B is "complete" when no commit remains to be blob-measured. This is
    // re-evaluated every tick, so new commits (ref moves, incremental) get
    // measured naturally. Cursor = last commit id measured.
    let mut poisoned = 0u64;
    let mut cursor: i64 = phase_b_cursor(store);
    loop {
        let next = next_phase_b_commit(store, cursor);
        let cid = match next {
            Some(c) => c,
            None => {
                complete_phase_b(store)?;
                return Ok((true, poisoned));
            }
        };
        let poisoned_batch = measure_commit_blobs(store, cid)?;
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

fn next_phase_b_commit(store: &FactsStore, after_cursor: i64) -> Option<i64> {
    store
        .conn()
        .query_row(
            "SELECT id FROM commits
             WHERE id > ?1 AND diff_state = ?2
             ORDER BY id LIMIT 1",
            params![after_cursor, DIFF_STATE_PENDING],
            |r| r.get(0),
        )
        .ok()
}

fn complete_phase_b(store: &mut FactsStore) -> Result<()> {
    store.conn().execute(
        "UPDATE ingest_jobs SET status = 'done', updated_at = ?1 WHERE phase = 'B'",
        [now_iso()],
    )?;
    Ok(())
}

/// For one commit, gather its changed paths, measure blob sizes via
/// `cat-file --batch-check`, and learn any over-cap paths as poison forever.
/// Returns the number of newly-poisoned paths.
fn measure_commit_blobs(store: &mut FactsStore, cid: i64) -> Result<u64> {
    let paths = changed_paths_for_commit(store, cid)?;
    let oid: String = store
        .conn()
        .query_row("SELECT oid FROM commits WHERE id = ?1", [cid], |r| r.get(0))?;
    let mut poisoned = 0u64;
    let sizes = measure_blob_sizes(store, &oid, &paths)?;
    for (path, (size_add, size_rem)) in &sizes {
        if *size_add > BLOB_CAP_BYTES as u64 || *size_rem > BLOB_CAP_BYTES as u64 {
            store.learn_poison(path, &format!("blob over {}B cap", BLOB_CAP_BYTES))?;
            poisoned += 1;
        }
    }
    Ok(poisoned)
}

fn changed_paths_for_commit(store: &FactsStore, cid: i64) -> Result<Vec<String>> {
    let mut stmt = store.conn().prepare("SELECT path FROM file_changes WHERE commit_id = ?1")?;
    let rows = stmt.query_map([cid], |r| r.get::<_, String>(0))?;
    let mut v = Vec::new();
    for row in rows {
        v.push(row?);
    }
    Ok(v)
}

/// Measure (added_size, removed_size) for each changed path of a commit using
/// `git cat-file --batch-check` on the blob OIDs at the commit's tree. Since
/// the runner's stdin is null, we pass OIDs as positional args.
fn measure_blob_sizes(
    store: &FactsStore,
    oid: &str,
    paths: &[String],
) -> Result<Vec<(String, (u64, u64))>> {
    let opts = GitOptions {
        timeout: Some(Duration::from_secs(30)),
        max_output_bytes: Some(1_048_576),
    };
    let cmd_runner = pixel_git::GitRunner::with_options(store.root(), opts);
    let mut out = Vec::new();
    for path in paths {
        let obj = format!("{oid}:{path}");
        let size_new = cat_file_size(&cmd_runner, &obj);
        let size_old = if let Some(old) = old_path_for(store, path) {
            cat_file_size(&cmd_runner, &format!("{oid}:{old}"))
        } else {
            0
        };
        out.push((path.clone(), (size_new, size_old)));
    }
    Ok(out)
}

fn cat_file_size(runner: &pixel_git::GitRunner, obj: &str) -> u64 {
    match runner.run(&["cat-file", "--batch-check", "--end-of-options", obj]) {
        Ok(bytes) => {
            let line = String::from_utf8_lossy(&bytes);
            let parts: Vec<&str> = line.split(' ').collect();
            if parts.len() >= 3 && parts[1] == "blob" {
                parts[2].trim().parse().unwrap_or(0)
            } else {
                0
            }
        }
        Err(_) => 0,
    }
}

fn old_path_for(store: &FactsStore, path: &str) -> Option<String> {
    store
        .conn()
        .query_row(
            "SELECT old_path FROM file_changes WHERE path = ?1 AND old_path IS NOT NULL LIMIT 1",
            [path],
            |r| r.get::<_, String>(0),
        )
        .ok()
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

fn pending_phase_c(store: &FactsStore) -> Result<Vec<i64>> {
    let mut stmt = store.conn().prepare(
        "SELECT id FROM commits
         WHERE diff_state = ?1
         ORDER BY committed_at, id
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
    let runner = pixel_git::GitRunner::with_options(store.root(), opts);
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
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

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
        Err(pixel_git::GitError::OutputTooLarge { .. }) => {
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
    let runner = pixel_git::GitRunner::with_options(store.root(), opts);
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
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
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
        .query_row("SELECT id FROM commits WHERE oid = ?1", [&commit.oid], |r| {
            r.get(0)
        })
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
                .take(crate::poison::CONTENT_PROBE_BYTES)
                .copied()
                .collect();
            let kind = classify_content(&probe, &f.path);
            !matches!(kind, crate::poison::ContentKind::Text)
        })
        .collect();
    let tx = store.conn_mut().transaction()?;
    {
        let mut ins = tx.prepare(
            "INSERT INTO hunks (commit_id, path, added, removed, truncated)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        let mut ins_gram = tx.prepare("INSERT INTO diff_grams (hash, hunk_id) VALUES (?1, ?2)")?;
        let mut mark = tx.prepare(
            "UPDATE commits SET diff_state = ?1 WHERE id = ?2 AND diff_state = ?3",
        )?;
        let mut commit_bytes = 0usize;
        let mut over_cap = false;
        for (i, file) in commit.files.iter().enumerate() {
            let path = &file.path;
            if skip[i] {
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
            // Trigram the added+removed text into diff_grams keyed by hunk id.
            emit_grams(&mut ins_gram, &file.added, id)?;
            emit_grams(&mut ins_gram, &file.removed, id)?;
            commit_bytes += file.added.len() + file.removed.len();
        }
        mark.execute(
            params![
                if over_cap { DIFF_STATE_SKIPPED } else { DIFF_STATE_INDEXED },
                cid,
                DIFF_STATE_PENDING
            ],
        )?;
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

/// Emit grams for `text` into the diff_grams posting table, tagged with the
/// hunk rowid. Uses pixel-index's trigram extractor (xxh3-based).
fn emit_grams(
    ins: &mut rusqlite::Statement,
    text: &str,
    hunk_id: i64,
) -> Result<()> {
    use pixel_index::{GramExtractor, TrigramExtractor};
    let extractor = TrigramExtractor;
    let mut hits = Vec::new();
    extractor.grams(text.as_bytes(), &mut hits);
    for h in hits {
        ins.execute(params![h.hash as i64, hunk_id])?;
    }
    Ok(())
}

fn now_iso() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

fn split_nul_lines(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    text.split('\n')
        .map(|s| s.to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Eviction (budget)
// ---------------------------------------------------------------------------

/// Enforce the diff-residue budget. Evicts oldest commits' hunks (diff_state=3,
/// metadata retained) until under budget, skipping any commit that is the
/// `removed-in` for a path deleted from HEAD (the rescue payload).
pub fn evict_to_budget(store: &mut FactsStore, budget_bytes: u64) -> Result<u64> {
    let total: i64 = store.conn().query_row(
        "SELECT COALESCE(SUM(length(added)) + SUM(length(removed)), 0) FROM hunks",
        [],
        |r| r.get(0),
    )?;
    if (total as u64) <= budget_bytes {
        return Ok(0);
    }
    let mut evicted = 0u64;
    // Oldest non-rescue commits first.
    let cids: Vec<i64> = {
        let mut stmt = store.conn().prepare(
            "SELECT h.commit_id
             FROM hunks h
             JOIN commits c ON c.id = h.commit_id
             WHERE c.id NOT IN (
                SELECT fc.commit_id FROM file_changes fc
                WHERE fc.status = 'D'
             )
             ORDER BY c.committed_at, c.id
             LIMIT 1000",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        let mut v = Vec::new();
        for row in rows {
            v.push(row?);
        }
        v
    };
    for cid in cids {
        let used: i64 = store.conn().query_row(
            "SELECT COALESCE(SUM(length(added)) + SUM(length(removed)), 0) FROM hunks WHERE commit_id = ?1",
            [cid],
            |r| r.get(0),
        )?;
        store.conn().execute("DELETE FROM hunks WHERE commit_id = ?1", [cid])?;
        store.conn().execute(
            "UPDATE commits SET diff_state = ?1 WHERE id = ?2",
            params![DIFF_STATE_EVICTED, cid],
        )?;
        evicted += used as u64;
        let now_total: i64 = store.conn().query_row(
            "SELECT COALESCE(SUM(length(added)) + SUM(length(removed)), 0) FROM hunks",
            [],
            |r| r.get(0),
        )?;
        if (now_total as u64) <= budget_bytes {
            break;
        }
    }
    Ok(evicted)
}
