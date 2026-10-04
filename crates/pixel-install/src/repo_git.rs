// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Keeps the machine-local files of `pixel install --repo` out of commits.
//!
//! Every repo artifact names this machine's `pixel` binary by absolute path.
//! Committed, it hands teammates a hook pointing at a path that does not
//! exist on their machine. The personal files the tools read
//! (`.claude/settings.local.json`, `.devin/config.local.json`) are only
//! gitignored by those tools' own conventions when the tool creates them, and
//! Codex and pi have no personal variant at all, so the install lists every
//! artifact in the clone's own `info/exclude`, which is never shared.

use std::fs;
use std::path::Path;

use pixel_git::GitRunner;

/// Whether `rel` (relative to `repo`) is tracked by the git repository
/// enclosing `repo`. Outside a git repository nothing is tracked.
pub(crate) fn is_tracked(repo: &Path, rel: &str) -> bool {
    GitRunner::new(repo)
        .run_opt(&["ls-files", "--error-unmatch", "--", rel])
        .is_some()
}

/// The lines to append to an exclude file holding `existing` so that each
/// of `rels` (relative to the directory at `prefix` inside the work tree) is
/// ignored: one root-anchored pattern per path the file does not list yet.
pub(crate) fn exclude_additions(existing: &str, prefix: &str, rels: &[&str]) -> Vec<String> {
    rels.iter()
        .map(|rel| format!("/{prefix}{rel}"))
        .filter(|pattern| !existing.lines().any(|line| line.trim() == pattern))
        .collect()
}

/// Append the missing `rels` patterns to the `info/exclude` of the git
/// repository enclosing `repo`; returns the patterns added. Outside a git
/// repository this is a no-op, and a dry run computes without writing.
///
/// # Errors
///
/// Reading or writing the exclude file.
pub(crate) fn exclude_locally(
    repo: &Path,
    rels: &[&str],
    dry_run: bool,
) -> crate::Result<Vec<String>> {
    let git = GitRunner::new(repo);
    let Some(exclude) = git.run_opt(&["rev-parse", "--git-path", "info/exclude"]) else {
        return Ok(Vec::new());
    };
    let prefix = git
        .run_opt(&["rev-parse", "--show-prefix"])
        .map(|out| String::from_utf8_lossy(&out).trim().to_string())
        .unwrap_or_default();
    // `--git-path` answers relative to the `-C` directory unless absolute.
    let path = repo.join(String::from_utf8_lossy(&exclude).trim());
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let added = exclude_additions(&existing, &prefix, rels);
    if added.is_empty() || dry_run {
        return Ok(added);
    }
    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str("# pixel install --repo: machine-local, names this machine's pixel binary\n");
    for pattern in &added {
        content.push_str(pattern);
        content.push('\n');
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, content)?;
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclude_additions_should_anchor_each_path_and_skip_listed_ones() {
        let existing = ["# comment", "/.claude/settings.local.json", "  /.devin/x  "].join("\n");
        assert_eq!(
            exclude_additions(
                &existing,
                "",
                &[
                    ".claude/settings.local.json",
                    ".devin/x",
                    ".codex/hooks.json"
                ]
            ),
            vec!["/.codex/hooks.json".to_string()],
            "a listed pattern, even indented, is not added twice"
        );
        assert_eq!(
            exclude_additions("", "sub/dir/", &[".codex/hooks.json"]),
            vec!["/sub/dir/.codex/hooks.json".to_string()],
            "a repo below the work-tree root is anchored at its own path"
        );
        assert_eq!(
            exclude_additions("/.codex/hooks.json.bak", "", &[".codex/hooks.json"]),
            vec!["/.codex/hooks.json".to_string()],
            "a longer pattern sharing the prefix does not count as listed"
        );
    }
}
