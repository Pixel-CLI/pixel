//! `branch` — create a new branch from the current HEAD.

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

    let result = json!({
        "branch": opts.name,
        "from": from,
        "created": true,
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
        std::process::Command::new("git").arg("init").arg("-q").arg(root).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(root).args(["config", "user.email", "t@t"]).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(root).args(["config", "user.name", "t"]).status().unwrap();
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        std::process::Command::new("git").arg("-C").arg(root).args(["add", "."]).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(root).args(["commit", "-qm", "init"]).status().unwrap();
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
