// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! pixel-flow — deterministic browser flow runtime for LLM agents.
//!
//! Saves, retrieves, lists, revises, and runs proven agent-browser paths
//! (auth flows, config flows) so the agent follows a deterministic shortcut
//! instead of re-discovering the UI from scratch every time.
//!
//! Storage is file-based (one JSON per flow in `~/.local/share/pixel/flows/`)
//! — no SQLite, no daemon. Simple, inspectable, human-editable.

pub mod execute;
pub mod run;
pub mod store;
pub mod types;

pub mod vars;

pub use execute::{Browser, ExecResult, agent_browser, evaluate_condition, execute, execute_step};
pub use store::{delete, ensure_flow_dir, exists, flow_dir, list, load, save, slugify};
pub use types::{Flow, FlowStep, FlowVar};
pub use vars::substitute;

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::{Value, json};

/// Subactions of the `pixel flow` command group.
#[derive(Debug, Clone)]
pub enum FlowAction {
    /// Create a new flow. Refuses to overwrite an existing flow.
    Save {
        name: String,
        title: String,
        description: String,
        tags: Vec<String>,
        url: Option<String>,
        from_file: Option<PathBuf>,
    },
    /// Retrieve a flow by name (JSON or pretty-printed).
    Get { name: String },
    /// List all saved flows (optionally filtered by tag).
    List { tag: Option<String> },
    /// Update an existing flow's metadata and/or steps. Bumps revision.
    Revise {
        name: String,
        title: Option<String>,
        description: Option<String>,
        from_file: Option<PathBuf>,
    },
    /// Emit ready-to-run agent-browser commands with variable substitution.
    Run {
        name: String,
        vars: HashMap<String, String>,
        dry_run: bool,
    },
    /// Actually execute the flow by running agent-browser commands.
    Execute {
        name: String,
        vars: HashMap<String, String>,
    },
    /// Delete a flow by name.
    Delete { name: String },
    /// Pretty-print the full flow document (human-readable).
    Show { name: String },
}

/// Entry point for all flow subactions.
pub fn flow(action: &FlowAction) -> Result<Value, String> {
    match action {
        FlowAction::Save {
            name,
            title,
            description,
            tags,
            url,
            from_file,
        } => save_flow(name, title, description, tags, url, from_file),
        FlowAction::Get { name } => get_flow(name),
        FlowAction::List { tag } => list_flows(tag),
        FlowAction::Revise {
            name,
            title,
            description,
            from_file,
        } => revise_flow(name, title, description, from_file),
        FlowAction::Run {
            name,
            vars,
            dry_run,
        } => run_flow(name, vars, *dry_run),
        FlowAction::Execute { name, vars } => execute_flow(name, vars),
        FlowAction::Delete { name } => delete_flow(name),
        FlowAction::Show { name } => show_flow(name),
    }
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn save_flow(
    name: &str,
    title: &str,
    description: &str,
    tags: &[String],
    url: &Option<String>,
    from_file: &Option<PathBuf>,
) -> Result<Value, String> {
    if exists(name) {
        return Err(format!(
            "flow '{}' already exists — use `pixel flow revise {}` to update it",
            slugify(name),
            slugify(name)
        ));
    }
    let now = now_unix();
    let flow = if let Some(path) = from_file {
        let data = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read steps file {}: {e}", path.display()))?;
        let trimmed = data.trim();
        if trimmed.starts_with('[') {
            // Bare steps array — construct a Flow from CLI args + steps.
            let steps: Vec<FlowStep> = serde_json::from_str(&data)
                .map_err(|e| format!("cannot parse steps JSON from {}: {e}", path.display()))?;
            Flow {
                name: name.to_string(),
                title: title.to_string(),
                description: description.to_string(),
                tags: tags.to_vec(),
                url: url.clone(),
                tab: None,
                success_url_contains: vec![],
                success_url_excludes: vec![],
                mfa_keywords: vec![],
                stale_tab_cleanup: vec![],
                preconditions: vec![],
                vars: vec![],
                steps,
                success_signal: None,
                created_unix: now,
                revised_unix: now,
                revision: 1,
                proven: false,
            }
        } else {
            // Full flow document — all fields optional except `steps`.
            // CLI args override file values when provided.
            #[derive(serde::Deserialize)]
            struct FlowInput {
                #[serde(default)]
                steps: Vec<FlowStep>,
                #[serde(default)]
                vars: Vec<FlowVar>,
                #[serde(default)]
                tab: Option<String>,
                #[serde(default)]
                url: Option<String>,
                #[serde(default)]
                success_signal: Option<String>,
                #[serde(default)]
                success_url_contains: Vec<String>,
                #[serde(default)]
                success_url_excludes: Vec<String>,
                #[serde(default)]
                mfa_keywords: Vec<String>,
                #[serde(default)]
                stale_tab_cleanup: Vec<String>,
                #[serde(default)]
                preconditions: Vec<String>,
            }
            let doc: FlowInput = serde_json::from_str(&data)
                .map_err(|e| format!("cannot parse flow doc from {}: {e}", path.display()))?;
            Flow {
                name: name.to_string(),
                title: title.to_string(),
                description: description.to_string(),
                tags: tags.to_vec(),
                url: url.clone().or(doc.url),
                tab: doc.tab,
                success_url_contains: doc.success_url_contains,
                success_url_excludes: doc.success_url_excludes,
                mfa_keywords: doc.mfa_keywords,
                stale_tab_cleanup: doc.stale_tab_cleanup,
                preconditions: doc.preconditions,
                vars: doc.vars,
                steps: doc.steps,
                success_signal: doc.success_signal,
                created_unix: now,
                revised_unix: now,
                revision: 1,
                proven: false,
            }
        }
    } else {
        return Err(
            "no steps provided — use --from-file <path> to supply the steps JSON array or full flow document".into(),
        );
    };
    let path = save(&flow)?;
    Ok(json!({
        "saved": true,
        "name": flow.name,
        "path": path.display().to_string(),
        "steps": flow.steps.len(),
        "revision": flow.revision,
    }))
}

fn get_flow(name: &str) -> Result<Value, String> {
    let flow = load(name)?;
    serde_json::to_value(&flow).map_err(|e| format!("cannot serialize flow: {e}"))
}

fn list_flows(tag: &Option<String>) -> Result<Value, String> {
    let all = list()?;
    if let Some(t) = tag {
        let filtered: Vec<Value> = all
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|v| {
                v["tags"]
                    .as_array()
                    .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some(t.as_str())))
            })
            .collect();
        return Ok(Value::Array(filtered));
    }
    Ok(all)
}

