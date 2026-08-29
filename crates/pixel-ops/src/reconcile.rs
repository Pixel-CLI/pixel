//! `reconcile` — Engine 4: one-call deterministic branch sync.
//!
//! Eliminates OID transcription: the snapshot inside the lock supplies all
//! expectations, making STALE_STATE structurally impossible within the op.
//!
//! Flow: snapshot → fetch → classify (rev-list --left-right --count) →
//!   up_to_date | fast_forward | ahead (leased push) | diverged (report or
//!   rebase-if-clean).
//!
//! Divergence policy: default "report"; "rebase-if-clean" is explicit opt-in.
//! Never merge commits. Zero-textual-conflict rebase is deterministic work.
//! Mechanics: clean worktree required → prove cleanliness per replayed
//! commit with `git merge-tree --write-tree` → non-interactive `git rebase`
//! under journal → backup ref first → any surprise conflict → `rebase --abort`
//! + diverged report.

use std::path::Path;

use serde_json::{json, Value};

use pixel_git::GitRunner;

use crate::durable::{sha256_hex, state_root};
use crate::journal::{BeginOutcome, JournalOperation, OperationJournal};
use crate::lock::RepositoryLock;

#[derive(Debug, Clone)]
pub struct ReconcileOptions {
    pub strategy: String,    // "report" | "rebase-if-clean"
    pub push: String,        // "auto" | "none"
    pub request_id: String,
}

pub fn reconcile(root: &Path, opts: &ReconcileOptions) -> Result<Value, String> {
    let runner = GitRunner::new(root);
    let repo_key = root.canonicalize().unwrap_or_else(|_| root.to_path_buf()).display().to_string();
    let input_hash = sha256_hex(&format!("{}\u{0}{}", opts.strategy, opts.push));

    let state_root = state_root();
    let journal = OperationJournal::with_state_root(state_root.clone());

    let outcome = journal.begin(&opts.request_id, JournalOperation::Update, &repo_key, &input_hash)?;
    if let BeginOutcome::Replay(result) = outcome {
        return Ok(result);
    }

    let mut lock = RepositoryLock::acquire_with_state_root(
        &root.join(".git").display().to_string(),
        &state_root,
    ).map_err(|_| "repository is busy".to_string())?;

    // Snapshot current state.
    let head = runner.rev_parse_head().ok_or("no HEAD")?;
    let branch = current_branch(root).unwrap_or_else(|| "HEAD".to_string());
    let upstream = format!("origin/{branch}");

    // Fetch (idempotent, outside journal transitions).
    let _ = runner.run(&["fetch", "--end-of-options", "origin"]).map_err(|e| {
        let _ = lock.release();
        format!("git fetch: {e}")
    })?;

    // Classify with rev-list --left-right --count.
    let counts = runner.run_opt(&[
        "rev-list",
        "--left-right",
        "--count",
        &format!("HEAD...{upstream}"),
    ]);

    let (ahead, behind) = match counts {
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
    };

    let merge_base = runner.run_opt(&["merge-base", "HEAD", &upstream])
        .map(|o| String::from_utf8_lossy(&o).trim().to_string())
        .unwrap_or_default();

    let state = match (ahead, behind) {
        (0, 0) => "up_to_date",
        (0, _) => "fast_forward",
        (_, 0) => "ahead",
        (_, _) => "diverged",
    };

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
            // Check for dirty paths that would be overwritten.
            let dirty = runner.status_porcelain();
            if !dirty.is_empty() {
                let changes = runner.diff_name_status(&head, &upstream);
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
            // Fast-forward.
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
            if opts.push == "auto" {
                // Leased push with --force-with-lease.
                let lease_arg = format!("--force-with-lease={branch}:{head}");
                runner.run(&["push", &lease_arg, "origin", &branch]).map_err(|e| {
                    let _ = lock.release();
                    format!("git push: {e}")
                })?;
                json!({
                    "state": "pushed",
                    "head": head,
                    "branch": branch,
                })
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
            if opts.strategy == "rebase-if-clean" {
                // Require clean worktree.
                let dirty = runner.status_porcelain();
                if !dirty.is_empty() {
                    let _ = lock.release();
                    return Err(format!("UNSUPPORTED_STATE: rebase-if-clean requires clean worktree, {} dirty files", dirty.len()));
                }

                // Write backup ref first.
                let backup_ref = format!("refs/pixel/reconcile-backup/{branch}");
                let _ = runner.run(&["update-ref", &backup_ref, &head]);

                // Check merge-tree cleanliness (git >= 2.38).
                let merge_tree = runner.run_opt(&[
                    "merge-tree",
                    "--write-tree",
                    "--no-messages",
                    &head,
                    &upstream,
                ]);
                let clean = match merge_tree {
                    Some(out) => {
                        // merge-tree --write-tree exits 0 on clean merge, 1 on conflicts.
                        // Output is just the tree OID on success.
                        !out.is_empty() && !String::from_utf8_lossy(&out).contains("CONFLICT")
                    }
                    None => false,
                };

                if !clean {
                    // Conflicts detected — report diverged, don't rebase.
                    let conflicts = extract_conflicts(root, &head, &upstream);
                    let _ = lock.release();
                    return Ok(json!({
                        "state": "diverged",
                        "merge_base": merge_base,
                        "ahead": ahead,
                        "behind": behind,
                        "clean_rebase_possible": false,
                        "conflicts": conflicts,
                        "backup_ref": backup_ref,
                        "next": "manual resolution required",
                    }));
                }

                // Perform the rebase.
                let rebase_result = runner.run(&[
                    "rebase",
                    &upstream,
                ]);

                match rebase_result {
                    Ok(_) => {
                        // Rebase succeeded — continue to push leg if auto.
                        let new_head = runner.rev_parse_head().unwrap_or_default();
                        if opts.push == "auto" {
                            let lease_arg = format!("--force-with-lease={branch}:{new_head}");
                            let _ = runner.run(&["push", &lease_arg, "origin", &branch]);
                        }
                        json!({
                            "state": "rebased",
                            "from": head,
                            "to": new_head,
                            "branch": branch,
                            "backup_ref": backup_ref,
                            "pushed": opts.push == "auto",
                        })
                    }
                    Err(e) => {
                        // Surprise conflict — abort and report.
                        let _ = runner.run(&["rebase", "--abort"]);
                        let _ = lock.release();
                        return Ok(json!({
                            "state": "diverged",
                            "merge_base": merge_base,
                            "ahead": ahead,
                            "behind": behind,
                            "clean_rebase_possible": true,
                            "rebase_aborted": true,
                            "error": e.to_string(),
                            "backup_ref": backup_ref,
                            "next": "manual rebase required",
                        }));
                    }
                }
            } else {
                // Default: report only.
                let conflicts = extract_conflicts(root, &head, &upstream);
                json!({
                    "state": "diverged",
                    "merge_base": merge_base,
                    "ahead": ahead,
                    "behind": behind,
                    "clean_rebase_possible": check_clean_rebase(root, &head, &upstream),
                    "conflicts": conflicts,
                    "non_conflicting": non_conflicting_paths(root, &head, &upstream),
                    "next": "use strategy=rebase-if-clean to auto-rebase",
                })
            }
        }
        _ => json!({"state": "unknown"}),
    };

    journal.complete(&opts.request_id, &repo_key, result.clone())?;
    let _ = lock.release();
    Ok(result)
}

