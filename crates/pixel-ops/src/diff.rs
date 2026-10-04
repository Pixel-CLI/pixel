// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `diff` — structured diff between two refs or working tree.

use std::path::Path;

use serde_json::{Value, json};

use pixel_git::GitRunner;

/// Show a diff. `from`/`to` are refs (validated). If `to` is None, diffs
/// from `from` to the working tree. Returns structured file changes +
/// unified diff text (byte-capped).
pub fn diff(
    root: &Path,
    from: &str,
    to: Option<&str>,
    paths: Option<&[String]>,
    byte_cap: Option<usize>,
) -> Result<Value, String> {
    let runner = GitRunner::new(root);
    let cap = byte_cap.unwrap_or(64_000).clamp(1024, 1_000_000);

    // Validate refs.
    pixel_git::validate_ref(from).map_err(|e| e.to_string())?;
    if let Some(t) = to {
        pixel_git::validate_ref(t).map_err(|e| e.to_string())?;
    }

    // Get unified diff text.
    let mut args: Vec<String> = vec!["diff".into()];
    args.push("--end-of-options".into());
    args.push(from.to_string());
    if let Some(t) = to {
        args.push(t.to_string());
    }
    if let Some(ps) = paths {
        args.push("--".into());
        args.extend(ps.iter().cloned());
    }
    // Metadata must use the same optional destination and path filters as the patch.
    // In particular, omitted `to` means working tree, not HEAD.
    let mut name_args = args.clone();
    name_args.splice(
        1..1,
        ["--name-status".into(), "--no-renames".into(), "-z".into()],
    );
    let name_refs: Vec<&str> = name_args.iter().map(String::as_str).collect();
    let names = runner
        .run(&name_refs)
        .map_err(|e| format!("git diff names: {e}"))?;
    let mut fields = names
        .split(|&byte| byte == 0)
        .filter(|field| !field.is_empty());
    let mut changes = Vec::new();
    while let Some(status) = fields.next() {
        let path = fields.next().ok_or("git diff name-status missing path")?;
        changes.push((
            status[0] as char,
            String::from_utf8_lossy(path).into_owned(),
        ));
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let diff_bytes = runner
        .run(&arg_refs)
        .map_err(|e| format!("git diff: {e}"))?;
    let diff_text = String::from_utf8_lossy(&diff_bytes);

    // Truncate diff text to byte cap.
    let (diff_truncated, diff_out) = if diff_text.len() > cap {
        // Find a safe truncation point (char boundary).
        let mut end = cap;
        while end > 0 && !diff_text.is_char_boundary(end) {
            end -= 1;
        }
        (true, diff_text[..end].to_string())
    } else {
        (false, diff_text.to_string())
    };

    let file_changes: Vec<Value> = changes
        .iter()
        .map(|(status, path)| {
            json!({
                "status": status.to_string(),
                "path": path,
            })
        })
        .collect();

    Ok(json!({
        "from": from,
        "to": to,
        "files": file_changes,
        "diff": diff_out,
        "truncated": diff_truncated,
        "byte_cap": cap,
        "file_count": file_changes.len(),
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
    fn diff_between_commits() {
        let dir = tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), b"v1").unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["add", "."])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["commit", "-qm", "c1"])
            .status()
            .unwrap();
        let head1 = GitRunner::new(dir.path()).rev_parse_head().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"v2").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"new").unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["add", "."])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["commit", "-qm", "c2"])
            .status()
            .unwrap();
        let head2 = GitRunner::new(dir.path()).rev_parse_head().unwrap();

        let result = diff(dir.path(), &head1, Some(&head2), None, None).unwrap();
        let files = result["files"].as_array().unwrap();
        assert!(files.iter().any(|f| f["path"] == "a.txt"));
        assert!(files.iter().any(|f| f["path"] == "b.txt"));
        assert!(result["diff"].as_str().unwrap().contains("v2"));
    }
    #[test]
    fn diff_metadata_matches_worktree_and_path_scope() {
        let dir = tempdir().unwrap();
        init_repo(dir.path());
        for revision in ["one", "two"] {
            for name in ["a.txt", "b.txt"] {
                std::fs::write(dir.path().join(name), revision).unwrap();
            }
            for args in [
                vec!["add", "."],
                vec![
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-qm",
                    revision,
                ],
            ] {
                assert!(
                    std::process::Command::new("git")
                        .arg("-C")
                        .arg(dir.path())
                        .args(args)
                        .status()
                        .unwrap()
                        .success()
                );
            }
        }
        for name in ["a.txt", "b.txt"] {
            std::fs::write(dir.path().join(name), "working change").unwrap();
        }
        let paths = vec!["a.txt".to_string()];
        for (from, to) in [("HEAD", None), ("HEAD~1", Some("HEAD"))] {
            let result = diff(dir.path(), from, to, Some(&paths), None).unwrap();
            assert_eq!(
                result["file_count"], 1,
                "metadata must use the same refs and path filter as diff text"
            );
            assert_eq!(result["files"][0]["path"], "a.txt");
            assert!(result["diff"].as_str().unwrap().contains("a.txt"));
            assert!(!result["diff"].as_str().unwrap().contains("b.txt"));
        }
    }
}
