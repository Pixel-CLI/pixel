// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Retirement of the Warp MCP server entry older installs wrote.
//!
//! Up to 0.6.1, `pixel install --repo` added a `pixel mcp` server to Warp's
//! `.warp/.mcp.json`. Pixel ships no MCP server any more, so an entry left
//! there points Warp at a command that is gone. Install and uninstall both
//! remove this repository's entry, and `pixel doctor` reports one that
//! remains. Only the exact entry Pixel wrote is touched: other servers, other
//! top-level keys and an entry naming another repository stay byte for byte.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::InstallError;
use crate::config;
use crate::install::{self, CheckStatus, InstallStep, Result};

/// The Warp MCP configuration file, relative to a repository root.
pub(crate) const CONFIG_FILE: &str = ".warp/.mcp.json";

/// Remove this repository's retired Pixel entry from Warp's MCP config.
///
/// The file goes, without a backup, when the entry was all it held (Pixel
/// created it), and so does a `.warp/` directory left empty; a file that
/// keeps other content is backed up and rewritten, and a config Git tracks
/// is never edited.
pub(crate) fn retire(repo: &Path, dry_run: bool) -> Result<InstallStep> {
    let path = repo.join(CONFIG_FILE);
    if crate::repo_git::is_tracked(repo, CONFIG_FILE) {
        return Ok(InstallStep {
            id: "mcp.warp".into(),
            status: CheckStatus::Yellow,
            summary: install::dry_run_summary(
                dry_run,
                ".warp/.mcp.json is tracked by git; a retired Pixel entry in it is left for you to remove",
            ),
            detail: Some(format!("config={}", path.display())),
        });
    }
    let repo_root = absolute_repo(repo)?;
    let mut root = read_root(&path)?;
    let removed = remove_owned_entry(&mut root, &repo_root);

    if removed && !dry_run {
        if root.is_empty() {
            // Nothing but Pixel's entry was in it, so there is nothing to back up.
            fs::remove_file(&path)?;
            if let Some(dir) = path.parent() {
                // Fails, and is meant to, when the directory holds anything else.
                let _ = fs::remove_dir(dir);
            }
        } else {
            let serialized = pretty_json(&root)?;
            config::backup_if_changing(&path, serialized.as_bytes())?;
            fs::write(&path, serialized)?;
        }
    }

    let summary = if removed {
        "removed this repository's retired Pixel Warp MCP server"
    } else {
        "no retired Pixel Warp MCP server for this repository"
    };
    Ok(InstallStep {
        id: "mcp.warp".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, summary),
        detail: Some(format!("config={}", path.display())),
    })
}

/// Whether Warp's config still holds the entry an older install wrote here.
pub(crate) fn has_retired_entry(repo: &Path) -> Result<bool> {
    let path = repo.join(CONFIG_FILE);
    if !path.exists() {
        return Ok(false);
    }
    let repo_root = absolute_repo(repo)?;
    let mut root = read_root(&path)?;
    Ok(remove_owned_entry(&mut root, &repo_root))
}

/// Take this repository's Pixel entry out of `root`, dropping an emptied
/// `mcpServers`; `false` when there was none.
fn remove_owned_entry(root: &mut Map<String, Value>, repo_root: &Path) -> bool {
    let Some(servers) = root.get_mut("mcpServers").and_then(Value::as_object_mut) else {
        return false;
    };
    if !servers
        .get("pixel")
        .is_some_and(|entry| owned_by_repo(entry, repo_root))
    {
        return false;
    }
    servers.remove("pixel");
    if servers.is_empty() {
        root.remove("mcpServers");
    }
    true
}

fn absolute_repo(repo: &Path) -> Result<PathBuf> {
    repo.canonicalize().map_err(InstallError::Io)
}