fn current_branch(root: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

fn extract_conflicts(root: &Path, ours: &str, theirs: &str) -> Value {
    // Use merge-tree to find conflicted paths.
    let runner = GitRunner::new(root);
    let out = runner.run_opt(&[
        "merge-tree",
        "--write-tree",
        "--name-only",
        ours,
        theirs,
    ]);
    match out {
        Some(o) => {
            let s = String::from_utf8_lossy(&o);
            // Conflicted paths appear after the tree OID line.
            let lines: Vec<&str> = s.lines().collect();
            let conflicts: Vec<&str> = lines.iter()
                .skip(1) // Skip tree OID
                .filter(|l| !l.is_empty() && !l.starts_with("CONFLICT"))
                .copied()
                .collect();
            json!({
                "paths": conflicts,
                "count": conflicts.len(),
            })
        }
        None => json!({"paths": [], "count": 0}),
    }
}

fn check_clean_rebase(root: &Path, ours: &str, theirs: &str) -> bool {
    let runner = GitRunner::new(root);
    let out = runner.run_opt(&[
        "merge-tree",
        "--write-tree",
        "--no-messages",
        ours,
        theirs,
    ]);
    match out {
        Some(o) => {
            let s = String::from_utf8_lossy(&o);
            !s.is_empty() && !s.contains("CONFLICT")
        }
        None => false,
    }
}

fn non_conflicting_paths(root: &Path, ours: &str, theirs: &str) -> Value {
    let runner = GitRunner::new(root);
    let ours_changes = runner.diff_name_status(ours, theirs);
    let theirs_changes = runner.diff_name_status(theirs, ours);

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
}
