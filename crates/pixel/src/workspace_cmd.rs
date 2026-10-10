// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The `--workspace` flag: a named set of repositories that graph ops can
//! fan out across.
//!
//! `.pixel/workspace.json` holds canonicalized member paths; the `workspace`
//! plugin (`pixel workspace add|remove|list|clear`, in `Pixel-CLI/pixel-plugins`)
//! writes it and this file only reads it. `impact` and
//! `who-calls` accept `--workspace`: the op runs against every member's own
//! index (each repo's daemon or in-process service answers independently)
//! and the results are merged with per-repo attribution. There is no
//! cross-repo edge — a member that never indexed the symbol simply reports
//! none — so the union answers "which repos' graphs reach this symbol",
//! which is the honest multi-repo version of the question.

use std::path::{Path, PathBuf};

use pixel_index::index::SHARD_DIR;
use serde::Deserialize;
use serde_json::{Value, json};

use pixel_daemon::api::Request;

const WORKSPACE_FILE: &str = "workspace.json";

#[derive(Debug, Default, Deserialize)]
struct Registry {
    #[serde(default)]
    members: Vec<PathBuf>,
}

fn registry_path(root: &Path) -> PathBuf {
    root.join(SHARD_DIR).join(WORKSPACE_FILE)
}

fn load(root: &Path) -> Result<Registry, String> {
    let path = registry_path(root);
    match std::fs::read(&path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).map_err(|e| format!("workspace {}: {e}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Registry::default()),
        Err(e) => Err(format!("workspace {}: {e}", path.display())),
    }
}

/// Canonicalized member list. Members that vanished from disk are reported
/// to the caller as part of the fan-out, not silently dropped here.
pub fn members(root: &Path) -> Result<Vec<PathBuf>, String> {
    Ok(load(root)?.members)
}

/// Every member's answer to the same op, in registry order. A failing
/// member is captured as an error entry — one broken checkout must not
/// hide the rest of the workspace.
pub fn fan_out(root: &Path, op: &dyn Fn() -> Request) -> Result<Vec<Value>, String> {
    let members = members(root)?;
    if members.is_empty() {
        return Err(
            "no workspace members — `pixel workspace add <repo>` first (the `workspace` plugin)"
                .to_string(),
        );
    }
    Ok(members
        .iter()
        .map(|member| match crate::execute(member, op(), false) {
            Ok(data) => json!({"repo": member.display().to_string(), "ok": true, "data": data}),
            Err(e) => {
                json!({"repo": member.display().to_string(), "ok": false, "error": e})
            }
        })
        .collect())
}

/// Merged output for a fanned-out op: text prints one section per member,
/// json emits the structured envelope.
pub fn print_fan_out(results: &[Value], json: bool) -> Result<(), String> {
    if json {
        let out = json!({"results": results});
        println!(
            "{}",
            serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    for result in results {
        let repo = result["repo"].as_str().unwrap_or("?");
        println!("== {repo} ==");
        match result["ok"].as_bool() {
            Some(true) => println!(
                "{}",
                serde_json::to_string_pretty(&result["data"]).map_err(|e| e.to_string())?
            ),
            _ => println!("error: {}", result["error"].as_str().unwrap_or("unknown")),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("px-ws-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The registry as the `workspace` plugin writes it.
    #[test]
    fn members_reads_the_registry_the_plugin_wrote() {
        let root = scratch("members");
        assert!(members(&root).unwrap().is_empty(), "no file, no members");
        std::fs::create_dir_all(root.join(SHARD_DIR)).unwrap();
        std::fs::write(registry_path(&root), r#"{"members": ["/a/one", "/b/two"]}"#).unwrap();
        assert_eq!(
            members(&root).unwrap(),
            [PathBuf::from("/a/one"), PathBuf::from("/b/two")]
        );
        std::fs::write(registry_path(&root), "{").unwrap();
        let err = members(&root).unwrap_err();
        assert!(err.contains("workspace.json"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn empty_workspace_fans_out_to_an_error() {
        let root = scratch("empty");
        let result = fan_out(&root, &|| Request::Status {});
        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
