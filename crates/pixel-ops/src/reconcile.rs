//! `reconcile` — Engine 4: one-call deterministic branch sync.
//!
//! Eliminates OID transcription: the snapshot inside the lock supplies all
//! expectations, making STALE_STATE structurally impossible within the op.
//!
//! Flow: snapshot → fetch (branch-scoped) → classify (rev-list --left-right
//! --count) → up_to_date | fast_forward | ahead (leased push) | diverged
//! (report or rebase-if-clean).
//!
//! Divergence policy: default "report"; "rebase-if-clean" is explicit opt-in.
//! Never merge commits. Zero-textual-conflict rebase is deterministic work.
//! Mechanics: clean worktree required → prove cleanliness per replayed
//! commit with `git merge-tree --write-tree` → non-interactive `git rebase`
//! under journal → backup ref first → any surprise conflict → `rebase --abort`
//! + diverged report.
//!
//! Lease race: if the remote moves between this call's own fetch and its
//! push attempt, exactly one re-fetch + reclassify is performed, then a
//! terminal report is returned — never a second blind retry.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{json, Value};

use pixel_git::GitRunner;

use crate::durable::{sha256_hex, state_root};
use crate::journal::{BeginOutcome, JournalOperation, OperationJournal};
use crate::lock::RepositoryLock;

/// Per-path hunk cap (bytes). Individual `ours`/`theirs` hunks are truncated
/// past this with `hunk_truncated: true`.
const HUNK_MAX_BYTES: usize = 32 * 1024;
/// Whole-report cap (bytes, approximate — measured over each conflict
/// entry's serialized size as it is accumulated). Past this, remaining
/// conflict entries are dropped and `report_truncated: true` is set.
const REPORT_MAX_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone)]
pub struct ReconcileOptions {
    pub strategy: String,    // "report" | "rebase-if-clean"
    pub push: String,        // "auto" | "none" ("never" accepted as an alias of "none")
    pub request_id: String,
}

/// Validate and normalize the `--push` value. Accepted: `"auto"` (push when
/// safe), `"none"` (never push), and `"never"` as an explicit alias of
/// `"none"` (older rule text documented it). Anything else is a structured
/// error naming the accepted values — previously any unrecognized string
/// (a typo like `--push always`, or the once-documented `never`) silently
/// meant don't-push.
fn validate_push_mode(push: &str) -> Result<&'static str, String> {
    match push {
        "auto" => Ok("auto"),
        "none" | "never" => Ok("none"),
        other => Err(format!(
            "invalid push value {other:?}: accepted values are \"auto\" (push when safe) \
             or \"none\" (never push; \"never\" is an accepted alias of \"none\")"
        )),
    }
}

pub fn reconcile(root: &Path, opts: &ReconcileOptions) -> Result<Value, String> {
    reconcile_with_hooks(root, opts, None)
}

