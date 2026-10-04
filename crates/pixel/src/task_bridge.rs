// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Shared host lifecycle binding and accounting. Native hooks are an integration boundary.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::time::Instant;

use fs2::FileExt;
use pixel_task::replay::{InternalActor, Observation, ReplayFrame, TelemetryEvent, ToolOutcome};
use pixel_task::{Action, Gate, Phase, Store, Task, TrajectoryEvent};
use serde_json::{Value, json};

use crate::task_commands::{error, observe};

/// One empty file per provider/session whose task recorded enforced gates,
/// named like its `session-locks/` entry. The hook's fallback reads it when
/// the ledger cannot answer in time, without opening the store.
const ENFORCED_SESSIONS: &str = ".pixel/tasks/enforced-sessions";

/// Whether a hook whose ledger did not answer would have been enforced:
/// `task.enforcement: enforce` in the repository settings (unreadable
/// settings count as enforced), or a session `handle_hook` marked enforced.
/// A marker lookup that fails counts as enforced. A session enforced before the
/// marker existed is marked by its next answered hook; until then its
/// configuration alone decides.
pub(crate) fn fallback_enforced(root: &Path, provider: &str, session: Option<&str>) -> bool {
    if crate::task_config::enabled(root).unwrap_or(true) {
        return true;
    }
    let Some(session) = session.filter(|id| !id.is_empty()) else {
        return false;
    };
    session_key(session)
        .and_then(|session| pixel_task::digest(&(provider, &session)).map_err(error))
        .ok()
        // An unreadable marker is not an absent one: only a lookup that proves
        // absence releases the session.
        .is_none_or(|name| {
            root.join(ENFORCED_SESSIONS)
                .join(name)
                .try_exists()
                .unwrap_or(true)
        })
}

