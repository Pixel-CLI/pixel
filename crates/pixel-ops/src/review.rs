//! `review` — show repo changes as structured items, INCLUDING conflicted
//! paths (fixes usable-git's blind spot where `review` filtered conflicts
//! to zero items).

use std::path::Path;

use serde_json::{json, Value};

use pixel_git::GitRunner;

/// Review the working tree: staged, unstaged, untracked, and conflicted
/// paths. Unlike usable-git's `review` which filtered conflicted paths to
/// zero, pixel surfaces them as structured conflict hunks.
pub fn review(root: &Path, _cursor: Option<&str>, byte_cap: Option<usize>) -> Result<Value, String> {
    let runner = GitRunner::new(root);
    const DEFAULT_BYTE_CAP: usize = 64_000;
    let cap = byte_cap.unwrap_or(DEFAULT_BYTE_CAP).clamp(128, 1_000_000);

    let status = runner.status_porcelain();
    let mut items: Vec<Value> = Vec::new();
    let mut bytes = 0usize;

    for (xy, path) in &status {
        let kind = classify_status(xy);
        let entry = json!({
            "path": path,
            "kind": kind,
            "status": xy.trim(),
        });
        let entry_bytes = serde_json::to_vec(&entry)
            .map_err(|e| e.to_string())?
            .len();
        if bytes.saturating_add(entry_bytes) > cap {
            break;
        }
        bytes += entry_bytes;

        // For conflicted paths, include conflict hunks.
        if kind == "conflicted" {
            if let Some(hunks) = conflict_hunks(root, path) {
                let mut entry = entry;
                entry["conflicts"] = hunks;
                items.push(entry);
            } else {
                items.push(entry);
            }
        } else {
            items.push(entry);
        }
    }

    let truncated = bytes >= cap;
    Ok(json!({
        "items": items,
        "truncated": truncated,
        "byte_cap": cap,
        "count": items.len(),
    }))
}

fn classify_status(xy: &str) -> &'static str {
    let x = xy.chars().next().unwrap_or(' ');
    let y = xy.chars().nth(1).unwrap_or(' ');
    match (x, y) {
        ('U', _) | (_, 'U') | ('D', 'D') | ('A', 'A') | ('C', 'C') => "conflicted",
        ('?', _) => "untracked",
        ('A', _) | ('M', _) | ('D', _) if y == ' ' => "staged",
        (_, 'M') | (_, 'D') | (_, 'A') if x == ' ' => "unstaged",
        ('R', _) | ('C', _) => "staged",
        _ => "unstaged",
    }
}

/// Extract conflict hunks for a conflicted path using `git diff --diff-filter=U`.
fn conflict_hunks(root: &Path, path: &str) -> Option<Value> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--diff-filter=U", "--", path])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let diff = String::from_utf8_lossy(&out.stdout);
    if diff.is_empty() {
        return None;
    }
    // Parse the diff into hunks (simplified — full hunk parsing is in
    // pixel-graph::changes::parse_diff).
    let hunks: Vec<&str> = diff.split("@@ ").skip(1).collect();
    Some(json!({
        "path": path,
        "hunk_count": hunks.len(),
        "diff": diff.lines().take(50).collect::<Vec<_>>().join("\n"),
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
    fn review_clean_repo() {
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

        let result = review(dir.path(), None, None).unwrap();
        assert_eq!(result["count"], json!(0));
    }

    #[test]
    fn review_shows_untracked_and_staged() {
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
        std::fs::write(dir.path().join("b.txt"), b"new").unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["add", "b.txt"])
            .status()
            .unwrap();
        std::fs::write(dir.path().join("c.txt"), b"untracked").unwrap();

        let result = review(dir.path(), None, None).unwrap();
        let items = result["items"].as_array().unwrap();
        assert!(items.iter().any(|i| i["path"] == "b.txt" && i["kind"] == "staged"));
        assert!(items.iter().any(|i| i["path"] == "c.txt" && i["kind"] == "untracked"));
    }
}