/// Test seam mirroring `push::PushProbe`: `pre_push_hook`, when given, runs
/// exactly once, right after this call's own fetch (`upstream_oid` has just
/// been captured) and before any leased push is attempted — the precise
/// window a "remote moved after our fetch, before our push" race occupies.
/// Production callers always go through `reconcile()`, which passes `None`;
/// this exists so the lease-race path (single re-fetch + reclassify, never a
/// second blind retry) can be exercised deterministically in tests instead
/// of relying on incidental timing.
pub fn reconcile_with_hooks(
    root: &Path,
    opts: &ReconcileOptions,
    mut pre_push_hook: Option<Box<dyn FnMut()>>,
) -> Result<Value, String> {
    // Validate BEFORE any journal/lock/git work — a bad value must be a
    // structured error, never a silent don't-push. The normalized mode is
    // also what gets hashed, so "never" and "none" replay identically.
    let push_mode = validate_push_mode(&opts.push)?;

    let runner = GitRunner::new(root);
    let repo_key = root.canonicalize().unwrap_or_else(|_| root.to_path_buf()).display().to_string();
    let input_hash = sha256_hex(&format!("{}\u{0}{}", opts.strategy, push_mode));

    let state_root = state_root();
    let journal = OperationJournal::with_state_root(state_root.clone());

    // The wire surface (pixel-proto's `Request::Reconcile` / pixel-daemon's
    // `op_reconcile`) does not currently thread a client-supplied
    // `request_id` through — every real dispatch arrives here with
    // `opts.request_id == ""`, which `journal.begin` rejects outright
    // ("requestId must be 1-128 chars"), hard-failing every call. Default to
    // a fresh UUID so the op is at least callable end-to-end; the tradeoff is
    // that crash-resume replay (`BeginOutcome::Replay`/`Resume`) is only
    // reachable for callers who supply their own stable `request_id`
    // explicitly — see the final report for the upstream wiring gap this
    // papers over.
    let request_id = if opts.request_id.is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        opts.request_id.clone()
    };

    let outcome = journal.begin(&request_id, JournalOperation::Update, &repo_key, &input_hash)?;
    if let BeginOutcome::Replay(result) = outcome {
        return Ok(result);
    }

    let mut lock = RepositoryLock::acquire_with_state_root(
        &root.join(".git").display().to_string(),
        &state_root,
    ).map_err(|_| "repository is busy".to_string())?;

    // Snapshot current state.
    let head = runner.rev_parse_head().ok_or("no HEAD")?;
    let branch = runner.current_branch().unwrap_or_else(|| "HEAD".to_string());
    let upstream = format!("origin/{branch}");

    // Branch-scoped fetch (idempotent, outside journal transitions). Fetches
    // only this branch rather than every ref on the remote — still updates
    // `refs/remotes/origin/<branch>` because it matches the remote's default
    // fetch refspec, without the non-determinism of pulling in unrelated
    // branch/tag churn.
    runner.run(&["fetch", "--end-of-options", "origin", &branch]).map_err(|e| {
        let _ = lock.release();
        format!("git fetch: {e}")
    })?;

    // The OID this call's own fetch just observed for the remote branch.
    // This — NOT local HEAD — is what a `--force-with-lease` must assert the
    // remote still is: local HEAD is by construction different from (ahead
    // of) the remote whenever a lease is about to be used, so leasing
    // against local HEAD is not a race-only failure, it is a guaranteed
    // "stale info" rejection on every single push (confirmed live: pushing
    // with `--force-with-lease=<branch>:<local HEAD>` against a real ahead
    // fixture is rejected 100% of the time; leasing against the fetched
    // `origin/<branch>` oid instead succeeds). This is what makes the lease
    // airtight per the one-call design: the expectation comes from a fetch
    // this very call performed, never from a value the caller transcribed.
    let upstream_oid = runner
        .run_opt(&["rev-parse", &upstream])
        .map(|o| String::from_utf8_lossy(&o).trim().to_string())
        .unwrap_or_default();

    if let Some(hook) = pre_push_hook.as_mut() {
        hook();
    }

    // Classify with rev-list --left-right --count.
    let (ahead, behind) = classify_counts(&runner, &upstream);

    let merge_base = runner.run_opt(&["merge-base", "HEAD", &upstream])
        .map(|o| String::from_utf8_lossy(&o).trim().to_string())
        .unwrap_or_default();

    let state = classify_state(ahead, behind);

    let result = match state {
        "up_to_date" => {
            json!({
                "state": "up_to_date",
                "head": head,
                "branch": branch,
                "upstream": upstream,
            })
        }
        "fast_forward" => {
            // update's guard suite: refuse if incoming changes intersect
            // dirty paths that would be silently overwritten. Uses
            // `status_porcelain_or_err`/`diff_name_status_or_err`, NOT
            // their lenient counterparts: an undetermined status or
            // changed-path set must abort the fast-forward, never be
            // silently read as "nothing is dirty"/"nothing changed" — that
            // would let `git merge --ff-only` overwrite a genuinely dirty
            // file with no way for the caller to have known.
            let dirty = runner.status_porcelain_or_err().map_err(|e| {
                let _ = lock.release();
                format!(
                    "could not determine working-tree status, refusing to fast-forward: {e}"
                )
            })?;
            if !dirty.is_empty() {
                let changes = runner.diff_name_status_or_err(&head, &upstream).map_err(|e| {
                    let _ = lock.release();
                    format!(
                        "could not determine which paths the fast-forward would change, \
                         refusing to proceed while dirty files are present: {e}"
                    )
                })?;
                let changed_paths: std::collections::HashSet<String> = changes.iter().map(|(_, p)| p.clone()).collect();
                let dirty_intersect: Vec<String> = dirty.iter()
                    .filter(|(_, p)| changed_paths.contains(p))
                    .map(|(_, p)| p.clone())
                    .collect();
                if !dirty_intersect.is_empty() {
                    let _ = lock.release();
                    return Err(format!("UNSUPPORTED_STATE: dirty files would be overwritten: {}", dirty_intersect.join(", ")));
                }
            }
            // Fast-forward. `--ff-only` guarantees this can never fabricate
            // a merge commit.
            runner.run(&["merge", "--ff-only", &upstream]).map_err(|e| {
                let _ = lock.release();
                format!("git merge --ff-only: {e}")
            })?;
            json!({
                "state": "fast_forwarded",
                "from": head,
                "to": runner.rev_parse_head().unwrap_or_default(),
                "branch": branch,
            })
        }
        "ahead" => {
            if push_mode == "auto" {
                match attempt_lease_push_or_reclassify(&runner, &branch, &upstream_oid, &upstream) {
                    LeaseOutcome::Pushed => json!({
                        "state": "pushed",
                        "head": head,
                        "branch": branch,
                    }),
                    LeaseOutcome::Raced { error, ahead: a2, behind: b2 } => json!({
                        "state": "push_raced",
                        "head": head,
                        "branch": branch,
                        "push_error": error,
                        "reclassified": {
                            "ahead": a2,
                            "behind": b2,
                            "state": classify_state(a2, b2),
                        },
                        "next": "remote advanced during push; call reconcile again",
                    }),
                }
            } else {
                json!({
                    "state": "ahead",
                    "head": head,
                    "branch": branch,
                    "ahead": ahead,
                })
            }
        }
        "diverged" => {
            if !git_supports_merge_tree_write_tree(root) {
                json!({
                    "state": "diverged",
                    "merge_base": merge_base,
                    "ahead": ahead,
                    "behind": behind,
                    "clean_rebase_possible": false,
                    "feature_unavailable": true,
                    "conflicts": [],
                    "non_conflicting": non_conflicting_paths(root, &merge_base, &head, &upstream),
                    "backup_ref": Value::Null,
                    "next": "git >= 2.38 required for merge-tree; conflict detail and rebase-if-clean are unavailable on this git",
                })
            } else {
                let probe = probe_merge_tree(root, &head, &upstream);

                if opts.strategy == "rebase-if-clean" {
                    // Clean worktree required as precondition. Uses
                    // `status_porcelain_or_err`: an undetermined status must
                    // abort, never be silently read as "clean" — that would
                    // let a rebase run against a dirty worktree it was
                    // specifically gated to never touch.
                    let dirty = runner.status_porcelain_or_err().map_err(|e| {
                        let _ = lock.release();
                        format!(
                            "could not determine working-tree status, refusing rebase-if-clean: {e}"
                        )
                    })?;
                    if !dirty.is_empty() {
                        let _ = lock.release();
                        return Err(format!("UNSUPPORTED_STATE: rebase-if-clean requires clean worktree, {} dirty files", dirty.len()));
                    }

                    // Backup ref written FIRST, before any attempt to touch
                    // the worktree.
                    let backup_ref = format!("refs/pixel/reconcile-backup/{branch}");
                    runner.run(&["update-ref", &backup_ref, &head]).map_err(|e| {
                        let _ = lock.release();
                        format!("git update-ref (backup): {e}")
                    })?;

                    if !probe.clean {
                        // merge-tree predicts conflicts — never attempt the
                        // rebase, report diverged with full conflict detail.
                        let report = build_conflict_report(&runner, &merge_base, &head, &upstream, &probe);
                        json!({
                            "state": "diverged",
                            "merge_base": merge_base,
                            "ahead": ahead,
                            "behind": behind,
                            "clean_rebase_possible": false,
                            "conflicts": report["conflicts"],
                            "conflict_count": report["conflict_count"],
                            "report_truncated": report["report_truncated"],
                            "non_conflicting": non_conflicting_paths(root, &merge_base, &head, &upstream),
                            "backup_ref": backup_ref,
                            "next": "manual resolution required",
                        })
                    } else {
                        // Non-interactive linear rebase under journal. Plain
                        // `git rebase` never fabricates a merge commit.
                        match runner.run(&["rebase", &upstream]) {
                            Ok(_) => {
                                let new_head = runner.rev_parse_head().unwrap_or_default();
                                if push_mode == "auto" {
                                    // Lease against `upstream_oid`, not
                                    // `new_head`: the rebase only rewrote
                                    // local history, the remote is still at
                                    // the OID this call fetched earlier.
                                    match attempt_lease_push_or_reclassify(&runner, &branch, &upstream_oid, &upstream) {
                                        LeaseOutcome::Pushed => json!({
                                            "state": "rebased",
                                            "from": head,
                                            "to": new_head,
                                            "branch": branch,
                                            "backup_ref": backup_ref,
                                            "pushed": true,
                                        }),
                                        LeaseOutcome::Raced { error, ahead: a2, behind: b2 } => json!({
                                            "state": "rebased",
                                            "from": head,
                                            "to": new_head,
                                            "branch": branch,
                                            "backup_ref": backup_ref,
                                            "pushed": false,
                                            "push_error": error,
                                            "reclassified": {
                                                "ahead": a2,
                                                "behind": b2,
                                                "state": classify_state(a2, b2),
                                            },
                                            "next": "local rebase succeeded but remote advanced during push; call reconcile again",
                                        }),
                                    }
                                } else {
                                    json!({
                                        "state": "rebased",
                                        "from": head,
                                        "to": new_head,
                                        "branch": branch,
                                        "backup_ref": backup_ref,
                                        "pushed": false,
                                    })
                                }
                            }
                            Err(e) => {
                                // Surprise conflict despite a clean merge-tree
                                // prediction. Capture the actual unmerged
                                // paths from the mid-rebase index BEFORE
                                // aborting — this is the only window they're
                                // observable in, since `rebase --abort`
                                // restores the clean pre-rebase worktree.
                                let unmerged: Vec<String> = runner
                                    .status_porcelain()
                                    .into_iter()
                                    .filter(|(xy, _)| xy.contains('U') || xy == "AA" || xy == "DD")
                                    .map(|(_, p)| p)
                                    .collect();
                                // Never leave the repo mid-rebase.
                                let _ = runner.run(&["rebase", "--abort"]);
                                json!({
                                    "state": "diverged",
                                    "merge_base": merge_base,
                                    "ahead": ahead,
                                    "behind": behind,
                                    "clean_rebase_possible": true,
                                    "rebase_aborted": true,
                                    "error": e.to_string(),
                                    "unmerged_paths_at_abort": unmerged,
                                    "non_conflicting": non_conflicting_paths(root, &merge_base, &head, &upstream),
                                    "backup_ref": backup_ref,
                                    "next": "manual rebase required",
                                })
                            }
                        }
                    }
                } else {
                    // Default: report only, no mutation.
                    let report = if probe.clean {
                        json!({"conflicts": [], "conflict_count": 0, "report_truncated": false})
                    } else {
                        build_conflict_report(&runner, &merge_base, &head, &upstream, &probe)
                    };
                    json!({
                        "state": "diverged",
                        "merge_base": merge_base,
                        "ahead": ahead,
                        "behind": behind,
                        "clean_rebase_possible": probe.clean,
                        "conflicts": report["conflicts"],
                        "conflict_count": report["conflict_count"],
                        "report_truncated": report["report_truncated"],
                        "non_conflicting": non_conflicting_paths(root, &merge_base, &head, &upstream),
                        "backup_ref": Value::Null,
                        "next": "use strategy=rebase-if-clean to auto-rebase",
                    })
                }
            }
        }
        _ => json!({"state": "unknown"}),
    };

    journal.complete(&request_id, &repo_key, result.clone())?;
    let _ = lock.release();
    Ok(result)
}

