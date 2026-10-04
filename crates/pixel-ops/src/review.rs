// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `review` — show repo changes as structured items, INCLUDING conflicted
//! paths (fixes usable-git's blind spot where `review` filtered conflicts
//! to zero items).

use std::path::Path;

use serde_json::{Value, json};

use pixel_git::GitRunner;

/// Review the working tree: staged, unstaged, untracked, and conflicted
/// paths. Unlike usable-git's `review` which filtered conflicted paths to
/// zero, pixel surfaces them as structured conflict hunks.
pub fn review(
    root: &Path,
    _cursor: Option<&str>,
    byte_cap: Option<usize>,
) -> Result<Value, String> {
    const DEFAULT_BYTE_CAP: usize = 64_000;
    let runner = GitRunner::new(root);
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
        let entry_bytes = serde_json::to_vec(&entry).map_err(|e| e.to_string())?.len();
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
    let out = pixel_git::GitRunner::new(root)
        .with_max_output_bytes(Some(pixel_git::ENUMERATION_MAX_OUTPUT_BYTES))
        .run(&["diff", "--diff-filter=U", "--", path])
        .ok()?;
    let diff = String::from_utf8_lossy(&out);
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
        assert!(
            items
                .iter()
                .any(|i| i["path"] == "b.txt" && i["kind"] == "staged")
        );
        assert!(
            items
                .iter()
                .any(|i| i["path"] == "c.txt" && i["kind"] == "untracked")
        );
    }

    fn git(root: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    }

    #[test]
    fn review_reports_conflict_hunks_for_a_conflicted_path() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        std::fs::write(root.join("a.txt"), b"base\n").unwrap();
        assert!(git(root, &["add", "."]));
        assert!(git(root, &["commit", "-qm", "init"]));
        assert!(git(root, &["checkout", "-qb", "side"]));
        std::fs::write(root.join("a.txt"), b"side\n").unwrap();
        assert!(git(root, &["commit", "-qam", "side"]));
        assert!(git(root, &["checkout", "-q", "-"]));
        std::fs::write(root.join("a.txt"), b"main\n").unwrap();
        assert!(git(root, &["commit", "-qam", "main"]));
        assert!(!git(root, &["merge", "side"]), "the merge must conflict");

        // A clean path has no conflict hunks; the conflicted one has at
        // least one, with the marker lines in the diff excerpt.
        assert_eq!(conflict_hunks(root, "missing.txt"), None);
        let hunks = conflict_hunks(root, "a.txt").expect("conflicted path has hunks");
        assert_eq!(hunks["path"], "a.txt");
        assert!(hunks["hunk_count"].as_u64().unwrap() >= 1, "{hunks}");
        assert!(
            hunks["diff"].as_str().unwrap().contains("<<<<<<<"),
            "{hunks}"
        );

        let result = review(root, None, None).unwrap();
        let items = result["items"].as_array().unwrap();
        let item = items
            .iter()
            .find(|i| i["path"] == "a.txt")
            .expect("conflicted path listed");
        assert_eq!(item["kind"], "conflicted", "{item}");
        assert_eq!(item["conflicts"]["path"], "a.txt", "{item}");
    }
}