pub(crate) fn handle_hook(
    root: &Path,
    provider: &str,
    event: &str,
    payload: &Value,
) -> Result<Value, String> {
    let started = Instant::now();
    if !matches!(provider, "claude" | "codex" | "pi") {
        return Err("unsupported task provider".into());
    }
    let mutation = payload["mutation"] == true;
    let decisive = mutation || event == "stop";
    let Some(session) = payload["session_id"].as_str().filter(|id| !id.is_empty()) else {
        return if decisive {
            Err("host session identity is missing; task edits and completion require a stable session".into())
        } else {
            Ok(json!({"decision":"observe","coverage":"unavailable"}))
        };
    };
    observed(root, provider, event, session, &payload["coverage"])?;
    let policy = std::env::var("PIXEL_TASK_POLICY").unwrap_or_else(|_| "gates_classifier".into());
    if !matches!(policy.as_str(), "retrieval" | "gates" | "gates_classifier") {
        return Err("invalid PIXEL_TASK_POLICY".into());
    }
    let configured_enforcement = crate::task_config::enabled(root)? && policy != "retrieval";
    let session = session_key(session)?;
    let store = Store::open(root).map_err(error)?;
    let binding_dir = root.join(".pixel/tasks/session-locks");
    pixel_ops::durable::ensure_dir(&binding_dir).map_err(error)?;
    let lock_name = pixel_task::digest(&(provider, &session)).map_err(error)?;
    let binding_lock =
        pixel_git::nofollow::open_lock(&binding_dir.join(&lock_name)).map_err(error)?;
    binding_lock.lock_exclusive().map_err(error)?;
    let prompt = session_prompt(root, &lock_name, event, payload)?;
    let existing = if let Some(id) = payload["task_id"].as_str() {
        let bound = store.status(id).map_err(error)?;
        let binding_session = if provider == "pi" {
            payload["binding_session_id"]
                .as_str()
                .map(session_key)
                .transpose()?
                .unwrap_or_else(|| session.clone())
        } else {
            session.clone()
        };
        if bound.provider != provider || bound.session_id.as_deref() != Some(&binding_session) {
            return Err(
                "branch task binding does not belong to this provider/session/attempt".into(),
            );
        }
        if payload["attempt_id"].as_str() != Some(bound.attempt_id.as_str()) {
            return Ok(response(
                &bound,
                if decisive { "deny" } else { "observe" },
                "task attempt changed after recovery; branch binding refreshed, retry the action",
                json!([]),
            ));
        }
        Some(bound)
    } else {
        store.find_session(provider, &session).map_err(error)?
    };
    let starts_task = event == "prompt-submit" && coding_prompt(&prompt);
    // Once a task has enforced gates, changing repository settings or evaluation
    // environment cannot silently release that task's existing obligations.
    let enforce = configured_enforcement
        || existing
            .as_ref()
            .map(|task| task_enforced(&store, task))
            .transpose()?
            .unwrap_or(false);
    if provider == "pi"
        && payload["branch_unbound"] == true
        && payload["task_id"].is_null()
        && existing.is_some()
        && !starts_task
        && decisive
        && enforce
    {
        return Err("selected Pi branch has no task binding; submit the coding objective on this branch before editing".into());
    }
    let mut created_by_mutation = false;
    let mut task = match existing {
        Some(task) if !task.phase.terminal() || !starts_task => task,
        _ if starts_task || (event == "pre-tool-use" && mutation) => {
            created_by_mutation = event == "pre-tool-use";
            let objective = if prompt.trim().is_empty() {
                "Complete the current coding task; map its acceptance criteria before verification"
            } else {
                &prompt
            };
            let mut contract = if let Some(path) = std::env::var_os("PIXEL_TASK_CONTRACT") {
                crate::task_config::from_file(root, Path::new(&path))?
            } else {
                crate::task_config::initial(
                    root,
                    &objective.chars().take(4096).collect::<String>(),
                )?
            };
            // Explicit evaluation contracts carry the frozen objective; normal prompts supply it.
            if contract.objective.is_empty() {
                contract.objective = objective.into();
            }
            let task = store
                .begin(
                    contract,
                    provider,
                    Some(&session),
                    &format!("begin-{}", event_key(event, payload)?),
                )
                .map_err(error)?;
            observe(
                &store,
                &task,
                &format!("origin-{}", task.task_id),
                "contract_origin",
                json!({
                    "repository_config":crate::config_file::preferred_path(&root.join(".pixel")),
                    "basis":"repository executable requirements plus task acceptance contract"
                }),
            )?;
            task
        }
        _ => return Ok(json!({"decision":"observe","coverage":"partial"})),
    };
    if enforce {
        observe(
            &store,
            &task,
            "hook-policy-enforce",
            "host_policy",
            json!({"enforce":true}),
        )?;
        // The marker is what an unanswered hook's fallback reads. When it
        // cannot be written, the same call must not fail open: the fallback
        // would find the marker provably absent and observe, so answer here
        // as that enforced fallback would — deny the events an enforced
        // ledger gates, let the rest through.
        let markers = root.join(ENFORCED_SESSIONS);
        let marked = pixel_ops::durable::ensure_dir(&markers)
            .and_then(|()| pixel_ops::durable::write_durably(&markers.join(&lock_name), b""));
        if (event == "stop" || (event == "pre-tool-use" && mutation))
            && let Err(failure) = marked
        {
            return Ok(response(
                &task,
                "deny",
                &format!("enforced-session marker persistence failed: {failure}"),
                json!([]),
            ));
        }
    }
    task = store.status(&task.task_id).map_err(error)?;
    drop(binding_lock);
    task = crate::task_commands::reconcile(root, &store, task)?;
    let key = event_key(event, payload)?;
    record_host(&store, &task, event, payload, &key)?;
    task = store.status(&task.task_id).map_err(error)?;
    if matches!(event, "post-tool-use" | "tool-failure") && mutation && !task.phase.terminal() {
        let source = pixel_task::snapshot::capture(root, &task.contract, false).map_err(error)?;
        if task
            .source
            .as_ref()
            .is_some_and(|prior| prior.content_id != source.content_id)
        {
            task = store
                .update(
                    &task.task_id,
                    task.revision,
                    &format!("edited-{key}"),
                    Action::Edited,
                )
                .map_err(error)?;
        }
    }
    if payload["cancelled"] == true || event == "interrupt" {
        if !task.phase.terminal() {
            task = store
                .update(
                    &task.task_id,
                    task.revision,
                    &format!("cancel-{key}"),
                    Action::Cancel,
                )
                .map_err(error)?;
        }
        return Ok(response(
            &task,
            "allow",
            "cancelled; automatic continuation stopped",
            json!([]),
        ));
    }
    let mut result = response(&task, "observe", "", json!([]));
    if event == "pre-tool-use" && mutation && enforce {
        if !task.phase.terminal() && !task.contract.checks.is_empty() {
            let decision = store.decision(&task.task_id, Gate::Edit).map_err(error)?;
            if decision
                .eligible_routes
                .contains(&pixel_task::Route::Prepare)
            {
                task =
                    crate::task_prepare::prepare(root, &store, &task, &format!("prepare-{key}"))?;
            }
        }
        let decision = store.decision(&task.task_id, Gate::Edit).map_err(error)?;
        let allowed = decision.allowed && !created_by_mutation;
        let reason = if created_by_mutation {
            format!(
                "Created task {} for the first mutation. Inspect its contract and retry after prerequisites are satisfied.",
                task.task_id
            )
        } else {
            gate_reason(&task, &decision.reasons)
        };
        result = response(
            &task,
            if allowed { "allow" } else { "deny" },
            &reason,
            json!(decision.eligible_routes),
        );
        if !allowed && let Some(call) = payload["tool_use_id"].as_str() {
            record(
                &store,
                &task,
                payload,
                &format!("blocked-{key}"),
                Observation::ToolFinished {
                    request_id: call.into(),
                    outcome: ToolOutcome::Blocked,
                    duration_ms: Some(0),
                },
            )?;
        }
    } else if event == "stop" && enforce {
        let decision = store.decision(&task.task_id, Gate::Finish).map_err(error)?;
        if decision.allowed {
            if task.phase != Phase::Complete {
                task = store
                    .update(
                        &task.task_id,
                        task.revision,
                        &format!("finish-{key}"),
                        Action::Finish,
                    )
                    .map_err(error)?;
            }
            result = response(&task, "allow", "verified complete", json!([]));
        } else if task.phase == Phase::Cancelled {
            result = response(
                &task,
                "allow",
                "cancelled; task remains unverified",
                json!([]),
            );
        } else {
            let state_key = pixel_task::digest(&(&decision.reasons, &decision.eligible_routes))
                .map_err(error)?;
            match store.update(
                &task.task_id,
                task.revision,
                &format!("correction-{key}"),
                Action::Correction { state_key },
            ) {
                Ok(updated) => {
                    task = updated;
                    let advice = crate::task_route::route(root, &store, &task, &key)?;
                    result = response(
                        &task,
                        "continue",
                        &gate_reason(&task, &decision.reasons),
                        advice["ranked_routes"].clone(),
                    );
                }
                Err(pixel_task::Error::Blocked(reason)) => {
                    result = response(
                        &task,
                        "deny",
                        &format!("unverified: {reason}; {}", decision.reasons.join("; ")),
                        json!([]),
                    );
                }
                Err(failure) => return Err(error(failure)),
            }
        }
    }
    record_internal(
        &store,
        &task,
        &format!("coordinator-{key}"),
        InternalActor::Coordinator,
        started.elapsed().as_millis() as u64,
    )?;
    Ok(result)
}

