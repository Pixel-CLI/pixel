//! pixel-flow — deterministic browser flow replay for LLM agents.
//!
//! Saves, retrieves, lists, revises, and replays proven agent-browser paths
//! (auth flows, config flows) so the agent follows a deterministic shortcut
//! instead of re-discovering the UI from scratch every time.
//!
//! Storage is file-based (one JSON per flow in `~/.local/share/pixel/flows/`)
//! — no SQLite, no daemon. Simple, inspectable, human-editable.

pub mod execute;
pub mod replay;
pub mod store;
pub mod types;

pub use execute::{execute, ExecResult};
pub use store::{delete, ensure_flow_dir, exists, flow_dir, list, load, save, slugify};
pub use types::{Flow, FlowStep, FlowVar};

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::{json, Value};

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
    Replay {
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
        FlowAction::Replay {
            name,
            vars,
            dry_run,
        } => replay_flow(name, vars, *dry_run),
        FlowAction::Execute { name, vars } => execute_flow(name, vars),
        FlowAction::Delete { name } => delete_flow(name),
        FlowAction::Show { name } => show_flow(name),
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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
    Ok(serde_json::to_value(&flow)
        .map_err(|e| format!("cannot serialize flow: {e}"))?)
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
                    .map(|tags| tags.iter().any(|tag| tag.as_str() == Some(t.as_str())))
                    .unwrap_or(false)
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

fn replay_flow(
    name: &str,
    vars: &HashMap<String, String>,
    dry_run: bool,
) -> Result<Value, String> {
    let flow = load(name)?;
    let output = replay::replay(&flow, vars)?;
    Ok(json!({
        "name": flow.name,
        "dry_run": dry_run,
        "output": output,
    }))
}

fn execute_flow(
    name: &str,
    vars: &HashMap<String, String>,
) -> Result<Value, String> {
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
    let pretty = serde_json::to_string_pretty(&flow)
        .map_err(|e| format!("cannot serialize flow: {e}"))?;
    Ok(json!({
        "name": flow.name,
        "output": pretty,
    }))
}