fn classify_counts(runner: &GitRunner, upstream: &str) -> (u64, u64) {
    let counts = runner.run_opt(&[
        "rev-list",
        "--left-right",
        "--count",
        &format!("HEAD...{upstream}"),
    ]);
    match counts {
        Some(out) => {
            let s = String::from_utf8_lossy(&out).trim().to_string();
            let parts: Vec<&str> = s.split_whitespace().collect();
            if parts.len() == 2 {
                (
                    parts[0].parse::<u64>().unwrap_or(0),
                    parts[1].parse::<u64>().unwrap_or(0),
                )
            } else {
                (0, 0)
            }
        }
        None => (0, 0),
    }
}

fn classify_state(ahead: u64, behind: u64) -> &'static str {
    match (ahead, behind) {
        (0, 0) => "up_to_date",
        (0, _) => "fast_forward",
        (_, 0) => "ahead",
        (_, _) => "diverged",
    }
}

enum LeaseOutcome {
    Pushed,
    Raced { error: String, ahead: u64, behind: u64 },
}

/// Leased push with `--force-with-lease=<branch>:<lease_oid>`, where
/// `lease_oid` is the OID *this call's own fetch* just observed — the
/// one-call design makes the lease airtight (no separately-transcribed OID
/// that could go stale between a caller's read and its push).
///
/// On any push failure (most commonly the remote having moved since our
/// fetch, i.e. a lease rejection), this performs exactly ONE re-fetch of the
/// branch plus one reclassification and returns that as data — it never
/// retries the push itself. A second blind retry is exactly the failure mode
/// this function exists to rule out.
fn attempt_lease_push_or_reclassify(
    runner: &GitRunner,
    branch: &str,
    lease_oid: &str,
    upstream: &str,
) -> LeaseOutcome {
    let lease_arg = format!("--force-with-lease={branch}:{lease_oid}");
    match runner.run(&["push", &lease_arg, "origin", branch]) {
        Ok(_) => LeaseOutcome::Pushed,
        Err(e) => {
            let _ = runner.run(&["fetch", "--end-of-options", "origin", branch]);
            let (a2, b2) = classify_counts(runner, upstream);
            LeaseOutcome::Raced { error: e.to_string(), ahead: a2, behind: b2 }
        }
    }
}