fn gate_reason(task: &Task, reasons: &[String]) -> String {
    if reasons.is_empty() {
        return String::new();
    }
    format!(
        "Task {}: {}. Use pixel task status {} --json; draft checks with pixel task contract TASK --definition '<JSON>' (no file write needed). Contract, prepare, verify, review and recovery remain available.",
        task.task_id,
        reasons.join("; "),
        task.task_id
    )
}

fn response(task: &Task, decision: &str, reason: &str, allowed: Value) -> Value {
    json!({"schema_version":1,"decision":decision,"reason":reason,"task_id":task.task_id,
        "attempt_id":task.attempt_id,"binding_session_id":task.session_id,"allowed_actions":allowed,"phase":task.phase,"coverage":"partial"})
}

fn session_key(session: &str) -> Result<String, String> {
    if pixel_task::model::valid_id(session).is_ok() {
        Ok(session.to_string())
    } else {
        Ok(format!(
            "session-{}",
            pixel_task::digest(session).map_err(error)?
        ))
    }
}

fn event_key(event: &str, payload: &Value) -> Result<String, String> {
    // Missing IDs stay distinct and coverage remains partial; never collapse separate requests by content.
    let identity = payload["event_id"]
        .as_str()
        .map_or_else(crate::task_commands::request, str::to_string);
    pixel_task::digest(&(event, identity)).map_err(error)
}

fn coding_prompt(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    let words: Vec<_> = lower
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    // A lexical hint may activate a contract early, but is never authorization.
    // Ambiguous and read-only prompts wait for the first actual mutation.
    if words.first().is_some_and(|word| {
        matches!(
            *word,
            "what"
                | "why"
                | "how"
                | "explain"
                | "compare"
                | "review"
                | "inspect"
                | "analyze"
                | "analyse"
                | "find"
                | "show"
                | "where"
                | "is"
                | "does"
        )
    }) {
        return false;
    }
    words.into_iter().take(8).any(|word| {
        matches!(
            word,
            "implement"
                | "fix"
                | "refactor"
                | "edit"
                | "change"
                | "add"
                | "remove"
                | "build"
                | "corrige"
                | "implémente"
                | "modifie"
        )
    })
}

fn task_enforced(store: &Store, task: &Task) -> Result<bool, String> {
    Ok(store
        .events(&task.task_id)
        .map_err(error)?
        .iter()
        .any(|event| {
            event.kind == "observation"
                && event.data["kind"] == "host_policy"
                && event.data["data"]["enforce"] == true
        }))
}

