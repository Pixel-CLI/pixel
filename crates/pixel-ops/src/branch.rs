// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `branch` — create a new branch from the current HEAD and switch to it.
//!
//! Switching is not optional: without it, a caller that creates a branch and
//! then calls `publish` commits onto whatever was checked out before —
//! silently landing work on `main` instead of the requested branch. This bit
//! a real agent workflow (branch -> publish -> push), so `branch` now mirrors
//! `git checkout -b` / `git switch -c`, not bare `git branch`.

use std::path::Path;

use serde_json::{Value, json};

use pixel_git::GitRunner;

use crate::durable::sha256_hex;
use crate::journal::{BeginOutcome, JournalOperation, JournalPhase, OperationJournal};
use crate::lock::RepositoryLock;
use crate::repo::repo_identity;

#[derive(Debug, Clone)]
pub struct BranchOptions {
    pub name: String,
    pub from: Option<String>, // base ref, defaults to HEAD
    pub request_id: String,
}

pub fn branch(root: &Path, opts: &BranchOptions) -> Result<Value, String> {
    branch_with_state(root, opts, &crate::durable::state_root(), || Ok(()))
}

fn branch_with_state(
    root: &Path,
    opts: &BranchOptions,
    state_root: &Path,
    mut applied: impl FnMut() -> Result<(), String>,
) -> Result<Value, String> {
    let runner = GitRunner::new(root);
    let repo_key = repo_identity(root);
    let input_hash = sha256_hex(&format!(
        "{}\u{0}{}",
        opts.name,
        opts.from.as_deref().unwrap_or("")
    ));
    let journal = OperationJournal::with_state_root(state_root.to_path_buf());
    let _lock = RepositoryLock::acquire_with_state_root(&repo_key, state_root)
        .map_err(|_| "repository is busy".to_string())?;
    let outcome = journal.begin(
        &opts.request_id,
        JournalOperation::Branch,
        &repo_key,
        &input_hash,
    )?;
    if let BeginOutcome::Replay(result) = &outcome {
        return Ok(result.clone());
    }
    pixel_git::validate_ref(&opts.name).map_err(|e| e.to_string())?;
    let from = opts.from.as_deref().unwrap_or("HEAD");
    pixel_git::validate_ref(from).map_err(|e| e.to_string())?;
    let worktree = root.canonicalize().map_err(|e| e.to_string())?;
    let existing = runner.run_opt(&[
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("refs/heads/{}", opts.name),
    ]);
    let result = json!({"branch": opts.name, "from": from, "created": true, "checked_out": true});
    let target = if let BeginOutcome::Resume {
        phase: JournalPhase::RefUpdateStarted,
        result: Some(meta),
    } = outcome
    {
        let target = meta
            .get("target")
            .and_then(Value::as_str)
            .ok_or("GIT_FAILED: branch intent target missing")?
            .to_owned();
        if meta.get("worktree") != Some(&json!(worktree)) {
            return Err("RECOVERY_CONFLICT: branch request belongs to another worktree".into());
        }
        if existing.is_some() {
            if runner.current_branch().as_deref() != Some(&opts.name)
                || runner.rev_parse_head().as_deref() != Some(&target)
            {
                return Err("RECOVERY_CONFLICT: branch or HEAD changed after creation".into());
            }
            journal.complete(&opts.request_id, &repo_key, result.clone())?;
            return Ok(result);
        }
        if meta.get("pre_head") != Some(&json!(runner.rev_parse_head()))
            || meta.get("pre_branch") != Some(&json!(runner.current_branch()))
        {
            return Err("RECOVERY_CONFLICT: checkout source changed after branch intent".into());
        }
        target
    } else {
        if existing.is_some() {
            return Err(format!("REF_EXISTS: branch '{}' already exists", opts.name));
        }
        let target = runner
            .run(&["rev-parse", "--verify", &format!("{from}^{{commit}}")])
            .map_err(|e| format!("git resolve branch base: {e}"))?;
        let target = String::from_utf8_lossy(&target).trim().to_owned();
        journal.transition(
            &opts.request_id,
            &repo_key,
            JournalPhase::RefUpdateStarted,
            Some(json!({"target": target, "worktree": worktree,
                "pre_head": runner.rev_parse_head(), "pre_branch": runner.current_branch()})),
        )?;
        target
    };
    // Git creates the ref only if the checkout itself succeeds.
    runner
        .run(&["checkout", "-b", &opts.name, &target])
        .map_err(|e| format!("git checkout: {e}"))?;
    applied()?;
    journal.complete(&opts.request_id, &repo_key, result.clone())?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn init_repo(root: &Path) {
        // `-b main`: never rely on the machine's init.defaultBranch — a
        // host without that config initializes `master` and every
        // main-named assertion below silently diverges.
        std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .arg("-b")
            .arg("main")
            .arg(root)
            .status()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["config", "user.email", "t@t"])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["config", "user.name", "t"])
            .status()
            .unwrap();
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["add", "."])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["commit", "-qm", "init"])
            .status()
            .unwrap();
    }

    fn current_branch(root: &Path) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn branch_creates_new() {
        let dir = tempdir().unwrap();
        init_repo(dir.path());
        let opts = BranchOptions {
            name: "feature/test".to_string(),
            from: None,
            request_id: format!("br-{}", uuid::Uuid::new_v4()),
        };
        let result = branch(dir.path(), &opts).unwrap();
        assert_eq!(result["branch"], json!("feature/test"));
        assert_eq!(result["created"], json!(true));
    }

    #[test]
    fn branch_switches_head_to_the_new_branch() {
        // Regression: `branch` used to create the branch without checking it
        // out, so a caller doing branch -> publish committed onto whatever
        // was checked out before (main) instead of the requested branch.
        let dir = tempdir().unwrap();
        init_repo(dir.path());
        let starting_branch = current_branch(dir.path());
        let opts = BranchOptions {
            name: "feature/switch-test".to_string(),
            from: None,
            request_id: format!("br-{}", uuid::Uuid::new_v4()),
        };
        let result = branch(dir.path(), &opts).unwrap();
        assert_eq!(result["checked_out"], json!(true));
        assert_eq!(
            current_branch(dir.path()),
            "feature/switch-test",
            "HEAD must move to the new branch, not stay on {starting_branch}"
        );
    }

    #[test]
    fn branch_rejects_existing() {
        let dir = tempdir().unwrap();
        init_repo(dir.path());
        let opts = BranchOptions {
            name: "main".to_string(),
            from: None,
            request_id: format!("br-{}", uuid::Uuid::new_v4()),
        };
        let err = branch(dir.path(), &opts).unwrap_err();
        assert!(err.contains("REF_EXISTS"));
    }
    #[test]
    fn branch_should_leave_no_ref_when_checkout_is_refused() {
        let root = tempdir().unwrap();
        let state = tempdir().unwrap();
        init_repo(root.path());
        let runner = GitRunner::new(root.path());
        runner.run(&["checkout", "-qb", "target"]).unwrap();
        std::fs::write(root.path().join("a.txt"), "target").unwrap();
        runner.run(&["commit", "-qam", "target"]).unwrap();
        runner.run(&["checkout", "-q", "main"]).unwrap();
        std::fs::write(root.path().join("a.txt"), "pending user edit").unwrap();
        let opts = BranchOptions {
            name: "new".into(),
            from: Some("target".into()),
            request_id: "refused".into(),
        };
        assert!(branch_with_state(root.path(), &opts, state.path(), || Ok(())).is_err());
        assert!(
            runner
                .run_opt(&["rev-parse", "--verify", "refs/heads/new"])
                .is_none(),
            "failed checkout must not leave a branch that blocks all retries"
        );
        assert_eq!(current_branch(root.path()), "main");
        assert_eq!(
            std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
            "pending user edit"
        );
        assert_eq!(
            runner.run(&["diff", "--cached", "--name-only"]).unwrap(),
            b""
        );
        // A retry pins the original base even if the source ref moves.
        runner
            .run(&["update-ref", "refs/heads/target", "main"])
            .unwrap();
        std::fs::write(root.path().join("a.txt"), "a").unwrap();
        assert_eq!(
            branch_with_state(root.path(), &opts, state.path(), || Ok(())).unwrap()["created"],
            true
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
            "target"
        );
    }

    #[test]
    fn branch_should_replay_after_checkout_precedes_the_terminal_record() {
        let root = tempdir().unwrap();
        let state = tempdir().unwrap();
        init_repo(root.path());
        let opts = BranchOptions {
            name: "new".into(),
            from: None,
            request_id: "applied".into(),
        };
        assert_eq!(
            branch_with_state(root.path(), &opts, state.path(), || Err(
                "interrupted".into()
            ))
            .unwrap_err(),
            "interrupted"
        );
        assert_eq!(current_branch(root.path()), "new");
        let result = branch_with_state(root.path(), &opts, state.path(), || {
            panic!("must not checkout twice")
        })
        .unwrap();
        assert_eq!(result["created"], true);
        assert_eq!(result["checked_out"], true);
        assert_eq!(
            branch_with_state(root.path(), &opts, state.path(), || panic!(
                "terminal replay"
            ))
            .unwrap(),
            result
        );
    }
    #[test]
    fn branch_retry_should_refuse_another_checkout_at_the_same_commit() {
        let root = tempdir().unwrap();
        let state = tempdir().unwrap();
        init_repo(root.path());
        let opts = BranchOptions {
            name: "new".into(),
            from: None,
            request_id: "moved".into(),
        };
        assert_eq!(
            branch_with_state(root.path(), &opts, state.path(), || Err(
                "interrupted".into()
            ))
            .unwrap_err(),
            "interrupted"
        );
        GitRunner::new(root.path())
            .run(&["checkout", "-q", "main"])
            .unwrap();
        let error = branch_with_state(root.path(), &opts, state.path(), || {
            panic!("must not checkout again")
        })
        .unwrap_err();
        assert!(error.starts_with("RECOVERY_CONFLICT:"));
        assert_eq!(current_branch(root.path()), "main");
    }
}
