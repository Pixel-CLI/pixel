//! `rewrite` — squash every commit on the current branch since a base into
//! ONE commit, with an optional leased force-push.
//!
//! Closes the biggest raw-git hole in the mutation surface: "squash into a
//! single commit and force push" / "organize commits" requests previously
//! forced `git reset --soft` + `git push --force` by hand.
//!
//! Crash-safety model (same discipline as `publish`/`push`):
//!   * repository lock + operation journal (`JournalOperation::Rewrite`),
//!   * a backup ref (`refs/pixel/rewrite-backup/<branch>`) written durably
//!     BEFORE any mutation — the sole basis for restoration,
//!   * journal phases: `started → ref_update_started (reset+commit window)
//!     → commit_observed → [push_started] → terminal`,
//!   * crash inside the reset+commit window → resume restores the branch to
//!     the recorded pre-rewrite head with `git reset --soft` (worktree and
//!     index untouched — `reset --soft` only moves HEAD, and the index
//!     content at both crash points equals the pre-rewrite tree content).
//!
//! Safety invariants:
//!   * refuse detached HEAD;
//!   * refuse rewriting the repo default branch (origin/HEAD or
//!     `init.defaultBranch` detection) unless `--onto` was explicit or
//!     `--allow-default-branch` was passed;
//!   * refuse when any squashed commit is already contained in the
//!     remote default branch — rewriting published mainline is forbidden
//!     unless `--allow-default-branch` was passed;
//!   * `expected_head` STALE_STATE gate like `publish`;
//!   * dirty worktree is tolerated (`reset --soft` never touches it) but
//!     recorded as a warning — pre-staged changes get absorbed into the
//!     squash commit;
//!   * the push step uses `--force-with-lease=<branch>:<old_remote_oid>`
//!     (the remote-tracking OID observed before the rewrite, same pattern
//!     as `reconcile::attempt_lease_push_or_reclassify`); a lease failure
//!     is classified as a structured `STALE_REMOTE` error in the result —
//!     NEVER retried with plain `--force`.

use std::path::Path;

use serde_json::{json, Value};

use pixel_git::GitRunner;

use crate::durable::{sha256_hex, state_root};
use crate::journal::{BeginOutcome, JournalOperation, JournalPhase, OperationJournal};
use crate::lock::RepositoryLock;

/// Options for a rewrite (squash) operation.
#[derive(Debug, Clone)]
pub struct RewriteOptions {
    /// Explicit base ref: squash everything in `<onto>..HEAD`. When absent,
    /// the base is the merge-base with the branch's upstream tracking ref,
    /// else the merge-base with the remote default branch (origin/HEAD).
    pub onto: Option<String>,
    /// Commit message for the squash commit. When absent, an auto-generated
    /// message is used: first line `squash: N commits`, body listing the
    /// squashed subjects oldest-first.
    pub message: Option<String>,
    /// Push the rewritten branch with `--force-with-lease` after squashing.
    pub push: bool,
    /// Remote name (default `origin`).
    pub remote: String,
    pub request_id: String,
    /// STALE_STATE gate: refuse unless HEAD is exactly this OID.
    pub expected_head: Option<String>,
    /// Explicitly allow rewriting the default branch and published mainline
    /// commits. Overrides both default-branch protection and published-
    /// mainline protection. The user has opted into rewriting shared history.
    pub allow_default_branch: bool,
}

/// A probe hook called at each phase. Used by tests to inject crashes.
/// Returns `Err` to simulate a crash at that point.
pub type RewriteProbe = Box<dyn FnMut(&str) -> Result<(), String>>;

pub fn rewrite(root: &Path, opts: &RewriteOptions) -> Result<Value, String> {
    let state_root = state_root();
    rewrite_with_state(root, opts, None, &state_root)
}

pub fn rewrite_with_state(
    root: &Path,
    opts: &RewriteOptions,
    probe: Option<RewriteProbe>,
    state_root: &Path,
) -> Result<Value, String> {
    let runner = GitRunner::new(root);
    let repo_key = repo_key(root);
    let input_hash = rewrite_input_hash(opts);

    let journal = OperationJournal::with_state_root(state_root.to_path_buf());

    // Same wiring-gap default as `reconcile`: an empty request_id would be
    // rejected outright by the journal; default to a fresh UUID so the op
    // is callable, at the cost of replay only working for callers that
    // supply a stable id.
    let request_id = if opts.request_id.is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        opts.request_id.clone()
    };

    let outcome = journal.begin(
        &request_id,
        JournalOperation::Rewrite,
        &repo_key,
        &input_hash,
    )?;

    match outcome {
        BeginOutcome::Replay(result) => Ok(result),
        BeginOutcome::Resume { phase, .. } => {
            resume_rewrite(root, opts, &request_id, &journal, phase, &runner, state_root)
        }
        BeginOutcome::Start => {
            run_body(root, opts, &request_id, &journal, &runner, state_root, probe)
        }
    }
}