fn session_prompt(root: &Path, key: &str, event: &str, payload: &Value) -> Result<String, String> {
    let directory = root.join(".pixel/tasks/session-context");
    let path = directory.join(format!("{key}.json"));
    if event == "prompt-submit"
        && let Some(prompt) = payload["prompt"].as_str()
    {
        let prompt: String = prompt.chars().take(4096).collect();
        pixel_ops::durable::ensure_dir(&directory).map_err(error)?;
        pixel_ops::durable::write_durably(&path, &serde_json::to_vec(&prompt).map_err(error)?)
            .map_err(error)?;
        return Ok(prompt);
    }
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(error),
        Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(failure) => Err(error(failure)),
    }
}

fn record_host(
    store: &Store,
    task: &Task,
    event: &str,
    payload: &Value,
    key: &str,
) -> Result<(), String> {
    let call = payload["tool_use_id"].as_str();
    match event {
        "user-bash" => observe(
            store,
            task,
            &format!("external-{key}"),
            "external_action",
            json!({"kind":"user-bash","success":null}),
        )?,
        "pre-tool-use" => {
            if let Some(call) = call {
                record(
                    store,
                    task,
                    payload,
                    &format!("requested-{key}"),
                    Observation::ToolRequested {
                        request_id: call.into(),
                        tool: payload["tool_name"]
                            .as_str()
                            .filter(|label| !label.is_empty())
                            .unwrap_or("unknown")
                            .into(),
                        retry_of: payload["retry_of"].as_str().map(str::to_string),
                    },
                )?;
            }
        }
        "post-tool-use" | "tool-failure" => {
            if let Some(call) =
                call.filter(|_| event == "tool-failure" || payload["success"].is_boolean())
            {
                let outcome = if event == "tool-failure" || payload["success"] == false {
                    ToolOutcome::Failed
                } else {
                    ToolOutcome::Succeeded
                };
                record(
                    store,
                    task,
                    payload,
                    &format!("finished-{key}"),
                    Observation::ToolFinished {
                        request_id: call.into(),
                        outcome,
                        duration_ms: payload["duration_ms"].as_u64(),
                    },
                )?;
            }
        }
        "model-response" => {
            for call in payload["request_ids"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                let id = pixel_task::digest(&(key, call)).map_err(error)?;
                record(
                    store,
                    task,
                    payload,
                    &format!("model-request-{id}"),
                    Observation::ToolRequested {
                        request_id: call.into(),
                        tool: payload["request_tools"][call]
                            .as_str()
                            .unwrap_or("unknown")
                            .into(),
                        retry_of: None,
                    },
                )?;
            }
            record(
                store,
                task,
                payload,
                &format!("model-{key}"),
                Observation::ModelResponse {
                    response_id: payload["response_id"].as_str().unwrap_or(key).into(),
                    usage: serde_json::from_value(payload["usage"].clone()).ok(),
                    duration_ms: payload["duration_ms"].as_u64(),
                },
            )?;
        }
        "stop" | "session-end" | "subagent-stop" => {
            let children = payload["child_spans"]
                .as_array()
                .map_or_else(Vec::new, |items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                });
            record(
                store,
                task,
                payload,
                &format!("coverage-{key}"),
                Observation::Coverage {
                    // A finalized model response covers that response only. None
                    // of the native adapters proves full task/child coverage.
                    complete: false,
                    child_spans: children,
                    missing: vec![
                        "native hooks do not establish full model and child request coverage"
                            .into(),
                    ],
                },
            )?;
        }
        _ => (),
    }
    Ok(())
}

fn record(
    store: &Store,
    task: &Task,
    payload: &Value,
    id: &str,
    observation: Observation,
) -> Result<(), String> {
    let previous = store
        .events(&task.task_id)
        .map_err(error)?
        .into_iter()
        .find(|event| event.id == id);
    let occurred_ms = previous
        .as_ref()
        .and_then(|event| event.data["data"]["occurred_ms"].as_u64())
        .unwrap_or_else(pixel_task::now_ms);
    let event = TelemetryEvent {
        schema_version: 1,
        event_id: id.into(),
        task_id: task.task_id.clone(),
        attempt_id: task.attempt_id.clone(),
        span_id: payload["agent_id"]
            .as_str()
            .or_else(|| payload["branch_id"].as_str())
            .unwrap_or("root")
            .into(),
        parent_span_id: payload["parent_span_id"].as_str().map(str::to_string),
        host_call_id: payload["tool_use_id"].as_str().map(str::to_string),
        occurred_ms,
        observation,
    };
    let data = serde_json::to_value(&event).map_err(error)?;
    observe(store, task, id, "telemetry", data)?;
    // The durable journal may have committed before an interrupted export.
    // Re-append on delivery retries; consumers deduplicate the stable event ID.
    export(&event)?;
    Ok(())
}