fn revise_flow(
    name: &str,
    title: &Option<String>,
    description: &Option<String>,
    from_file: &Option<PathBuf>,
) -> Result<Value, String> {
    let mut flow = load(name)?;
    if let Some(t) = title {
        flow.title = t.clone();
    }
    if let Some(d) = description {
        flow.description = d.clone();
    }
    if let Some(path) = from_file {
        let data = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read steps file {}: {e}", path.display()))?;
        let trimmed = data.trim();
        if trimmed.starts_with('[') {
            let parsed: Vec<FlowStep> = serde_json::from_str(&data)
                .map_err(|e| format!("cannot parse steps JSON from {}: {e}", path.display()))?;
            flow.steps = parsed;
        } else {
            // Full flow document — all fields optional except `steps`.
            #[derive(serde::Deserialize)]
            struct FlowInput {
                #[serde(default)]
                steps: Vec<FlowStep>,
                #[serde(default)]
                vars: Vec<FlowVar>,
                #[serde(default)]
                tab: Option<String>,
                #[serde(default)]
                url: Option<String>,
                #[serde(default)]
                success_signal: Option<String>,
                #[serde(default)]
                success_url_contains: Vec<String>,
                #[serde(default)]
                success_url_excludes: Vec<String>,
                #[serde(default)]
                mfa_keywords: Vec<String>,
                #[serde(default)]
                stale_tab_cleanup: Vec<String>,
                #[serde(default)]
                preconditions: Vec<String>,
            }
            let doc: FlowInput = serde_json::from_str(&data)
                .map_err(|e| format!("cannot parse flow doc from {}: {e}", path.display()))?;
            flow.steps = doc.steps;
            flow.vars = doc.vars;
            flow.success_signal = doc.success_signal;
            flow.tab = doc.tab;
            flow.success_url_contains = doc.success_url_contains;
            flow.success_url_excludes = doc.success_url_excludes;
            flow.mfa_keywords = doc.mfa_keywords;
            flow.stale_tab_cleanup = doc.stale_tab_cleanup;
            flow.preconditions = doc.preconditions;
            if doc.url.is_some() {
                flow.url = doc.url;
            }
        }
    }
    flow.revised_unix = now_unix();
    flow.revision += 1;
    let path = save(&flow)?;
    Ok(json!({
        "revised": true,
        "name": flow.name,
        "path": path.display().to_string(),
        "revision": flow.revision,
        "steps": flow.steps.len(),
    }))
}