/// `git --version` >= 2.38 gate for `merge-tree --write-tree` (the "real
/// merge" mode this op depends on for in-memory conflict prediction). Older
/// git either lacks `--write-tree` entirely or predates its stabilized
/// output format, so callers must not attempt to interpret its output.
fn git_supports_merge_tree_write_tree(root: &Path) -> bool {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("--version")
        .output();
    let Ok(out) = out else { return false };
    if !out.status.success() {
        return false;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    // "git version 2.55.0" (possibly with a vendor/build suffix).
    let Some(ver_field) = s.split_whitespace().nth(2) else { return false };
    let mut parts = ver_field.split('.');
    let Some(major) = parts.next().and_then(|p| p.parse::<u32>().ok()) else { return false };
    let minor = parts.next().and_then(|p| p.parse::<u32>().ok()).unwrap_or(0);
    (major, minor) >= (2, 38)
}

struct MergeTreeProbe {
    /// True iff `merge-tree --write-tree` exited 0 (no textual conflicts).
    clean: bool,
    /// Raw stdout of `merge-tree --write-tree --no-messages <ours> <theirs>`
    /// — tree oid line, then (if conflicted) one `<mode> <oid> <stage>\t<path>`
    /// line per conflicted stage entry.
    stage_lines: String,
    /// Raw stdout of `merge-tree --write-tree <ours> <theirs>` (messages
    /// included) — used to recover human-readable `CONFLICT (<kind>): ...`
    /// text per path.
    messages: String,
}

/// Runs `git merge-tree --write-tree` directly via `std::process::Command`
/// rather than through `GitRunner`. This is deliberate: `merge-tree` exits 1
/// (not 0) precisely when there ARE conflicts, and `GitRunner::run`/`run_opt`
/// both discard stdout whenever the subprocess exits non-zero (`run_opt`
/// maps a non-zero exit straight to `None` — see `pixel-git::runner::execute`).
/// That meant the previous implementation of this probe always observed
/// `None` on the exact inputs it most needed to inspect, so the resulting
/// conflict report was unconditionally empty on every real conflict — the
/// exact defect class PLAN.md calls out (`review`'s `!conflicted` filtering)
/// that this op must not repeat. Capturing stdout unconditionally, keyed off
/// the real exit status instead of `Result::ok()`, is what actually fixes it.
fn probe_merge_tree(root: &Path, ours: &str, theirs: &str) -> MergeTreeProbe {
    let run = |extra: &[&str]| -> (bool, String) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .arg("merge-tree")
            .arg("--write-tree")
            .args(extra)
            .arg(ours)
            .arg(theirs)
            .output();
        match out {
            Ok(o) => (o.status.success(), String::from_utf8_lossy(&o.stdout).into_owned()),
            Err(_) => (false, String::new()),
        }
    };
    let (clean, stage_lines) = run(&["--no-messages"]);
    let (_, messages) = run(&[]);
    MergeTreeProbe { clean, stage_lines, messages }
}

