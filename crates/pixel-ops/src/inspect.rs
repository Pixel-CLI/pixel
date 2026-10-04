// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `inspect` — repo state snapshot (HEAD, branch, dirty files, fingerprints).
//!
//! Port of usable-git's `inspect` op. Returns a structured snapshot of the
//! working tree state. Used by mutations to capture `expected` state and
//! by the agent to understand the current repo state.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use pixel_git::GitRunner;

/// A file fingerprint: sha256 of the file content (tracked) or "dirty" for
/// unstaged changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileFingerprint {
    pub path: String,
    pub fingerprint: String,
    pub status: String,
}

/// Inspect result: repo state at a point in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InspectResult {
    pub root: String,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub dirty: Vec<FileFingerprint>,
    pub clean: Vec<String>,
}

/// Run `inspect` on a repo root. Returns the current state.
/// Cap on the clean-file list `inspect` returns. A monorepo with 50K
/// tracked files would dump ~2MB of path strings into the agent's context;
/// the count stays exact, the list is truncated for display. `pixel search`
/// or `pixel targets` is the tool for enumerating files by content.
const CLEAN_LIST_CAP: usize = 200;

pub fn inspect(root: &Path) -> Result<Value, String> {
    let runner = GitRunner::new(root);
    let head = runner.rev_parse_head();
    let branch = GitRunner::new(root).current_branch();
    let dirty = runner.status_porcelain();

    // Build dirty file list with fingerprints.
    let mut dirty_files: Vec<FileFingerprint> = Vec::new();
    let mut dirty_paths: Vec<String> = Vec::new();
    for (status, path) in &dirty {
        dirty_paths.push(path.clone());
        // Fingerprint = sha256 of current file content (or "deleted" if gone).
        let fp = std::fs::read(root.join(path)).map_or_else(
            |_| "deleted".to_string(),
            |bytes| {
                use sha2::{Digest, Sha256};
                let mut h = Sha256::new();
                h.update(&bytes);
                hex::encode(h.finalize())
            },
        );
        dirty_files.push(FileFingerprint {
            path: path.clone(),
            fingerprint: fp,
            status: status.trim().to_string(),
        });
    }

    // Clean tracked files: exact count, list capped (see `CLEAN_LIST_CAP`).
    let all_tracked = runner.ls_files();
    let dirty_set: std::collections::HashSet<&String> = dirty.iter().map(|(_, p)| p).collect();
    let clean_total = all_tracked
        .iter()
        .filter(|p| !dirty_set.contains(p))
        .count();
    let clean: Vec<String> = all_tracked
        .into_iter()
        .filter(|p| !dirty_set.contains(p))
        .take(CLEAN_LIST_CAP)
        .collect();
    let clean_truncated = clean_total > clean.len();

    Ok(json!({
        "root": root.display().to_string(),
        "head": head,
        "branch": branch,
        "dirty": dirty_files,
        "clean": clean,
        "dirty_count": dirty_files.len(),
        "clean_count": clean_total,
        "clean_list_truncated": clean_truncated,
        "clean_list_cap": CLEAN_LIST_CAP,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn init_repo(root: &Path) {
        std::process::Command::new("git")
            .arg("init")
            .arg("-q")
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
    }

    #[test]
    fn inspect_clean_repo() {
        let dir = tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["add", "."])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["commit", "-qm", "init"])
            .status()
            .unwrap();

        let result = inspect(dir.path()).unwrap();
        assert_eq!(result["dirty_count"], json!(0));
        assert_eq!(result["clean_count"], json!(1));
        assert!(result["head"].as_str().unwrap().len() >= 7);
        let branch = result["branch"].as_str().unwrap_or("main");
        assert!(branch == "main" || branch == "master");
    }

    #[test]
    fn inspect_dirty_repo() {
        let dir = tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["add", "."])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["commit", "-qm", "init"])
            .status()
            .unwrap();
        std::fs::write(dir.path().join("a.txt"), b"dirty edit").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"new").unwrap();

        let result = inspect(dir.path()).unwrap();
        assert_eq!(result["dirty_count"], json!(2));
        let dirty = result["dirty"].as_array().unwrap();
        assert!(dirty.iter().any(|d| d["path"] == "a.txt"));
        assert!(dirty.iter().any(|d| d["path"] == "b.txt"));
    }
}