fn run_flow(name: &str, vars: &HashMap<String, String>, dry_run: bool) -> Result<Value, String> {
    let flow = load(name)?;
    let output = run::run(&flow, vars)?;
    Ok(json!({
        "name": flow.name,
        "dry_run": dry_run,
        "output": output,
    }))
}

fn execute_flow(name: &str, vars: &HashMap<String, String>) -> Result<Value, String> {
    let flow = load(name)?;
    let result = execute::execute(&flow, vars);
    Ok(json!({
        "name": flow.name,
        "success": result.success,
        "steps_executed": result.steps_executed,
        "steps_skipped": result.steps_skipped,
        "error": result.error,
        "log": result.log,
    }))
}

fn delete_flow(name: &str) -> Result<Value, String> {
    let deleted = delete(name)?;
    Ok(json!({
        "deleted": deleted,
        "name": slugify(name),
    }))
}

fn show_flow(name: &str) -> Result<Value, String> {
    let flow = load(name)?;
    let pretty =
        serde_json::to_string_pretty(&flow).map_err(|e| format!("cannot serialize flow: {e}"))?;
    Ok(json!({
        "name": flow.name,
        "output": pretty,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FlowStep;

    /// `created_unix`/`revised_unix` on a saved flow come from here; a
    /// placeholder would date every flow to 1970 (or to the future).
    #[test]
    fn now_unix_is_the_current_unix_epoch_in_seconds() {
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after 1970")
            .as_secs() as i64;
        let ts = now_unix();
        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after 1970")
            .as_secs() as i64;
        assert!(ts >= before && ts <= after, "{before} <= {ts} <= {after}");
        assert!(ts > 1_577_836_800, "{ts}"); // 2020-01-01T00:00:00Z
    }

    fn tagged(name: &str, tags: &[&str]) -> Flow {
        Flow {
            name: name.into(),
            title: name.into(),
            description: String::new(),
            tags: tags.iter().map(|t| (*t).to_string()).collect(),
            url: None,
            tab: None,
            success_url_contains: vec![],
            success_url_excludes: vec![],
            mfa_keywords: vec![],
            stale_tab_cleanup: vec![],
            preconditions: vec![],
            vars: vec![],
            steps: vec![FlowStep {
                action: "snapshot".into(),
                ..Default::default()
            }],
            success_signal: None,
            created_unix: 1,
            revised_unix: 1,
            revision: 1,
            proven: false,
        }
    }

    /// A proven flow is only replaced through `revise`, which bumps the
    /// revision: a second `save` under the same name must refuse, and its
    /// error names the command the agent can run instead.
    #[test]
    fn save_flow_writes_a_new_flow_and_refuses_to_overwrite_it() {
        let _guard = store::ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: ENV_MUTEX serialises every test that touches PIXEL_FLOW_DIR;
        // nothing else in this process reads it concurrently.
        unsafe {
            std::env::set_var("PIXEL_FLOW_DIR", tmp.path());
        }
        let steps = tmp.path().join("steps.json");
        std::fs::write(&steps, r#"[{"action":"snapshot"}]"#).unwrap();
        let from_file = Some(steps);
        let saved = save_flow("Login Flow", "Login", "", &[], &None, &from_file).unwrap();
        assert_eq!(saved["saved"], true, "{saved}");
        assert_eq!(saved["steps"], 1, "{saved}");
        assert_eq!(saved["revision"], 1, "{saved}");
        assert_eq!(load("Login Flow").unwrap().title, "Login");

        let again = save_flow("Login Flow", "Other", "", &[], &None, &from_file).unwrap_err();
        assert_eq!(
            again,
            "flow 'login-flow' already exists — use `pixel flow revise login-flow` to update it"
        );
        assert_eq!(
            load("Login Flow").unwrap().title,
            "Login",
            "not overwritten"
        );
        // SAFETY: as above.
        unsafe {
            std::env::remove_var("PIXEL_FLOW_DIR");
        }
    }

    #[test]
    fn list_flows_returns_every_flow_or_only_those_carrying_the_tag() {
        let _guard = store::ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: ENV_MUTEX serialises every test that touches PIXEL_FLOW_DIR;
        // nothing else in this process reads it concurrently.
        unsafe {
            std::env::set_var("PIXEL_FLOW_DIR", tmp.path());
        }
        store::save(&tagged("alpha", &["auth", "github"])).unwrap();
        store::save(&tagged("beta", &["auth"])).unwrap();
        store::save(&tagged("gamma", &[])).unwrap();
        let names = |v: Value| -> Vec<String> {
            v.as_array()
                .unwrap()
                .iter()
                .map(|f| f["name"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(
            names(list_flows(&None).unwrap()),
            ["alpha", "beta", "gamma"]
        );
        assert_eq!(
            names(list_flows(&Some("auth".into())).unwrap()),
            ["alpha", "beta"]
        );
        assert_eq!(
            names(list_flows(&Some("github".into())).unwrap()),
            ["alpha"]
        );
        assert!(names(list_flows(&Some("none".into())).unwrap()).is_empty());
        // SAFETY: as above.
        unsafe {
            std::env::remove_var("PIXEL_FLOW_DIR");
        }
    }

    /// `flow()` dispatches to the real action handlers: a mutant that
    /// replaces the body with `Ok(Default::default())` short-circuits every
    /// action to a JSON null and silently drops the field the caller
    /// asked for. Pin each action shape with at least one field the
    /// action fills in.
    #[test]
    fn flow_dispatch_returns_action_specific_fields_not_a_null_default() {
        let _guard = store::ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: ENV_MUTEX serialises every test that touches PIXEL_FLOW_DIR.
        unsafe {
            std::env::set_var("PIXEL_FLOW_DIR", tmp.path());
        }
        let steps = tmp.path().join("steps.json");
        std::fs::write(&steps, r#"[{"action":"snapshot"}]"#).unwrap();
        let from_file = Some(steps);
        let saved = flow(&FlowAction::Save {
            name: "audit".into(),
            title: "Audit".into(),
            description: String::new(),
            tags: vec![],
            url: None,
            from_file,
        })
        .unwrap();
        assert_eq!(saved["saved"], true, "{saved}");
        assert_eq!(saved["steps"], 1, "{saved}");

        let listed = flow(&FlowAction::List { tag: None }).unwrap();
        assert!(!listed.as_array().unwrap().is_empty(), "{listed}");

        let fetched = flow(&FlowAction::Get {
            name: "audit".into(),
        })
        .unwrap();
        assert_eq!(fetched["title"], "Audit", "{fetched}");

        let rendered = flow(&FlowAction::Run {
            name: "audit".into(),
            vars: HashMap::new(),
            dry_run: true,
        })
        .unwrap();
        assert_eq!(rendered["dry_run"], true, "{rendered}");
        assert!(rendered["output"].is_string(), "{rendered}");

        let shown = flow(&FlowAction::Show {
            name: "audit".into(),
        })
        .unwrap();
        assert!(shown["name"].is_string(), "{shown}");

        // SAFETY: as above.
        unsafe {
            std::env::remove_var("PIXEL_FLOW_DIR");
        }
    }

    /// `run_flow` resolves the flow, runs the engine, and returns the
    /// rendered output. A mutant that returns `Ok(Default::default())`
    /// returns a JSON null and skips the entire pipeline.
    #[test]
    fn run_flow_returns_the_engine_output_not_a_null_default() {
        let _guard = store::ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: as above.
        unsafe {
            std::env::set_var("PIXEL_FLOW_DIR", tmp.path());
        }
        let steps = tmp.path().join("steps.json");
        std::fs::write(&steps, r#"[{"action":"snapshot"}]"#).unwrap();
        let from_file = Some(steps);
        save_flow("audit", "Audit", "", &[], &None, &from_file).unwrap();
        let value = run_flow("audit", &HashMap::new(), true).unwrap();
        assert!(value["name"].is_string(), "{value}");
        assert_eq!(value["dry_run"], true, "{value}");
        assert!(value["output"].is_string(), "{value}");
        // SAFETY: serialised by ENV_MUTEX; restores the env so the next
        // test starts clean.
        unsafe {
            std::env::remove_var("PIXEL_FLOW_DIR");
        }
    }
}