/// Parses `merge-tree --write-tree --no-messages` stdout into
/// `path -> (stage -> oid)`. Stage 1 = merge base, 2 = ours, 3 = theirs; a
/// missing stage means that side has no version of the path (e.g. add/add
/// conflicts have no stage 1, modify/delete conflicts have no stage 3 when
/// the file was deleted on the "theirs" side).
fn parse_stage_lines(text: &str) -> BTreeMap<String, BTreeMap<u8, String>> {
    let mut map: BTreeMap<String, BTreeMap<u8, String>> = BTreeMap::new();
    for line in text.lines().skip(1) {
        // skip the leading tree-oid line
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, '\t');
        let Some(meta) = parts.next() else { continue };
        let Some(path) = parts.next() else { continue };
        let mut fields = meta.split_whitespace();
        let Some(_mode) = fields.next() else { continue };
        let Some(oid) = fields.next() else { continue };
        let Some(stage) = fields.next().and_then(|s| s.parse::<u8>().ok()) else { continue };
        map.entry(path.to_string()).or_default().insert(stage, oid.to_string());
    }
    map
}

/// Parses `CONFLICT (<kind>): <message>` lines out of merge-tree's
/// informational message section.
fn parse_conflict_kinds(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            let rest = l.strip_prefix("CONFLICT (")?;
            let idx = rest.find(')')?;
            let kind = rest[..idx].to_string();
            let msg = rest[idx + 1..].trim_start_matches(':').trim().to_string();
            Some((kind, msg))
        })
        .collect()
}

