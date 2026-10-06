// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Commands for durable task evidence; legacy Claude packet commands remain compatible.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use pixel_task::{Action, Gate, Store, Task, TrajectoryEvent};
use serde_json::{Value, json};

thread_local! {
    static ACTIVE_CORRELATION: std::cell::RefCell<Option<pixel_actionlog::TaskCorrelation>> = const { std::cell::RefCell::new(None) };
}

#[derive(Debug, Args)]
pub(crate) struct TaskRef {
    task_id: String,
    #[arg(default_value = ".")]
    path: PathBuf,
    #[arg(long)]
    json: bool,
    /// Stable retry identity; reuse the same value for the same operation.
    #[arg(long)]
    request_id: Option<String>,
    /// Reject a mutation if another writer has advanced this revision.
    #[arg(long)]
    expected_revision: Option<u64>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum TaskCmd {
    /// Begin an unverified task with repository checks and acceptance criteria.
    Begin {
        objective: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value = "claude")]
        provider: String,
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        contract: Option<PathBuf>,
        #[arg(long)]
        request_id: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Collect bounded scope, impact, and test suggestions for current source.
    Prepare(TaskRef),
    /// Read state and reevaluate edit and completion gates against current source.
    Status(TaskRef),
    /// Read the durable journal and measured trajectory summary.
    Events(TaskRef),
    /// Read or revise a contract from a JSON/YAML file or inline JSON definition.
    Contract {
        #[command(flatten)]
        task: TaskRef,
        #[arg(long, conflicts_with = "definition")]
        file: Option<PathBuf>,
        /// Draft a contract without a file write while the edit gate is closed.
        #[arg(long)]
        definition: Option<String>,
        /// Confirm a weakening interactively on a human terminal.
        #[arg(long, requires = "file")]
        authorize_weakening: bool,
    },
    /// Run named checks in a captured private source workspace.
    Verify {
        #[command(flatten)]
        task: TaskRef,
        #[arg(long = "check")]
        checks: Vec<String>,
    },
    /// Record a source-bound review; any finding blocks completion.
    Review {
        #[command(flatten)]
        task: TaskRef,
        #[arg(long = "finding")]
        finding: Vec<String>,
    },
    /// Declare completion only when fresh evidence satisfies the contract.
    Finish(TaskRef),
    /// Recommend a legal action; classification can rank but cannot waive gates.
    Route(TaskRef),
    /// Replay recorded policy inputs without running checks, tools, or models.
    Replay(TaskRef),
    /// Run an explicit controlled evaluation suite in disposable containers.
    Evaluate {
        suite: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Stop a task and its automatic correction lineage.
    Cancel(TaskRef),
    /// Recover an interrupted verification without inventing a passing receipt.
    Recover(TaskRef),
    /// Inspect the existing Claude session packet.
    Show {
        #[arg(long)]
        session: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Reset the existing Claude session packet (does not erase completion evidence).
    Reset {
        #[arg(long)]
        session: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

pub(crate) fn run(command: TaskCmd) -> Result<(), String> {
    match command {
        TaskCmd::Begin {
            objective,
            path,
            provider,
            session,
            contract,
            request_id,
            json,
        } => {
            let root = crate::discover_root(&path)?;
            let contract = if let Some(file) = contract {
                let contract = crate::task_config::from_file(&root, &file)?;
                if contract.objective != objective {
                    return Err("contract objective differs from the requested objective".into());
                }
                contract
            } else {
                crate::task_config::initial(&root, &objective)?
            };
            let store = Store::open(&root).map_err(error)?;
            let task = store
                .begin(
                    contract,
                    &provider,
                    session.as_deref(),
                    &request_id.unwrap_or_else(request),
                )
                .map_err(error)?;
            bind_invocation(&task);
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
            output(&task, json)
        }
        TaskCmd::Prepare(args) => {
            let (root, store, task) = open(&args)?;
            let task = crate::task_prepare::prepare(&root, &store, &task, &request_for(&args))?;
            output(&task, args.json)
        }
        TaskCmd::Status(args) => {
            let (_, store, task) = open(&args)?;
            let edit = store.decision(&task.task_id, Gate::Edit).map_err(error)?;
            let finish = store.decision(&task.task_id, Gate::Finish).map_err(error)?;
            crate::print_data(
                &json!({"schema_version":2,"task":task,"edit":edit,"finish":finish}),
                args.json,
            )
        }
        TaskCmd::Events(args) => {
            let (_, store, task) = open(&args)?;
            let events = store.events(&task.task_id).map_err(error)?;
            let telemetry = crate::task_bridge::telemetry(&events)?;
            let summary = pixel_task::replay::summarize_trajectory(&telemetry).map_err(error)?;
            crate::print_data(
                &json!({"schema_version":2,"task_id":task.task_id,"events":events,"trajectory":summary}),
                args.json,
            )
        }
        TaskCmd::Contract {
            task: args,
            file,
            definition,
            authorize_weakening,
        } => {
            let (root, store, task) = open(&args)?;
            let contract = match (file, definition) {
                (Some(file), _) => crate::task_config::from_file(&root, &file)?,
                (_, Some(definition)) => crate::task_config::from_definition(&root, &definition)?,
                _ => return output(&task.contract, args.json),
            };
            let human_authorized = authorize_weakening && approve_revision(&task.task_id)?;
            let task = store
                .update(
                    &task.task_id,
                    task.revision,
                    &request_for(&args),
                    Action::SetContract {
                        contract,
                        human_authorized,
                    },
                )
                .map_err(error)?;
            output(&task, args.json)
        }
        TaskCmd::Verify { task: args, checks } => {
            let (root, store, task) = open(&args)?;
            let selected: Vec<_> = task
                .contract
                .checks
                .iter()
                .filter(|check| checks.is_empty() || checks.contains(&check.id))
                .cloned()
                .collect();
            let context = crate::task_verify::gather(&root, &selected).map_err(error)?;
            let task = store
                .verify_with_context(&task.task_id, &checks, &request_for(&args), Some(&context))
                .map_err(error)?;
            let failed = task
                .receipts
                .iter()
                .rev()
                .take(if checks.is_empty() {
                    task.contract.checks.len()
                } else {
                    checks.len()
                })
                .any(|receipt| receipt.outcome != pixel_task::CheckOutcome::Passed);
            output(&task, args.json)?;
            if failed {
                Err("one or more verification checks did not pass".into())
            } else {
                Ok(())
            }
        }
        TaskCmd::Review {
            task: args,
            finding,
        } => {
            let (root, store, task) = open(&args)?;
            let task =
                crate::task_prepare::review(&root, &store, &task, finding, &request_for(&args))?;
            output(&task, args.json)
        }
        TaskCmd::Finish(task) => mutate(&task, Action::Finish),
        TaskCmd::Cancel(task) => mutate(&task, Action::Cancel),
        TaskCmd::Recover(task) => mutate(&task, Action::Recover),
        TaskCmd::Route(args) => {
            let (root, store, task) = open(&args)?;
            let advice = crate::task_route::route(&root, &store, &task, &request_for(&args))?;
            crate::print_data(&advice, args.json)
        }
        TaskCmd::Replay(args) => {
            // Deliberately do not reconcile config, capture source, or call a runner on replay.
            let root = crate::discover_root(&args.path)?;
            let store = Store::open(&root).map_err(error)?;
            let events = store.events(&args.task_id).map_err(error)?;
            let frames = crate::task_bridge::replay_frames(&events)?;
            output(
                &pixel_task::replay::replay_policy(&frames).map_err(error)?,
                args.json,
            )
        }
        TaskCmd::Evaluate { suite, json } => {
            let suite = std::fs::read(&suite).map_err(error)?;
            let suite = serde_json::from_slice(&suite).map_err(error)?;
            let report = pixel_task::evaluation::evaluate(&suite).map_err(error)?;
            output(&report, json)?;
            if report.all_passed {
                Ok(())
            } else {
                Err("controlled evaluation did not pass every quality and interaction gate".into())
            }
        }
        TaskCmd::Show {
            session,
            path,
            json,
        } => crate::print_data(
            &crate::task_runtime::show(&path, &session)?.unwrap_or(Value::Null),
            json,
        ),
        TaskCmd::Reset {
            session,
            path,
            json,
        } => crate::print_data(
            &json!({"session_id":session,"reset":crate::task_runtime::reset(&path, &session)?}),
            json,
        ),
    }
}

fn open(args: &TaskRef) -> Result<(PathBuf, Store, Task), String> {
    let root = crate::discover_root(&args.path)?;
    let store = Store::open(&root).map_err(error)?;
    let task = store.status(&args.task_id).map_err(error)?;
    if args
        .expected_revision
        .is_some_and(|revision| revision != task.revision)
    {
        return Err(format!("task revision conflict: actual {}", task.revision));
    }
    let task = reconcile(&root, &store, task)?;
    bind_invocation(&task);
    Ok((root, store, task))
}

pub(crate) fn reconcile(root: &Path, store: &Store, task: Task) -> Result<Task, String> {
    let contract = crate::task_config::reconcile(root, &task.contract)?;
    if contract == task.contract {
        return Ok(task);
    }
    store
        .update(
            &task.task_id,
            task.revision,
            &format!("repo-contract-{}", contract.id().map_err(error)?),
            Action::SetContract {
                contract,
                human_authorized: false,
            },
        )
        .map_err(error)
}

fn mutate(args: &TaskRef, action: Action) -> Result<(), String> {
    let (_, store, task) = open(args)?;
    let task = store
        .update(&task.task_id, task.revision, &request_for(args), action)
        .map_err(error)?;
    output(&task, args.json)
}

fn approve_revision(task_id: &str) -> Result<bool, String> {
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Err("weakening requires an interactive human terminal; ordinary agent contract revisions may only strengthen requirements".into());
    }
    eprint!("Authorize removing or weakening requirements for task {task_id}? Type the task ID: ");
    std::io::stderr().flush().map_err(error)?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).map_err(error)?;
    if answer.trim() != task_id {
        return Err("contract revision not authorized".into());
    }
    Ok(true)
}

fn request_for(args: &TaskRef) -> String {
    args.request_id.clone().unwrap_or_else(request)
}
pub(crate) fn request() -> String {
    format!(
        "cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}
// Diagnostic conversion only; authorization and outcomes are decided by callers.
#[cfg_attr(test, mutants::skip)]
pub(crate) fn error(error: impl std::fmt::Display) -> String {
    error.to_string()
}
fn output(value: &impl serde::Serialize, json: bool) -> Result<(), String> {
    crate::print_data(&serde_json::to_value(value).map_err(error)?, json)
}

pub(crate) fn observe(
    store: &Store,
    task: &Task,
    id: &str,
    kind: &str,
    data: Value,
) -> Result<(), String> {
    if let Some(existing) = store
        .events(&task.task_id)
        .map_err(error)?
        .into_iter()
        .find(|event| event.id == id)
    {
        if existing.data["kind"] != kind || existing.data["data"] != data {
            return Err("conflicting duplicate task observation".into());
        }
        return Ok(());
    }
    let action = Action::Observe {
        event: TrajectoryEvent {
            id: id.into(),
            kind: kind.into(),
            attempt_id: task.attempt_id.clone(),
            occurred_ms: pixel_task::now_ms(),
            data,
        },
    };
    for _ in 0..3 {
        let current = store.status(&task.task_id).map_err(error)?;
        match store.update(&task.task_id, current.revision, id, action.clone()) {
            Ok(_) => return Ok(()),
            Err(pixel_task::Error::Conflict { .. }) => (),
            Err(failure) => return Err(error(failure)),
        }
    }
    Err("task changed repeatedly while recording observation; retry the same event".into())
}

pub(crate) fn handle_hook(
    root: &Path,
    provider: &str,
    event: &str,
    payload: &Value,
) -> Result<Value, String> {
    crate::task_bridge::handle_hook(root, provider, event, payload)
}

fn bind_invocation(task: &Task) {
    ACTIVE_CORRELATION.set(Some(pixel_actionlog::TaskCorrelation {
        task_id: task.task_id.clone(),
        attempt_id: task.attempt_id.clone(),
        span_id: std::env::var("PIXEL_TASK_SPAN_ID").unwrap_or_else(|_| "root".into()),
        parent_span_id: std::env::var("PIXEL_TASK_PARENT_SPAN_ID").ok(),
        host_call_id: std::env::var("PIXEL_TASK_HOST_CALL_ID").ok(),
    }));
}

pub(crate) fn action_correlation(root: &Path) -> Option<pixel_actionlog::TaskCorrelation> {
    if let Some(correlation) = ACTIVE_CORRELATION.with_borrow(Clone::clone) {
        return Some(correlation);
    }
    let id = std::env::var("PIXEL_TASK_ID").ok()?;
    let task = Store::open(root).ok()?.status(&id).ok()?;
    bind_invocation(&task);
    ACTIVE_CORRELATION.with_borrow(Clone::clone)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn observations_should_reject_changed_kind_or_data_independently() {
        let root = Scratch(std::env::temp_dir().join(format!("pixel-task-observe-{}", request())));
        std::fs::create_dir(&root.0).unwrap();
        let store = Store::open(&root.0).unwrap();
        let contract = serde_json::from_value(json!({"objective":"observe a task"})).unwrap();
        let task = store.begin(contract, "pi", None, "begin").unwrap();
        observe(
            &store,
            &task,
            "observation",
            "original-kind",
            json!({"value":1}),
        )
        .unwrap();
        let events = store.events(&task.task_id).unwrap();
        let current = store.status(&task.task_id).unwrap();
        observe(
            &store,
            &task,
            "observation",
            "original-kind",
            json!({"value":1}),
        )
        .unwrap();
        assert_eq!(store.events(&task.task_id).unwrap(), events);
        for (kind, data) in [
            ("different-kind", json!({"value":1})),
            ("original-kind", json!({"value":2})),
        ] {
            assert_eq!(
                observe(&store, &task, "observation", kind, data),
                Err("conflicting duplicate task observation".into())
            );
            assert_eq!(store.events(&task.task_id).unwrap(), events);
            assert_eq!(store.status(&task.task_id).unwrap(), current);
        }
    }
}