/// The exact shape older installs wrote: a Pixel executable, `mcp <repo>`
/// and the repository as working directory, nothing else.
fn owned_by_repo(entry: &Value, repo_root: &Path) -> bool {
    let Some(object) = entry.as_object() else {
        return false;
    };
    let Some(command) = object.get("command").and_then(Value::as_str) else {
        return false;
    };
    let Some(args) = object.get("args").and_then(Value::as_array) else {
        return false;
    };
    let root = repo_root.to_string_lossy();
    object.len() == 3
        && config::PIXEL_EXECUTABLES.contains(
            &Path::new(command)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default(),
        )
        && args.len() == 2
        && args[0].as_str() == Some("mcp")
        && args[1].as_str() == Some(root.as_ref())
        && object.get("working_directory").and_then(Value::as_str) == Some(root.as_ref())
}

fn read_root(path: &Path) -> Result<Map<String, Value>> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(error) => return Err(error.into()),
    };
    let value: Value = serde_json::from_str(&raw).map_err(|error| invalid_config(path, error))?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| invalid_config(path, "top-level JSON value must be an object"))
}

fn pretty_json(root: &Map<String, Value>) -> Result<String> {
    Ok(format!("{}\n", serde_json::to_string_pretty(root)?))
}

fn invalid_config(path: &Path, reason: impl ToString) -> InstallError {
    InstallError::InvalidSettings {
        path: path.to_path_buf(),
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::Value;

    use super::*;

    fn config_path(repo: &Path) -> PathBuf {
        repo.join(CONFIG_FILE)
    }

    /// The entry 0.6.1 and earlier wrote for `repo`.
    fn legacy_entry(repo: &Path, exe: &str) -> Value {
        let root = repo.canonicalize().unwrap();
        serde_json::json!({
            "command": exe,
            "args": ["mcp", root],
            "working_directory": root,
        })
    }

    fn write_config(repo: &Path, value: &Value) -> PathBuf {
        let path = config_path(repo);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, format!("{value}\n")).unwrap();
        path
    }

    #[test]
    fn retire_should_remove_the_legacy_entry_and_preserve_everything_else() {
        let parent = tempfile::tempdir().unwrap();
        let repo = parent.path().join("project with spaces");
        fs::create_dir_all(&repo).unwrap();
        let path = write_config(
            &repo,
            &serde_json::json!({
                "other": {"command": "keep"},
                "mcpServers": {
                    "lint": {"command": "lint"},
                    "pixel": legacy_entry(&repo, "/opt/Pixel Tools/pixel"),
                },
            }),
        );
        assert!(has_retired_entry(&repo).unwrap());

        let step = retire(&repo, false).unwrap();

        assert_eq!(step.status, CheckStatus::Green);
        assert!(step.summary.starts_with("removed"), "{}", step.summary);
        let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "other": {"command": "keep"},
                "mcpServers": {"lint": {"command": "lint"}},
            })
        );
        assert!(!has_retired_entry(&repo).unwrap());
    }

    #[test]
    fn retire_should_delete_a_config_and_directory_that_held_only_pixel() {
        let repo = tempfile::tempdir().unwrap();
        let path = write_config(
            repo.path(),
            &serde_json::json!({"mcpServers": {"pixel": legacy_entry(repo.path(), "/opt/pixel")}}),
        );

        retire(repo.path(), false).unwrap();

        assert!(!path.exists());
        assert!(
            !repo.path().join(".warp").exists(),
            "an emptied .warp/ is Pixel's leftover too"
        );
    }

    #[test]
    fn retire_should_keep_a_warp_directory_that_holds_other_files() {
        let repo = tempfile::tempdir().unwrap();
        let path = write_config(
            repo.path(),
            &serde_json::json!({"mcpServers": {"pixel": legacy_entry(repo.path(), "/opt/pixel")}}),
        );
        let sibling = repo.path().join(".warp/workflows.yaml");
        fs::write(&sibling, "user file\n").unwrap();

        retire(repo.path(), false).unwrap();

        assert!(!path.exists());
        assert_eq!(fs::read_to_string(sibling).unwrap(), "user file\n");
    }

    #[test]
    fn retire_dry_run_should_report_without_changing_the_config() {
        let repo = tempfile::tempdir().unwrap();
        let path = write_config(
            repo.path(),
            &serde_json::json!({"mcpServers": {"pixel": legacy_entry(repo.path(), "/opt/pixel")}}),
        );
        let before = fs::read(&path).unwrap();

        let step = retire(repo.path(), true).unwrap();

        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(
            step.summary,
            "[dry-run] would report: removed this repository's retired Pixel Warp MCP server"
        );
    }

    #[test]
    fn retire_should_leave_an_absent_config_absent() {
        let repo = tempfile::tempdir().unwrap();

        let step = retire(repo.path(), false).unwrap();

        assert_eq!(step.status, CheckStatus::Green);
        assert!(step.summary.starts_with("no retired"), "{}", step.summary);
        assert!(!repo.path().join(".warp").exists());
        assert!(!has_retired_entry(repo.path()).unwrap());
    }

    #[test]
    fn retire_should_keep_entries_it_did_not_write() {
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        let other_repo = legacy_entry(repo.path(), "/opt/pixel");
        let mut other_repo = other_repo.as_object().unwrap().clone();
        other_repo.insert("args".into(), serde_json::json!(["mcp", "/another/repo"]));
        let cases = [
            ("another repository", Value::Object(other_repo)),
            (
                "a foreign command",
                legacy_entry(repo.path(), "/opt/unrelated/tool"),
            ),
            (
                "a user field",
                serde_json::json!({
                    "command": "/opt/pixel",
                    "args": ["mcp", root],
                    "working_directory": root,
                    "user_note": "keep this",
                }),
            ),
            (
                "another working directory",
                serde_json::json!({
                    "command": "/opt/pixel",
                    "args": ["mcp", root],
                    "working_directory": "/elsewhere",
                }),
            ),
            (
                "another subcommand",
                serde_json::json!({
                    "command": "/opt/pixel",
                    "args": ["serve", root],
                    "working_directory": root,
                }),
            ),
            ("not an object", serde_json::json!("pixel mcp")),
        ];
        for (case, entry) in cases {
            let path = write_config(
                repo.path(),
                &serde_json::json!({"mcpServers": {"pixel": entry}}),
            );
            let before = fs::read(&path).unwrap();

            assert!(!has_retired_entry(repo.path()).unwrap(), "{case}");
            retire(repo.path(), false).unwrap();

            assert_eq!(fs::read(&path).unwrap(), before, "{case}");
        }
    }

    #[test]
    fn retire_should_refuse_malformed_config_without_modifying_bytes() {
        let repo = tempfile::tempdir().unwrap();
        let path = config_path(repo.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        for raw in ["{", "[]"] {
            fs::write(&path, raw).unwrap();
            assert!(matches!(
                retire(repo.path(), false),
                Err(InstallError::InvalidSettings { .. })
            ));
            assert!(has_retired_entry(repo.path()).is_err());
            assert_eq!(fs::read(&path).unwrap(), raw.as_bytes());
        }
    }

    #[test]
    fn retire_should_leave_a_git_tracked_warp_config_untouched() {
        let repo = tempfile::tempdir().unwrap();
        let path = write_config(
            repo.path(),
            &serde_json::json!({"mcpServers": {"pixel": legacy_entry(repo.path(), "/opt/pixel")}}),
        );
        let git = pixel_git::GitRunner::new(repo.path());
        assert!(git.run_opt(&["init"]).is_some());
        assert!(git.run_opt(&["add", "--", CONFIG_FILE]).is_some());
        let before = fs::read(&path).unwrap();

        let step = retire(repo.path(), false).unwrap();

        assert_eq!(step.status, CheckStatus::Yellow);
        assert!(step.summary.contains("tracked by git"), "{}", step.summary);
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn has_retired_entry_should_report_an_unreadable_config_instead_of_absent() {
        let repo = tempfile::tempdir().unwrap();
        fs::create_dir_all(config_path(repo.path())).unwrap();
        assert!(has_retired_entry(repo.path()).is_err());
    }
}