/// Truncates `s` to at most `cap` bytes on a UTF-8 char boundary, reporting
/// whether truncation happened.
fn capped(mut s: String, cap: usize) -> (String, bool) {
    if s.len() <= cap {
        return (s, false);
    }
    s.truncate(cap);
    while !s.is_char_boundary(s.len()) {
        s.pop();
    }
    (s, true)
}

/// `git diff --unified=0 <merge_base> <side_ref> -- <path>`, capped at
/// `HUNK_MAX_BYTES`. `None` when there is no merge base (unrelated
/// histories) or the diff could not be produced.
fn side_hunk(runner: &GitRunner, merge_base: &str, side_ref: &str, path: &str) -> (Option<String>, bool) {
    if merge_base.is_empty() {
        return (None, false);
    }
    match runner.run_opt(&["diff", "--no-color", "--unified=0", merge_base, side_ref, "--", path]) {
        Some(bytes) => {
            let (text, truncated) = capped(String::from_utf8_lossy(&bytes).into_owned(), HUNK_MAX_BYTES);
            (Some(text), truncated)
        }
        None => (None, false),
    }
}

/// Extracts the base-side span (`"<start>,<count>"`) from a hunk's
/// `@@ -a,b +c,d @@` header, if present.
fn extract_base_span(hunk: &Option<String>) -> Option<String> {
    let h = hunk.as_ref()?;
    for line in h.lines() {
        if let Some(rest) = line.strip_prefix("@@ -")
            && let Some(end) = rest.find(" +")
        {
            return Some(rest[..end].to_string());
        }
    }
    None
}

