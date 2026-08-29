//! `history` — commit history with detail levels and byte caps.

use std::path::Path;

use serde_json::{json, Value};

use pixel_git::GitRunner;

/// Show commit history. `ref` defaults to HEAD. `limit` capped at 100.
/// `detail` = "compact" (oid + subject) or "full" (oid + author + date +
/// subject + body).
pub fn history(
    root: &Path,
    ref_name: Option<&str>,
    limit: Option<usize>,
    detail: &str,
    cursor: Option<&str>,
    byte_cap: Option<usize>,
) -> Result<Value, String> {
    let runner = GitRunner::new(root);
    let limit = limit.unwrap_or(20).clamp(1, 100);
    let cap = byte_cap.unwrap_or(64_000).clamp(1024, 1_000_000);
    let ref_name = ref_name.unwrap_or("HEAD");

    // Validate ref to prevent option injection.
    pixel_git::validate_ref(ref_name).map_err(|e| e.to_string())?;

    // Build the log format.
    let format = if detail == "full" {
        "%H%x1f%an%x1f%ae%x1f%at%x1f%s%x1f%b%x1e"
    } else {
        "%H%x1f%at%x1f%s%x1e"
    };

    // Cursor = skip N commits (simple pagination).
    let skip = cursor.and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);

    let mut args: Vec<String> = vec![
        "log".into(),
        format!("--format={format}"),
        "-n".into(),
        limit.to_string(),
        "--skip".into(),
        skip.to_string(),
    ];
    // Use --end-of-options to treat ref as a rev, not an option.
    args.push("--end-of-options".into());
    args.push(ref_name.to_string());

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = runner
        .run(&arg_refs)
        .map_err(|e| format!("git log: {e}"))?;
    let log = String::from_utf8_lossy(&output);

    let mut commits: Vec<Value> = Vec::new();
    let mut bytes = 0usize;

    for record in log.split('\u{1e}') {
        let record = record.trim();
        if record.is_empty() {
            continue;
        }
        let fields: Vec<&str> = record.split('\u{1f}').collect();
        let entry = if detail == "full" && fields.len() >= 6 {
            json!({
                "oid": fields[0],
                "author": fields[1],
                "email": fields[2],
                "date": fields[3],
                "subject": fields[4],
                "body": fields[5],
            })
        } else if fields.len() >= 3 {
            json!({
                "oid": fields[0],
                "date": fields[1],
                "subject": fields[2],
            })
        } else {
            continue;
        };

        let entry_bytes = serde_json::to_vec(&entry)
            .map_err(|e| e.to_string())?
            .len();
        if bytes.saturating_add(entry_bytes) > cap {
            break;
        }
        bytes += entry_bytes;
        commits.push(entry);
    }

    let truncated = commits.len() >= limit || bytes >= cap;
    let next_cursor = if truncated {
        Some((skip + commits.len()).to_string())
    } else {
        None
    };

    Ok(json!({
        "commits": commits,
        "truncated": truncated,
        "cursor": cursor,
        "next_cursor": next_cursor,
        "limit": limit,
        "byte_cap": cap,
        "count": commits.len(),
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
    fn history_returns_commits() {
        let dir = tempdir().unwrap();
        init_repo(dir.path());
        for i in 0..3 {
            std::fs::write(dir.path().join(format!("f{i}.txt")), b"x").unwrap();
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["add", "."])
                .status()
                .unwrap();
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["commit", "-qm", &format!("commit {i}")])
                .status()
                .unwrap();
        }
        let result = history(dir.path(), None, None, "compact", None, None).unwrap();
        let commits = result["commits"].as_array().unwrap();
        assert_eq!(commits.len(), 3);
        assert!(commits[0]["subject"].as_str().unwrap().contains("commit 2"));
    }

    #[test]
    fn history_paginates_with_cursor() {
        let dir = tempdir().unwrap();
        init_repo(dir.path());
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("f{i}.txt")), b"x").unwrap();
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["add", "."])
                .status()
                .unwrap();
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["commit", "-qm", &format!("commit {i}")])
                .status()
                .unwrap();
        }
        let page1 = history(dir.path(), None, Some(2), "compact", None, None).unwrap();
        assert_eq!(page1["count"], json!(2));
        let cursor = page1["next_cursor"].as_str().unwrap();
        let page2 = history(dir.path(), None, Some(2), "compact", Some(cursor), None).unwrap();
        assert_eq!(page2["count"], json!(2));
        // Pages should not overlap.
        let p1_oids: std::collections::HashSet<String> = page1["commits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["oid"].as_str().unwrap().to_string())
            .collect();
        let p2_oids: std::collections::HashSet<String> = page2["commits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["oid"].as_str().unwrap().to_string())
            .collect();
        assert!(p1_oids.is_disjoint(&p2_oids));
    }
}