fn run_body(
    root: &Path,
    opts: &RewriteOptions,
    request_id: &str,
    journal: &OperationJournal,
    runner: &GitRunner,
    state_root: &Path,
    mut probe: Option<RewriteProbe>,
) -> Result<Value, String> {
    let repo_key = repo_key(root);

    pixel_git::validate_ref(&opts.remote).map_err(|e| e.to_string())?;

    let mut lock = RepositoryLock::acquire_with_state_root(&common_dir(root), state_root)
        .map_err(|_| "repository is busy".to_string())?;

    macro_rules! bail {
        ($e:expr) => {{
            let _ = lock.release();
            return Err($e);
        }};
    }

    // Probe: journal:started
    if let Some(p) = probe.as_mut()
        && let Err(e) = p("journal:started")
    {
        bail!(e);
    }

    // Refuse detached HEAD.
    let Some(branch) = runner.current_branch() else {
        bail!("UNSUPPORTED_STATE: detached HEAD — rewrite requires a checked-out branch".to_string());
    };
    let Some(old_head) = runner.rev_parse_head() else {
        bail!("UNSUPPORTED_STATE: no HEAD commit".to_string());
    };

    // STALE_STATE: expected HEAD gate, same as publish.
    if let Some(expected) = &opts.expected_head
        && expected != &old_head
    {
        bail!(format!(
            "STALE_STATE: expected head {expected}, got {old_head}"
        ));
    }

    // Default-branch protection.
    let default_branch = detect_default_branch(runner, &opts.remote);
    if let Some(default) = &default_branch
        && &branch == default
        && opts.onto.is_none()
        && !opts.allow_default_branch
    {
        bail!(format!(
            "REFUSED: {branch} is the repository default branch; rewriting it is forbidden \
             unless an explicit --onto base is given or --allow-default-branch is passed \
             (and even then, published commits are never rewritten without \
             --allow-default-branch)"
        ));
    }

    // Resolve the base.
    let base_oid = match resolve_base(runner, &branch, &opts.onto, &opts.remote, &default_branch) {
        Ok(oid) => oid,
        Err(e) => bail!(e),
    };

    // Base must be a strict ancestor of HEAD.
    if base_oid == old_head {
        bail!(format!(
            "NOTHING_TO_SQUASH: base {base_oid} is already HEAD — no commits to squash \
             (if the branch's commits are all pushed to its upstream, pass an explicit \
             --onto <base-ref>, e.g. --onto {}/{})",
            opts.remote,
            default_branch.as_deref().unwrap_or("main"),
        ));
    }
    if runner
        .run(&["merge-base", "--is-ancestor", &base_oid, &old_head])
        .is_err()
    {
        bail!(format!(
            "REFUSED: base {base_oid} is not an ancestor of HEAD {old_head} — rewrite only \
             squashes a linear range; pick a base on this branch's history"
        ));
    }

    // Published-mainline protection: refuse if ANY commit being squashed is
    // already contained in the remote default branch.
    if let Some(default) = &default_branch
        && !opts.allow_default_branch
    {
        let remote_default = format!("refs/remotes/{}/{}", opts.remote, default);
        if runner
            .run_opt(&["rev-parse", "--verify", "--quiet", &remote_default])
            .is_some()
        {
            let total = rev_list_count(runner, &[&format!("{base_oid}..{old_head}")]);
            let outside = rev_list_count(
                runner,
                &[&format!("{base_oid}..{old_head}"), &format!("^{remote_default}")],
            );
            if outside < total {
                bail!(format!(
                    "REFUSED: {} of the {} commits to squash are already contained in {} — \
                     rewriting published mainline history is forbidden; pass \
                     --allow-default-branch to override",
                    total - outside,
                    total,
                    remote_default,
                ));
            }
        }
    }

    // Gather what we are squashing (subjects oldest-first for the message).
    let commits_squashed = rev_list_count(runner, &[&format!("{base_oid}..{old_head}")]);
    let subjects: Vec<String> = runner
        .run_opt(&["log", "--reverse", "--format=%s", &format!("{base_oid}..{old_head}")])
        .map(|o| {
            String::from_utf8_lossy(&o)
                .lines()
                .map(|l| l.to_string())
                .collect()
        })
        .unwrap_or_default();
    let message = opts.message.clone().unwrap_or_else(|| {
        let mut m = format!("squash: {commits_squashed} commits\n");
        for s in &subjects {
            m.push_str(&format!("\n- {s}"));
        }
        m
    });

    // Remote-tracking OID of THIS branch before the rewrite — the lease
    // expectation for the push step. Empty when the branch was never pushed
    // (an empty lease expectation means "the remote ref must not exist").
    let remote_branch_ref = format!("refs/remotes/{}/{}", opts.remote, branch);
    let old_remote_oid = runner
        .run_opt(&["rev-parse", "--verify", "--quiet", &remote_branch_ref])
        .map(|o| String::from_utf8_lossy(&o).trim().to_string())
        .unwrap_or_default();

    // Dirty worktree: tolerated (reset --soft never touches worktree or
    // index), but warn — pre-staged changes will be absorbed into the
    // squash commit.
    let mut warnings: Vec<String> = Vec::new();
    let dirty = runner.status_porcelain();
    if !dirty.is_empty() {
        warnings.push(format!(
            "worktree has {} dirty path(s); reset --soft preserves them, but any \
             pre-staged changes are absorbed into the squash commit",
            dirty.len()
        ));
    }

    // Backup ref FIRST, before any mutation (same style as reconcile's
    // refs/pixel/reconcile-backup/<branch>).
    let backup_ref = format!("refs/pixel/rewrite-backup/{branch}");
    if let Err(e) = runner.run(&["update-ref", &backup_ref, &old_head]) {
        bail!(format!("git update-ref (backup): {e}"));
    }

    // Probe: backup:written
    if let Some(p) = probe.as_mut()
        && let Err(e) = p("backup:written")
    {
        bail!(e);
    }

    // Enter the ambiguous reset+commit window: journal the recovery
    // metadata durably BEFORE mutating. On crash anywhere inside this
    // window, resume restores `old_head` via `reset --soft` (see
    // `resume_rewrite`).
    journal.transition(
        request_id,
        &repo_key,
        JournalPhase::RefUpdateStarted,
        Some(json!({
            "old_head": old_head,
            "base_oid": base_oid,
            "branch": branch,
            "backup_ref": backup_ref,
        })),
    )?;

    // Probe: journal:ref_update_started
    if let Some(p) = probe.as_mut()
        && let Err(e) = p("journal:ref_update_started")
    {
        bail!(e);
    }

    // The squash: soft-reset to base (HEAD moves, index + worktree stay),
    // then one plain commit of the accumulated diff.
    if let Err(e) = runner.run(&["reset", "--soft", &base_oid]) {
        bail!(format!("git reset --soft: {e}"));
    }

    // Probe: reset:done
    if let Some(p) = probe.as_mut()
        && let Err(e) = p("reset:done")
    {
        bail!(e);
    }

    if let Err(e) = runner.run(&["commit", "-m", &message]) {
        // Restore the branch before reporting — never leave HEAD parked at
        // the base with the whole range staged.
        let _ = runner.run(&["reset", "--soft", &old_head]);
        bail!(format!(
            "git commit failed during squash; branch restored to {old_head} \
             (backup ref {backup_ref} also points there): {e}"
        ));
    }

    let Some(new_head) = runner.rev_parse_head() else {
        bail!("git rev-parse HEAD failed after squash commit".to_string());
    };

    journal.transition(
        request_id,
        &repo_key,
        JournalPhase::CommitObserved,
        Some(json!({
            "old_head": old_head,
            "base_oid": base_oid,
            "branch": branch,
            "backup_ref": backup_ref,
            "new_head": new_head,
            "commits_squashed": commits_squashed,
        })),
    )?;

    // Probe: journal:commit_observed
    if let Some(p) = probe.as_mut()
        && let Err(e) = p("journal:commit_observed")
    {
        bail!(e);
    }

    // Push step: exact reconcile lease pattern. Lease against the
    // remote-tracking OID observed BEFORE the rewrite; on failure, classify
    // as STALE_REMOTE — never retry with plain --force.
    let mut pushed = false;
    let mut push_error: Option<String> = None;
    if opts.push {
        journal.transition(request_id, &repo_key, JournalPhase::PushStarted, None)?;
        // Probe: journal:push_started
        if let Some(p) = probe.as_mut()
            && let Err(e) = p("journal:push_started")
        {
            bail!(e);
        }
        let lease_arg = format!("--force-with-lease={branch}:{old_remote_oid}");
        match runner.run(&["push", &opts.remote, &branch, &lease_arg]) {
            Ok(_) => pushed = true,
            Err(e) => {
                push_error = Some(format!(
                    "STALE_REMOTE: leased push rejected — the remote {}/{branch} no longer \
                     matches the pre-rewrite OID {}; someone pushed since. The local squash \
                     succeeded; fetch/reconcile, then push with a fresh lease. Never retried \
                     with plain --force. ({e})",
                    opts.remote,
                    if old_remote_oid.is_empty() { "<absent>" } else { &old_remote_oid },
                ));
            }
        }
    }

    let mut result = json!({
        "state": "squashed",
        "branch": branch,
        "base_oid": base_oid,
        "old_head": old_head,
        "new_head": new_head,
        "commits_squashed": commits_squashed,
        "backup_ref": backup_ref,
        "pushed": pushed,
        "warnings": warnings,
    });
    if let Some(pe) = push_error {
        result["push_error"] = json!(pe);
    }

    journal.complete(request_id, &repo_key, result.clone())?;

    // Probe: journal:terminal
    if let Some(p) = probe.as_mut()
        && let Err(e) = p("journal:terminal")
    {
        bail!(e);
    }

    let _ = lock.release();
    Ok(result)
}