/// Builds the full conflict report — this is the fix for the known defect
/// class in the predecessor tool's `review` op, which silently filtered
/// conflicted paths out of its response. Every path merge-tree reports as
/// conflicted MUST be present here, with per-side oid/hunk detail and a
/// parsed `conflict_kind`, capped per-path (`HUNK_MAX_BYTES`) and in total
/// (`REPORT_MAX_BYTES`) with truncation flagged rather than silently dropped.
fn build_conflict_report(
    runner: &GitRunner,
    merge_base: &str,
    head: &str,
    upstream: &str,
    probe: &MergeTreeProbe,
) -> Value {
    let stage_map = parse_stage_lines(&probe.stage_lines);
    let kinds = parse_conflict_kinds(&probe.messages);

    let mut conflicts = Vec::new();
    let mut total_bytes = 0usize;
    let mut report_truncated = false;

    for (path, stages) in &stage_map {
        if total_bytes >= REPORT_MAX_BYTES {
            report_truncated = true;
            break;
        }
        let base_oid = stages.get(&1).cloned();
        let ours_oid = stages.get(&2).cloned();
        let theirs_oid = stages.get(&3).cloned();

        let (ours_hunk, ours_truncated) = side_hunk(runner, merge_base, head, path);
        let (theirs_hunk, theirs_truncated) = side_hunk(runner, merge_base, upstream, path);
        let base_span = extract_base_span(&ours_hunk).or_else(|| extract_base_span(&theirs_hunk));

        let conflict_kind = kinds
            .iter()
            .find(|(_, msg)| msg.contains(path.as_str()))
            .map(|(k, _)| k.clone())
            .unwrap_or_else(|| "content".to_string());

        let entry = json!({
            "path": path,
            "conflict_kind": conflict_kind,
            "base_span": base_span,
            "base_oid": base_oid,
            "ours": {
                "oid": ours_oid,
                "hunk": ours_hunk,
                "hunk_truncated": ours_truncated,
            },
            "theirs": {
                "oid": theirs_oid,
                "hunk": theirs_hunk,
                "hunk_truncated": theirs_truncated,
            },
        });
        total_bytes = total_bytes.saturating_add(entry.to_string().len());
        conflicts.push(entry);
    }

    json!({
        "conflicts": conflicts,
        "conflict_count": stage_map.len(),
        "report_truncated": report_truncated,
    })
}