pub(crate) fn record_internal(
    store: &Store,
    task: &Task,
    id: &str,
    actor: InternalActor,
    duration_ms: u64,
) -> Result<(), String> {
    // Repeat delivery of a host event is the same internal operation, not new work.
    if let Some(previous) = store
        .events(&task.task_id)
        .map_err(error)?
        .into_iter()
        .find(|event| event.id == id)
    {
        // A previous delivery can commit its duration before export fails.
        // Re-export those frozen bytes rather than replacing the observation.
        let event: TelemetryEvent =
            serde_json::from_value(previous.data["data"].clone()).map_err(error)?;
        return export(&event);
    }
    record(
        store,
        task,
        &json!({}),
        id,
        Observation::InternalCall {
            call_id: id.into(),
            actor,
            duration_ms: Some(duration_ms),
        },
    )
}

fn export(event: &TelemetryEvent) -> Result<(), String> {
    let Some(path) = std::env::var_os("PIXEL_TASK_TELEMETRY_PATH") else {
        return Ok(());
    };
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(error)?;
    file.lock_exclusive().map_err(error)?;
    let mut line = serde_json::to_vec(event).map_err(error)?;
    line.push(b'\n');
    file.write_all(&line).map_err(error)
}

fn observed(
    root: &Path,
    provider: &str,
    event: &str,
    session: &str,
    coverage: &Value,
) -> Result<(), String> {
    let path = root.join(".pixel/task-hook-observations.json");
    pixel_ops::durable::ensure_dir(&root.join(".pixel")).map_err(error)?;
    let lock = pixel_git::nofollow::open_lock(&root.join(".pixel/task-hook-observations.lock"))
        .map_err(error)?;
    lock.lock_exclusive().map_err(error)?;
    let mut value: Value = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(error)?,
        Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(failure) => return Err(error(failure)),
    };
    if !value.is_object() {
        return Err("task hook observation marker is corrupt".into());
    }
    value[provider] = json!({"schema_version":1,"provider":provider,"session_id":session_key(session)?,"event":event,"observed_unix":pixel_task::now_ms()/1000,"coverage":coverage});
    pixel_ops::durable::ensure_dir(&root.join(".pixel")).map_err(error)?;
    pixel_ops::durable::write_durably(&path, &serde_json::to_vec(&value).map_err(error)?)
        .map_err(error)
}

pub(crate) fn telemetry(events: &[TrajectoryEvent]) -> Result<Vec<TelemetryEvent>, String> {
    events
        .iter()
        .filter(|event| event.kind == "observation" && event.data["kind"] == "telemetry")
        .map(|event| serde_json::from_value(event.data["data"].clone()).map_err(error))
        .collect()
}

