// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `pixel workspace` — a named set of repositories that graph ops can fan
//! out across, and the `--workspace` flag that uses it.
//!
//! `.pixel/workspace.json` holds canonicalized member paths. `impact` and
//! `who-calls` accept `--workspace`: the op runs against every member's own
//! index (each repo's daemon or in-process service answers independently)
//! and the results are merged with per-repo attribution. There is no
//! cross-repo edge — a member that never indexed the symbol simply reports
//! none — so the union answers "which repos' graphs reach this symbol",
//! which is the honest multi-repo version of the question.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use pixel_daemon::Request;

use crate::commands::Done;

const WORKSPACE_FILE: &str = "workspace.json";

#[derive(Debug, Default, Serialize, Deserialize)]
struct Registry {
    #[serde(default)]
    members: Vec<PathBuf>,
}

fn registry_path(root: &Path) -> PathBuf {
    root.join(pixel_io::sidecar::DIR_NAME).join(WORKSPACE_FILE)
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

fn save(root: &Path, registry: &Registry) -> Result<(), String> {
    let path = registry_path(root);
    if let Some(dir) = path.parent() {
        pixel_io::sidecar::private_dir(dir).map_err(|e| format!("workspace dir: {e}"))?;
    }
    let bytes = serde_json::to_vec_pretty(registry).map_err(|e| e.to_string())?;
    // A fresh temporary file renamed over the name: a link committed at
    // either name is replaced, never written through.
    pixel_io::nofollow::write_replace(&path, &bytes, pixel_io::nofollow::PRIVATE_MODE)
        .map_err(|e| format!("workspace write: {e}"))
}

/// Canonicalized member list. Members that vanished from disk are reported
/// to the caller as part of the fan-out, not silently dropped here.
pub fn members(root: &Path) -> Result<Vec<PathBuf>, String> {
    Ok(load(root)?.members)
}

/// Every member's answer to the same op, in registry order. A failing
/// member is captured as an error entry: one broken checkout must not hide
/// the rest of the workspace.
pub fn fan_out(
    root: &Path,
    op: &dyn Fn() -> Request,
    ask: &dyn Fn(&Path, Request) -> Result<Value, String>,
) -> Result<Vec<Value>, String> {
    let members = members(root)?;
    if members.is_empty() {
        return Err("no workspace members: `pixel workspace add <repo>` first".to_string());
    }
    Ok(members
        .iter()
        .map(|member| match ask(member, op()) {
            Ok(data) => json!({"repo": member.display().to_string(), "ok": true, "data": data}),
            Err(e) => {
                json!({"repo": member.display().to_string(), "ok": false, "error": e})
            }
        })
        .collect())
}

/// Merged output for a fanned-out op: text prints one section per member
/// (`render` shows a member's data), json emits the structured envelope.
pub fn fan_out_done(results: &[Value], json: bool, render: &dyn Fn(&Value) -> String) -> Done {
    let value = json!({"results": results});
    let mut text = String::new();
    for result in results {
        let repo = result["repo"].as_str().unwrap_or("?");
        text.push_str(&format!("== {repo} ==\n"));
        match result["ok"].as_bool() {
            Some(true) => text.push_str(&render(&result["data"])),
            _ => text.push_str(&format!(
                "error: {}\n",
                result["error"].as_str().unwrap_or("unknown")
            )),
        }
    }
    Done::new(value, text, json)
}

/// The repository holding the registry: the one `path` is in, else `path`.
pub(crate) fn root_of(path: &Path) -> PathBuf {
    crate::client::discover_root(path).unwrap_or_else(|_| path.to_path_buf())
}

pub(crate) fn run(cmd: &WorkspaceCmd) -> Result<Done, String> {
    let said = |text: String| {
        Ok(Done::new(
            json!({"workspace": text}),
            format!("{text}\n"),
            false,
        ))
    };
    match cmd {
        WorkspaceCmd::Add { member, path } => {
            let member = member
                .canonicalize()
                .map_err(|e| format!("workspace add: {}: {e}", member.display()))?;
            let mut registry = load(&root_of(path))?;
            if !registry.members.contains(&member) {
                registry.members.push(member.clone());
                save(&root_of(path), &registry)?;
            }
            said(format!("workspace: added {}", member.display()))
        }
        WorkspaceCmd::Remove { member, path } => {
            let member = member
                .canonicalize()
                .map_err(|e| format!("workspace remove: {}: {e}", member.display()))?;
            let mut registry = load(&root_of(path))?;
            let before = registry.members.len();
            registry.members.retain(|m| m != &member);
            if registry.members.len() == before {
                return Err(format!("workspace: {} is not a member", member.display()));
            }
            save(&root_of(path), &registry)?;
            said(format!("workspace: removed {}", member.display()))
        }
        WorkspaceCmd::List { path, json } => {
            let members = members(&root_of(path))?;
            let value = json!({"members": members.iter().map(|m| m.display().to_string()).collect::<Vec<_>>()});
            let text = if members.is_empty() {
                "workspace: no members; `pixel workspace add <repo>`\n".to_owned()
            } else {
                members
                    .iter()
                    .map(|m| format!("{}\n", m.display()))
                    .collect()
            };
            Ok(Done::new(value, text, *json))
        }
        WorkspaceCmd::Clear { path } => {
            save(&root_of(path), &Registry::default())?;
            said("workspace: cleared".to_owned())
        }
    }
}

#[derive(clap::Subcommand)]
pub enum WorkspaceCmd {
    /// Register a member repository (canonicalized at add time).
    Add {
        /// Repository to add.
        member: PathBuf,
        /// Repository holding the workspace registry.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Remove a member repository.
    Remove {
        member: PathBuf,
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// List member repositories.
    List {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Remove every member.
    Clear {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("px-ws-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn add_dedups_and_remove_reports_non_members() {
        let root = scratch("members");
        let member = scratch("member");
        run(&WorkspaceCmd::Add {
            member: member.clone(),
            path: root.clone(),
        })
        .unwrap();
        run(&WorkspaceCmd::Add {
            member: member.clone(),
            path: root.clone(),
        })
        .unwrap();
        assert_eq!(members(&root).unwrap().len(), 1);
        let stranger = scratch("stranger");
        assert!(
            run(&WorkspaceCmd::Remove {
                member: stranger,
                path: root.clone(),
            })
            .is_err()
        );
        run(&WorkspaceCmd::Remove {
            member,
            path: root.clone(),
        })
        .unwrap();
        assert!(members(&root).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn empty_workspace_fans_out_to_an_error() {
        let root = scratch("empty");
        let result = fan_out(&root, &|| Request::Status {}, &|_, _| Ok(json!({})));
        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