/// Resume after a crash, keyed off the journaled phase.
fn resume_rewrite(
    root: &Path,
    opts: &RewriteOptions,
    request_id: &str,
    journal: &OperationJournal,
    phase: JournalPhase,
    runner: &GitRunner,
    state_root: &Path,
) -> Result<Value, String> {
    let repo_key = repo_key(root);
    match phase {
        JournalPhase::Started => {
            // Crash before the mutation window opened (the backup ref may
            // or may not exist, but nothing moved HEAD). Safe to run fresh.
            run_body(root, opts, request_id, journal, runner, state_root, None)
        }
        JournalPhase::RefUpdateStarted => {
            // Crash inside the reset+commit window. We cannot prove whether
            // the reset, the commit, both, or neither completed — but every
            // one of those states is restored by moving the branch back to
            // the recorded pre-rewrite head with `reset --soft` (worktree
            // and index are untouched by any of the possible crash states).
            let record = journal.read(&repo_key, request_id);
            let old_head = record
                .as_ref()
                .and_then(|r| r.result.as_ref())
                .and_then(|v| v.get("old_head"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    // Belt and braces: the backup ref carries the same OID.
                    let branch = runner.current_branch()?;
                    let backup = format!("refs/pixel/rewrite-backup/{branch}");
                    runner
                        .run_opt(&["rev-parse", "--verify", "--quiet", &backup])
                        .map(|o| String::from_utf8_lossy(&o).trim().to_string())
                })
                .ok_or("GIT_FAILED: crash detected mid-rewrite and no recovery metadata or backup ref found")?;
            runner
                .run(&["reset", "--soft", &old_head])
                .map_err(|e| format!("GIT_FAILED: restore from backup failed: {e}"))?;
            Err(format!(
                "GIT_FAILED: crash detected mid-rewrite; branch restored to the \
                 pre-rewrite head {old_head} (backup ref preserved)"
            ))
        }
        JournalPhase::CommitObserved => {
            // The squash commit durably exists; nothing ambiguous remains.
            // Complete without pushing (the push never started).
            let record = journal.read(&repo_key, request_id);
            let meta = record.and_then(|r| r.result).unwrap_or(json!({}));
            let result = json!({
                "state": "squashed",
                "branch": meta.get("branch").cloned().unwrap_or(Value::Null),
                "base_oid": meta.get("base_oid").cloned().unwrap_or(Value::Null),
                "old_head": meta.get("old_head").cloned().unwrap_or(Value::Null),
                "new_head": meta.get("new_head").cloned().unwrap_or(Value::Null),
                "commits_squashed": meta.get("commits_squashed").cloned().unwrap_or(Value::Null),
                "backup_ref": meta.get("backup_ref").cloned().unwrap_or(Value::Null),
                "pushed": false,
                "warnings": ["resumed after crash; push (if requested) was not attempted"],
            });
            journal.complete(request_id, &repo_key, result.clone())?;
            Ok(result)
        }
        JournalPhase::PushStarted => {
            Err("NETWORK_AMBIGUITY: push may have started before the crash, cannot safely retry — verify the remote, then push with a fresh lease".to_string())
        }
        JournalPhase::Terminal => {
            let record = journal.read(&repo_key, request_id);
            record
                .and_then(|r| r.result)
                .ok_or("journal record lost".to_string())
        }
        other => Err(format!("unexpected phase for rewrite: {other}")),
    }
}

