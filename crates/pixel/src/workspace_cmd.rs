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

use pixel_index::index::SHARD_DIR;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use pixel_daemon::api::Request;

const WORKSPACE_FILE: &str = "workspace.json";

#[derive(Debug, Default, Serialize, Deserialize)]
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

fn save(root: &Path, registry: &Registry) -> Result<(), String> {
    let path = registry_path(root);
    if let Some(dir) = path.parent() {
        pixel_git::sidecar::private_dir(dir).map_err(|e| format!("workspace dir: {e}"))?;
    }
    let bytes = serde_json::to_vec_pretty(registry).map_err(|e| e.to_string())?;
    // A fresh temporary file renamed over the name: a link committed at
    // either name is replaced, never written through.
    pixel_git::nofollow::write_replace(&path, &bytes, pixel_git::nofollow::PRIVATE_MODE)
        .map_err(|e| format!("workspace write: {e}"))
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
        return Err("no workspace members — `pixel workspace add <repo>` first".to_string());
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

pub fn run(cmd: WorkspaceCmd) -> Result<(), String> {
    match cmd {
        WorkspaceCmd::Add { member, path } => {
            let member = member
                .canonicalize()
                .map_err(|e| format!("workspace add: {}: {e}", member.display()))?;
            let mut registry = load(&path)?;
            if !registry.members.contains(&member) {
                registry.members.push(member.clone());
                save(&path, &registry)?;
            }
            println!("workspace: added {}", member.display());
            Ok(())
        }
        WorkspaceCmd::Remove { member, path } => {
            let member = member
                .canonicalize()
                .map_err(|e| format!("workspace remove: {}: {e}", member.display()))?;
            let mut registry = load(&path)?;
            let before = registry.members.len();
            registry.members.retain(|m| m != &member);
            if registry.members.len() == before {
                return Err(format!("workspace: {} is not a member", member.display()));
            }
            save(&path, &registry)?;
            println!("workspace: removed {}", member.display());
            Ok(())
        }
        WorkspaceCmd::List { path, json } => {
            let members = members(&path)?;
            if json {
                let out = json!({"members": members.iter().map(|m| m.display().to_string()).collect::<Vec<_>>()});
                println!(
                    "{}",
                    serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?
                );
            } else if members.is_empty() {
                println!("workspace: no members — `pixel workspace add <repo>`");
            } else {
                for m in &members {
                    println!("{}", m.display());
                }
            }
            Ok(())
        }
        WorkspaceCmd::Clear { path } => {
            save(&path, &Registry::default())?;
            println!("workspace: cleared");
            Ok(())
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
        run(WorkspaceCmd::Add {
            member: member.clone(),
            path: root.clone(),
        })
        .unwrap();
        run(WorkspaceCmd::Add {
            member: member.clone(),
            path: root.clone(),
        })
        .unwrap();
        assert_eq!(members(&root).unwrap().len(), 1);
        let stranger = scratch("stranger");
        assert!(
            run(WorkspaceCmd::Remove {
                member: stranger,
                path: root.clone(),
            })
            .is_err()
        );
        run(WorkspaceCmd::Remove {
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
        let result = fan_out(&root, &|| Request::Status {});
        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