pub(crate) fn replay_frames(events: &[TrajectoryEvent]) -> Result<Vec<ReplayFrame>, String> {
    events
        .iter()
        .filter(|event| event.kind == "observation" && event.data["kind"] == "route")
        .map(|event| serde_json::from_value(event.data["data"]["frame"].clone()).map_err(error))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(std::path::PathBuf);
    impl Scratch {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "pixel-task-bridge-{}-{}",
                crate::task_commands::request(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&root).unwrap();
            Self(root)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // Telemetry tests must not append to an export sink inherited from the host.
    fn isolated_telemetry_test(name: &str, test: impl FnOnce()) {
        if std::env::var("PIXEL_BRIDGE_TELEMETRY_TEST").as_deref() == Ok(name) {
            test();
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("PIXEL_BRIDGE_TELEMETRY_TEST", name)
            .env_remove("PIXEL_TASK_TELEMETRY_PATH")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{name}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(&format!("test {name} ... ok")),
            "the isolated child must run the named test: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    fn fallback_should_enforce_only_configured_or_marked_sessions() {
        let root = Scratch::new();
        // No settings, no marker: a ledger that answered would only observe.
        assert!(!fallback_enforced(&root.0, "claude", Some("session-a")));
        assert!(!fallback_enforced(&root.0, "claude", None));
        assert!(!fallback_enforced(&root.0, "claude", Some("")));

        let name = pixel_task::digest(&("claude", session_key("session-a").unwrap())).unwrap();
        std::fs::create_dir_all(root.0.join(ENFORCED_SESSIONS)).unwrap();
        std::fs::write(root.0.join(ENFORCED_SESSIONS).join(name), b"").unwrap();
        assert!(fallback_enforced(&root.0, "claude", Some("session-a")));
        // The marker is per provider and per session.
        assert!(!fallback_enforced(&root.0, "codex", Some("session-a")));
        assert!(!fallback_enforced(&root.0, "claude", Some("session-b")));

        let config = crate::config_file::preferred_path(&root.0.join(".pixel"));
        std::fs::write(&config, "task:\n  enforcement: enforce\n").unwrap();
        assert!(fallback_enforced(&root.0, "codex", None));
        std::fs::write(&config, "task:\n  enforcement: sometimes\n").unwrap();
        assert!(
            fallback_enforced(&root.0, "codex", None),
            "unreadable settings count as enforced"
        );
        std::fs::write(&config, "task:\n  enforcement: advisory\n").unwrap();
        assert!(!fallback_enforced(&root.0, "codex", Some("session-a")));
        // A marker directory that cannot be read through (here: a file in its
        // place, ENOTDIR even for root) proves nothing, so it enforces.
        std::fs::remove_dir_all(root.0.join(ENFORCED_SESSIONS)).unwrap();
        std::fs::write(root.0.join(ENFORCED_SESSIONS), b"not a directory").unwrap();
        assert!(fallback_enforced(&root.0, "codex", Some("session-a")));
    }

    #[test]
    fn an_enforced_hook_should_leave_the_marker_its_fallback_reads() {
        let root = Scratch::new();
        let config = crate::config_file::preferred_path(&root.0.join(".pixel"));
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, "task:\n  enforcement: advisory\n").unwrap();
        let prompt =
            json!({"session_id":"s1","prompt":"fix the parser bug in src/a.rs","mutation":false});
        handle_hook(&root.0, "claude", "prompt-submit", &prompt).unwrap();
        assert!(
            !root.0.join(ENFORCED_SESSIONS).exists(),
            "advisory sessions leave no marker"
        );

        std::fs::write(&config, "task:\n  enforcement: enforce\n").unwrap();
        handle_hook(&root.0, "claude", "prompt-submit", &prompt).unwrap();
        // Enforcement turned off afterwards: the task keeps its obligations, and
        // so does the fallback that cannot open the store.
        std::fs::write(&config, "task:\n  enforcement: advisory\n").unwrap();
        assert!(fallback_enforced(&root.0, "claude", Some("s1")));
        assert!(!fallback_enforced(&root.0, "claude", Some("s2")));
    }

    #[test]
    fn an_unwritable_marker_should_deny_what_an_enforced_ledger_gates() {
        let root = Scratch::new();
        let config = crate::config_file::preferred_path(&root.0.join(".pixel"));
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, "task:\n  enforcement: enforce\n").unwrap();
        // A read-only markers directory fails the durable write while the
        // lookup still proves the marker absent — the state whose fallback
        // releases the session. Root ignores directory mode bits, and so can
        // any environment where the chmod fails to bite: when the write
        // succeeded anyway the fixture never established that state, and the
        // case is skipped rather than asserting a world it did not build.
        let markers = root.0.join(ENFORCED_SESSIONS);
        std::fs::create_dir_all(&markers).unwrap();
        let original = std::fs::metadata(&markers).unwrap().permissions();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut locked = original.clone();
            locked.set_mode(locked.mode() & !0o222);
            std::fs::set_permissions(&markers, locked).unwrap();
        }
        let marker_name =
            |session: &str| pixel_task::digest(&("claude", session_key(session).unwrap())).unwrap();

        // The task starts enforced: host_policy is recorded before the marker
        // write, so the obligation survives the failure; a prompt-submit gates
        // nothing, so its own answer continues normally.
        let prompt =
            json!({"session_id":"s1","prompt":"fix the parser bug in src/a.rs","mutation":false});
        let started = handle_hook(&root.0, "claude", "prompt-submit", &prompt).unwrap();
        assert_eq!(started["decision"], "observe", "{started}");
        if markers.join(marker_name("s1")).exists() {
            // The chmod did not bite (a privileged run can bypass directory
            // mode bits): the write succeeded, the fixture never established
            // an unwritable marker, and the deny-below would assert a state
            // this run cannot build.
            std::fs::set_permissions(&markers, original).unwrap();
            return;
        }

        std::fs::write(&config, "task:\n  enforcement: advisory\n").unwrap();
        // The settings no longer enforce, the task's host_policy record does,
        // and the marker is provably absent — the fallback alone would observe.
        assert!(!fallback_enforced(&root.0, "claude", Some("s1")));
        let denied = handle_hook(
            &root.0,
            "claude",
            "pre-tool-use",
            &json!({"session_id":"s1","mutation":true}),
        )
        .unwrap();
        assert_eq!(denied["decision"], "deny", "{denied}");
        let stopped = handle_hook(
            &root.0,
            "claude",
            "stop",
            &json!({"session_id":"s1","mutation":false}),
        )
        .unwrap();
        assert_eq!(stopped["decision"], "deny", "{stopped}");
        // The marker failure denies only the gated events: a read-only
        // pre-tool hook still answers as an observing ledger would.
        let observed = handle_hook(
            &root.0,
            "claude",
            "pre-tool-use",
            &json!({"session_id":"s1","mutation":false}),
        )
        .unwrap();
        assert_eq!(observed["decision"], "observe", "{observed}");

        std::fs::set_permissions(&markers, original).unwrap();
    }

    #[test]
    fn missing_session_should_block_stop_independently_of_mutation() {
        let root = Scratch::new();
        for (event, mutation) in [("stop", false), ("pre-tool-use", true)] {
            assert_eq!(
                handle_hook(&root.0, "pi", event, &json!({"mutation":mutation})).unwrap_err(),
                "host session identity is missing; task edits and completion require a stable session"
            );
        }
        assert_eq!(
            handle_hook(&root.0, "pi", "session-start", &json!({"mutation":false})).unwrap(),
            json!({"decision":"observe","coverage":"unavailable"})
        );
    }

    #[test]
    fn retained_enforcement_requires_a_host_policy_with_true_enforcement() {
        let root = Scratch::new();
        let store = Store::open(&root.0).unwrap();
        let contract = serde_json::from_value(json!({
            "version":1,"objective":"fix source","checks":[],"criteria":[]
        }))
        .unwrap();
        let task = store.begin(contract, "pi", Some("s"), "begin").unwrap();
        assert!(!task_enforced(&store, &task).unwrap());
        observe(
            &store,
            &task,
            "unrelated",
            "diagnostic",
            json!({"enforce":true}),
        )
        .unwrap();
        assert!(!task_enforced(&store, &task).unwrap());
        observe(
            &store,
            &task,
            "disabled",
            "host_policy",
            json!({"enforce":false}),
        )
        .unwrap();
        assert!(!task_enforced(&store, &task).unwrap());
        observe(
            &store,
            &task,
            "enabled",
            "host_policy",
            json!({"enforce":true}),
        )
        .unwrap();
        assert!(task_enforced(&store, &task).unwrap());
    }

    #[test]
    fn prompt_context_should_bound_content_preserve_sessions_and_report_corruption() {
        let root = Scratch::new();
        assert_eq!(
            session_prompt(&root.0, "one", "pre-tool-use", &json!({})).unwrap(),
            ""
        );
        let bounded = "é".repeat(4096);
        assert_eq!(
            session_prompt(
                &root.0,
                "one",
                "prompt-submit",
                &json!({"prompt":format!("{bounded}extra")})
            )
            .unwrap(),
            bounded
        );
        assert_eq!(
            session_prompt(
                &root.0,
                "one",
                "pre-tool-use",
                &json!({"prompt":"not a user prompt"})
            )
            .unwrap(),
            bounded
        );
        assert_eq!(
            session_prompt(&root.0, "two", "pre-tool-use", &json!({})).unwrap(),
            ""
        );
        let path = root.0.join(".pixel/tasks/session-context/one.json");
        std::fs::write(&path, "{").unwrap();
        assert!(session_prompt(&root.0, "one", "pre-tool-use", &json!({})).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(session_prompt(&root.0, "one", "pre-tool-use", &json!({})).is_err());
    }

    #[test]
    fn observed_markers_should_preserve_each_provider_and_reject_corruption() {
        let root = Scratch::new();
        observed(
            &root.0,
            "pi",
            "pre-tool-use",
            "pi-session",
            &json!({"model":"partial"}),
        )
        .unwrap();
        observed(
            &root.0,
            "codex",
            "stop",
            "codex-session",
            &json!({"model":"partial"}),
        )
        .unwrap();
        let path = root.0.join(".pixel/task-hook-observations.json");
        let markers: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(markers.as_object().unwrap().len(), 2);
        assert_eq!(markers["pi"]["session_id"], "pi-session");
        assert_eq!(markers["codex"]["event"], "stop");
        for corrupt in ["[]", "{"] {
            std::fs::write(&path, corrupt).unwrap();
            assert!(observed(&root.0, "pi", "stop", "s", &json!({})).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), corrupt);
        }
    }

    #[test]
    #[cfg(unix)]
    fn observed_marker_read_errors_must_not_replace_existing_state() {
        let root = Scratch::new();
        std::fs::create_dir(root.0.join(".pixel")).unwrap();
        let path = root.0.join(".pixel/task-hook-observations.json");
        let target = Path::new("task-hook-observations.json");
        std::os::unix::fs::symlink(target, &path).unwrap();
        let read_error = std::fs::read(&path).unwrap_err();
        assert_eq!(read_error.raw_os_error(), Some(libc::ELOOP));
        assert_eq!(
            observed(&root.0, "codex", "stop", "session", &json!({})).unwrap_err(),
            read_error.to_string()
        );
        assert_eq!(std::fs::read_link(&path).unwrap(), target);

        std::fs::remove_file(&path).unwrap();
        observed(&root.0, "codex", "stop", "session", &json!({})).unwrap();
        let marker: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(marker.as_object().unwrap().len(), 1);
        assert_eq!(marker["codex"]["session_id"], "session");
        assert_eq!(marker["codex"]["event"], "stop");
    }

    #[test]
    fn identity_should_separate_sessions_and_event_kinds_without_content_deduplication() {
        assert_ne!(session_key("one").unwrap(), session_key("two").unwrap());
        let payload = json!({"event_id":"host-1"});
        assert_eq!(
            event_key("pre-tool-use", &payload).unwrap(),
            event_key("pre-tool-use", &payload).unwrap()
        );
        assert_ne!(
            event_key("pre-tool-use", &payload).unwrap(),
            event_key("post-tool-use", &payload).unwrap()
        );
        assert_ne!(
            event_key("pre-tool-use", &json!({})).unwrap(),
            event_key("pre-tool-use", &json!({})).unwrap()
        );
        assert!(coding_prompt("Implement a task gate"));
        assert!(!coding_prompt("What is a gate?"));
        assert!(!coding_prompt(""));
        assert!(!coding_prompt("How do I implement this?"));
        assert!(coding_prompt("  Please implement this"));
        assert_eq!(session_key("valid-session").unwrap(), "valid-session");
        assert!(
            session_key("invalid session/path")
                .unwrap()
                .starts_with("session-")
        );
    }

    #[test]
    fn record_host_should_keep_tool_names_and_finalize_coverage() {
        isolated_telemetry_test(
            "task_bridge::tests::record_host_should_keep_tool_names_and_finalize_coverage",
            || {
                let root = Scratch::new();
                let store = Store::open(&root.0).unwrap();
                let contract = serde_json::from_value(json!({
                    "version":1,"objective":"fix source","checks":[],"criteria":[]
                }))
                .unwrap();
                let task = store.begin(contract, "pi", Some("s"), "begin").unwrap();

                for (index, (label, expected)) in [
                    (Some("Bash"), "Bash"),
                    (Some("Read"), "Read"),
                    (Some(""), "unknown"),
                    (None, "unknown"),
                ]
                .into_iter()
                .enumerate()
                {
                    let call = format!("call-{index}");
                    let mut payload = json!({"tool_use_id":call,"retry_of":"earlier-call"});
                    if let Some(label) = label {
                        payload["tool_name"] = json!(label);
                    }
                    record_host(&store, &task, "pre-tool-use", &payload, &call).unwrap();
                    let recorded = telemetry(&store.events(&task.task_id).unwrap()).unwrap();
                    assert_eq!(recorded.len(), index + 1);
                    assert_eq!(recorded[index].host_call_id.as_deref(), Some(call.as_str()));
                    assert_eq!(
                        recorded[index].observation,
                        Observation::ToolRequested {
                            request_id: call,
                            tool: expected.into(),
                            retry_of: Some("earlier-call".into()),
                        }
                    );
                }
                for (index, event) in ["stop", "session-end", "subagent-stop"]
                    .into_iter()
                    .enumerate()
                {
                    record_host(
                        &store,
                        &task,
                        event,
                        &json!({"child_spans":["child-1", 7, null]}),
                        event,
                    )
                    .unwrap();
                    let recorded = telemetry(&store.events(&task.task_id).unwrap()).unwrap();
                    assert_eq!(recorded.len(), 5 + index, "{event}");
                    let ending = recorded.last().unwrap();
                    assert_eq!(ending.event_id, format!("coverage-{event}"));
                    assert_eq!(
                        ending.observation,
                        Observation::Coverage {
                            complete: false,
                            child_spans: vec!["child-1".into()],
                            missing: vec!["native hooks do not establish full model and child request coverage".into()],
                        },
                        "{event}"
                    );
                    let summary =
                        pixel_task::replay::summarize_trajectory(std::slice::from_ref(ending))
                            .unwrap();
                    assert_eq!(summary.coverage, pixel_task::replay::Coverage::Partial);
                    assert_eq!(summary.model_tool_requests, None);
                    assert_eq!(summary.observed_model_tool_requests, 0);
                    assert_eq!(
                        summary.missing,
                        vec![
                            "incomplete span root",
                            "native hooks do not establish full model and child request coverage",
                            "uncovered span child-1",
                            "uncovered span root",
                        ],
                        "{event}"
                    );
                }
                let recorded = telemetry(&store.events(&task.task_id).unwrap()).unwrap();
                let summary = pixel_task::replay::summarize_trajectory(&recorded).unwrap();
                assert_eq!(summary.coverage, pixel_task::replay::Coverage::Partial);
                assert_eq!(summary.model_tool_requests, None);
                assert_eq!(summary.observed_model_tool_requests, 4);
            },
        );
    }
}