/// Default-branch detection: `refs/remotes/<remote>/HEAD` symbolic ref
/// first, then `init.defaultBranch` config. `None` when neither resolves.
fn detect_default_branch(runner: &GitRunner, remote: &str) -> Option<String> {
    let prefix = format!("refs/remotes/{remote}/");
    if let Some(out) = runner.run_opt(&["symbolic-ref", "--quiet", &format!("{prefix}HEAD")]) {
        let s = String::from_utf8_lossy(&out).trim().to_string();
        if let Some(name) = s.strip_prefix(&prefix)
            && !name.is_empty()
        {
            return Some(name.to_string());
        }
    }
    if let Some(out) = runner.run_opt(&["config", "init.defaultBranch"]) {
        let s = String::from_utf8_lossy(&out).trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

/// Resolve the squash base to a full OID.
fn resolve_base(
    runner: &GitRunner,
    branch: &str,
    onto: &Option<String>,
    remote: &str,
    default_branch: &Option<String>,
) -> Result<String, String> {
    if let Some(onto) = onto {
        let spec = format!("{onto}^{{commit}}");
        return runner
            .run_opt(&["rev-parse", "--verify", "--quiet", &spec])
            .map(|o| String::from_utf8_lossy(&o).trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or(format!("REFUSED: --onto {onto} does not resolve to a commit"));
    }

    // Upstream tracking base. A branch tracking its OWN remote ref
    // (<remote>/<branch>) that is fully pushed resolves to base == HEAD,
    // which would refuse with NOTHING_TO_SQUASH — but "squash my pushed
    // messy branch" is the #1 real use case, so that self-tracking case
    // falls through to the remote-default-branch base instead.
    if let Some(out) = runner.run_opt(&[
        "rev-parse",
        "--abbrev-ref",
        "--symbolic-full-name",
        &format!("{branch}@{{upstream}}"),
    ]) {
        let upstream = String::from_utf8_lossy(&out).trim().to_string();
        if !upstream.is_empty()
            && let Some(mb) = runner
                .run_opt(&["merge-base", "HEAD", &upstream])
                .map(|o| String::from_utf8_lossy(&o).trim().to_string())
                .filter(|s| !s.is_empty())
        {
            let head_oid = runner
                .run_opt(&["rev-parse", "HEAD"])
                .map(|o| String::from_utf8_lossy(&o).trim().to_string())
                .unwrap_or_default();
            let self_tracking = upstream == format!("{remote}/{branch}");
            if !(self_tracking && mb == head_oid) {
                return Ok(mb);
            }
        }
    }

    // Merge-base with the remote default branch.
    if let Some(default) = default_branch {
        let remote_default = format!("refs/remotes/{remote}/{default}");
        if runner
            .run_opt(&["rev-parse", "--verify", "--quiet", &remote_default])
            .is_some()
            && let Some(mb) = runner
                .run_opt(&["merge-base", "HEAD", &remote_default])
                .map(|o| String::from_utf8_lossy(&o).trim().to_string())
                .filter(|s| !s.is_empty())
        {
            return Ok(mb);
        }
    }

    Err(format!(
        "REFUSED: no base resolvable for {branch} — it has no upstream tracking ref and no \
         merge-base with the remote default branch; pass an explicit --onto <base-ref>"
    ))
}

fn rev_list_count(runner: &GitRunner, ranges: &[&str]) -> u64 {
    let mut args: Vec<&str> = vec!["rev-list", "--count"];
    args.extend_from_slice(ranges);
    runner
        .run_opt(&args)
        .and_then(|o| String::from_utf8_lossy(&o).trim().parse::<u64>().ok())
        .unwrap_or(0)
}

fn repo_key(root: &Path) -> String {
    root.canonicalize()
        .unwrap_or_else(|_| root.to_path_buf())
        .display()
        .to_string()
}

fn common_dir(root: &Path) -> String {
    root.join(".git").display().to_string()
}

fn rewrite_input_hash(opts: &RewriteOptions) -> String {
    sha256_hex(&format!(
        "{}\u{0}{}\u{0}{}\u{0}{}\u{0}{}\u{0}{}",
        opts.onto.as_deref().unwrap_or(""),
        opts.message.as_deref().unwrap_or(""),
        opts.push,
        opts.remote,
        opts.expected_head.as_deref().unwrap_or(""),
        opts.allow_default_branch,
    ))
}