/// Paths changed on exactly one side *relative to the merge base* — a path
/// changed on both sides isn't "non-conflicting" (it's either a real
/// conflict, tracked separately in `conflicts`, or coincidentally identical
/// on both sides), so this belongs here, not in the conflict report.
///
/// Deliberately diffs each side against `merge_base`, NOT against each
/// other: `diff_name_status(ours, theirs)` and `diff_name_status(theirs,
/// ours)` report the exact same set of changed paths (just with swapped
/// A/D status letters for adds/deletes) since a raw two-tree diff is
/// direction-symmetric in its path set — so a first version of this
/// function that diffed the two sides directly against each other always
/// computed `ours_only == theirs_only == {}`, silently dropping the
/// "which side introduced this file" signal the report schema promises.
fn non_conflicting_paths(root: &Path, merge_base: &str, ours: &str, theirs: &str) -> Value {
    let runner = GitRunner::new(root);
    if merge_base.is_empty() {
        return json!({"ours_only_paths": [], "theirs_only_paths": []});
    }
    let ours_changes = runner.diff_name_status(merge_base, ours);
    let theirs_changes = runner.diff_name_status(merge_base, theirs);

    let ours_paths: std::collections::HashSet<String> = ours_changes.iter().map(|(_, p)| p.clone()).collect();
    let theirs_paths: std::collections::HashSet<String> = theirs_changes.iter().map(|(_, p)| p.clone()).collect();

    let ours_only: Vec<String> = ours_paths.difference(&theirs_paths).cloned().collect();
    let theirs_only: Vec<String> = theirs_paths.difference(&ours_paths).cloned().collect();

    json!({
        "ours_only_paths": ours_only,
        "theirs_only_paths": theirs_only,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn init_repo_with_remote(root: &Path, remote: &Path) {
        std::process::Command::new("git").arg("init").arg("-q").arg("-b").arg("main").arg(root).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(root).args(["config", "user.email", "t@t"]).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(root).args(["config", "user.name", "t"]).status().unwrap();
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        std::process::Command::new("git").arg("-C").arg(root).args(["add", "."]).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(root).args(["commit", "-qm", "init"]).status().unwrap();
        std::process::Command::new("git").arg("init").arg("--bare").arg("-q").arg(remote).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(root).args(["remote", "add", "origin", remote.to_str().unwrap()]).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(root).args(["push", "-u", "origin", "main"]).status().unwrap();
    }

    #[test]
    fn reconcile_up_to_date() {
        let dir = tempdir().unwrap();
        let remote = tempdir().unwrap();
        init_repo_with_remote(dir.path(), remote.path());

        let opts = ReconcileOptions {
            strategy: "report".to_string(),
            push: "none".to_string(),
            request_id: format!("rec-{}", uuid::Uuid::new_v4()),
        };
        let result = reconcile(dir.path(), &opts).unwrap();
        assert_eq!(result["state"], json!("up_to_date"));
    }

    #[test]
    fn reconcile_fast_forward() {
        let dir = tempdir().unwrap();
        let remote = tempdir().unwrap();
        init_repo_with_remote(dir.path(), remote.path());

        // Make a new commit on the remote.
        let clone_dir = tempdir().unwrap();
        std::process::Command::new("git").arg("clone").arg("-q").arg(remote.path()).arg(clone_dir.path()).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(clone_dir.path()).args(["config", "user.email", "t@t"]).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(clone_dir.path()).args(["config", "user.name", "t"]).status().unwrap();
        std::fs::write(clone_dir.path().join("b.txt"), b"b").unwrap();
        std::process::Command::new("git").arg("-C").arg(clone_dir.path()).args(["add", "."]).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(clone_dir.path()).args(["commit", "-qm", "remote commit"]).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(clone_dir.path()).args(["push"]).status().unwrap();

        let opts = ReconcileOptions {
            strategy: "report".to_string(),
            push: "none".to_string(),
            request_id: format!("rec-{}", uuid::Uuid::new_v4()),
        };
        let result = reconcile(dir.path(), &opts).unwrap();
        assert_eq!(result["state"], json!("fast_forwarded"));
    }

    #[test]
    fn reconcile_diverged_reports() {
        let dir = tempdir().unwrap();
        let remote = tempdir().unwrap();
        init_repo_with_remote(dir.path(), remote.path());

        // Diverge: local commit.
        std::fs::write(dir.path().join("local.txt"), b"local").unwrap();
        std::process::Command::new("git").arg("-C").arg(dir.path()).args(["add", "."]).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(dir.path()).args(["commit", "-qm", "local"]).status().unwrap();

        // Remote commit.
        let clone_dir = tempdir().unwrap();
        std::process::Command::new("git").arg("clone").arg("-q").arg(remote.path()).arg(clone_dir.path()).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(clone_dir.path()).args(["config", "user.email", "t@t"]).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(clone_dir.path()).args(["config", "user.name", "t"]).status().unwrap();
        std::fs::write(clone_dir.path().join("remote.txt"), b"remote").unwrap();
        std::process::Command::new("git").arg("-C").arg(clone_dir.path()).args(["add", "."]).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(clone_dir.path()).args(["commit", "-qm", "remote"]).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(clone_dir.path()).args(["push"]).status().unwrap();

        let opts = ReconcileOptions {
            strategy: "report".to_string(),
            push: "none".to_string(),
            request_id: format!("rec-{}", uuid::Uuid::new_v4()),
        };
        let result = reconcile(dir.path(), &opts).unwrap();
        assert_eq!(result["state"], json!("diverged"));
        assert!(result["ahead"].as_u64().unwrap() >= 1);
        assert!(result["behind"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn reconcile_defaults_empty_request_id() {
        // Regression: pixel-daemon's op_reconcile currently dispatches with
        // request_id == "" (pixel-proto's Reconcile request carries none).
        // journal.begin rejects an empty request id outright, so this must
        // not be allowed to reach the journal unmodified.
        let dir = tempdir().unwrap();
        let remote = tempdir().unwrap();
        init_repo_with_remote(dir.path(), remote.path());

        let opts = ReconcileOptions {
            strategy: "report".to_string(),
            push: "none".to_string(),
            request_id: String::new(),
        };
        let result = reconcile(dir.path(), &opts).unwrap();
        assert_eq!(result["state"], json!("up_to_date"));
    }
}
