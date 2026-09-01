//! `branch` — create a new branch from the current HEAD and switch to it.
//!
//! Switching is not optional: without it, a caller that creates a branch and
//! then calls `publish` commits onto whatever was checked out before —
//! silently landing work on `main` instead of the requested branch. This bit
//! a real agent workflow (branch -> publish -> push), so `branch` now mirrors
//! `git checkout -b` / `git switch -c`, not bare `git branch`.

use std::path::Path;

use serde_json::{json, Value};

use pixel_git::GitRunner;

use crate::durable::sha256_hex;
use crate::journal::{BeginOutcome, JournalOperation, OperationJournal};
use crate::lock::RepositoryLock;

#[derive(Debug, Clone)]
pub struct BranchOptions {
    pub name: String,
    pub from: Option<String>, // base ref, defaults to HEAD
    pub request_id: String,
}

pub fn branch(root: &Path, opts: &BranchOptions) -> Result<Value, String> {
    let runner = GitRunner::new(root);
    let repo_key = root.canonicalize().unwrap_or_else(|_| root.to_path_buf()).display().to_string();
    let input_hash = sha256_hex(&format!("{}\u{0}{}", opts.name, opts.from.as_deref().unwrap_or("")));

    let state_root = crate::durable::state_root();
    let journal = OperationJournal::with_state_root(state_root.clone());

    let outcome = journal.begin(&opts.request_id, JournalOperation::Branch, &repo_key, &input_hash)?;
    if let BeginOutcome::Replay(result) = outcome {
        return Ok(result);
    }

    let mut lock = RepositoryLock::acquire_with_state_root(
        &root.join(".git").display().to_string(),
        &state_root,
    ).map_err(|_| "repository is busy".to_string())?;

    // Validate branch name.
    pixel_git::validate_ref(&opts.name).map_err(|e| e.to_string())?;

    // Check if branch already exists.
    let existing = runner.run_opt(&["rev-parse", "--verify", "--quiet", &format!("refs/heads/{}", opts.name)]);
    if let Some(out) = existing {
        if !out.is_empty() {
            let _ = lock.release();
            return Err(format!("REF_EXISTS: branch '{}' already exists", opts.name));
        }
    }

    // Create branch.
    let from = opts.from.as_deref().unwrap_or("HEAD");
    pixel_git::validate_ref(from).map_err(|e| e.to_string())?;
    runner.run(&["branch", opts.name.as_str(), from]).map_err(|e| {
        let _ = lock.release();
        format!("git branch: {e}")
    })?;
    runner.run(&["checkout", opts.name.as_str()]).map_err(|e| {
        let _ = lock.release();
        format!("git checkout: {e}")
    })?;

    let result = json!({
        "branch": opts.name,
        "from": from,
        "created": true,
        "checked_out": true,
    });
    journal.complete(&opts.request_id, &repo_key, result.clone())?;
    let _ = lock.release();
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
        std::process::Command::new("git").arg("init").arg("-q").arg("-b").arg("main").arg(root).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(root).args(["config", "user.email", "t@t"]).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(root).args(["config", "user.name", "t"]).status().unwrap();
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        std::process::Command::new("git").arg("-C").arg(root).args(["add", "."]).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(root).args(["commit", "-qm", "init"]).status().unwrap();
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
}
