// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! End-to-end contract, private verification and native lifecycle behavior.

use std::io::Write;
use std::path::Path;
use std::process::{Output, Stdio};

use serde_json::{Value, json};

use super::support::{Scratch, git, pixel_command};

fn repo(tag: &str) -> Scratch {
    let root = Scratch::for_test("task-cli", tag);
    git(&root, &["init", "-q"]);
    std::fs::write(root.join("source.txt"), "correct\n").unwrap();
    std::fs::write(root.join(".gitignore"), ".pixel/\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "initial"]);
    std::fs::create_dir(root.join(".pixel")).unwrap();
    // These tests exercise the enforcing gate; the product default is
    // advisory, so the fixture opts in explicitly.
    std::fs::write(
        root.join(".pixel/config.json"),
        json!({"task":{"enforcement":"enforce"}}).to_string(),
    )
    .unwrap();
    root
}

fn contract() -> Value {
    json!({"version":1,"objective":"fix source","checks":[{"id":"content","argv":["/bin/sh","-c","test \"$(cat source.txt)\" = correct"],"timeout_ms":5000}],
        "criteria":[{"id":"task-acceptance","description":"fix source","checks":["content"]}],"conservative_checks":["content"]})
}

fn command(root: &Path, args: &[&str]) -> Output {
    pixel_command()
        .current_dir(root)
        .env("PIXEL_METRICS", "0")
        .env("PIXEL_TASK_POLICY", "gates")
        .env_remove("PIXEL_TASK_CONTRACT")
        .env_remove("PIXEL_TASK_TELEMETRY_PATH")
        .env_remove("PIXEL_TASK_ID")
        .env_remove("PIXEL_TASK_SPAN_ID")
        .env_remove("PIXEL_TASK_PARENT_SPAN_ID")
        .env_remove("PIXEL_TASK_HOST_CALL_ID")
        .args(args)
        .output()
        .unwrap()
}

fn good(root: &Path, args: &[&str]) -> Value {
    let output = command(root, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{args:?}: {error}: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn begin(root: &Path) -> String {
    std::fs::write(root.join(".pixel/contract.json"), contract().to_string()).unwrap();
    good(
        root,
        &[
            "task",
            "begin",
            "fix source",
            "--contract",
            ".pixel/contract.json",
            "--request-id",
            "begin",
            "--json",
        ],
    )["task_id"]
        .as_str()
        .unwrap()
        .to_string()
}

fn hook(root: &Path, provider: &str, event: &str, payload: Value) -> Value {
    hook_with_policy(root, provider, event, payload, "gates")
}

fn hook_with_policy(
    root: &Path,
    provider: &str,
    event: &str,
    mut payload: Value,
    policy: &str,
) -> Value {
    payload["cwd"] = json!(root);
    let mut child = pixel_command()
        .current_dir(root)
        .env("PIXEL_METRICS", "0")
        .env("PIXEL_TASK_POLICY", policy)
        .env_remove("PIXEL_TASK_CONTRACT")
        .env_remove("PIXEL_TASK_TELEMETRY_PATH")
        .args([
            "run-hook",
            "task-event",
            "--provider",
            provider,
            "--event",
            event,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn prepared_session(root: &Path, provider: &str, session: &str) -> pixel_task::Task {
    let store = pixel_task::Store::open(root).unwrap();
    let task = store
        .begin(
            serde_json::from_value(contract()).unwrap(),
            provider,
            Some(session),
            "begin-prepared-session",
        )
        .unwrap();
    good(root, &["task", "prepare", &task.task_id, "--json"]);
    store.status(&task.task_id).unwrap()
}

#[test]
fn fresh_task_enforcement_requires_both_repository_enablement_and_a_gated_policy() {
    for (enabled, policy) in [
        (true, "gates"),
        (false, "gates"),
        (true, "retrieval"),
        (false, "retrieval"),
    ] {
        let root = repo(&format!("enforcement-{enabled}-{policy}"));
        if !enabled {
            std::fs::write(
                root.join(".pixel/config.json"),
                json!({"task":{"enforcement":"off"}}).to_string(),
            )
            .unwrap();
        }
        let result = hook_with_policy(
            &root,
            "pi",
            "pre-tool-use",
            json!({"session_id":"s","tool_name":"Write","tool_use_id":"write"}),
            policy,
        );
        let enforced = enabled && policy == "gates";
        assert_eq!(
            result["decision"],
            if enforced { "deny" } else { "observe" }
        );
        let store = pixel_task::Store::open(&root).unwrap();
        let task = store.find_session("pi", "s").unwrap().unwrap();
        let policies: Vec<_> = store
            .events(&task.task_id)
            .unwrap()
            .into_iter()
            .filter(|event| event.data["kind"] == "host_policy")
            .map(|event| event.data["data"]["enforce"].clone())
            .collect();
        assert_eq!(policies, if enforced { vec![json!(true)] } else { vec![] });
    }
}

#[test]
fn explicit_binding_requires_provider_and_session_independently() {
    for (bound_provider, requested_session) in [("codex", "owner"), ("pi", "other")] {
        let root = repo(&format!("binding-{bound_provider}"));
        let task = prepared_session(&root, bound_provider, "owner");
        let store = pixel_task::Store::open(&root).unwrap();
        let before = store.events(&task.task_id).unwrap();
        let result = hook(
            &root,
            "pi",
            "pre-tool-use",
            json!({"session_id":requested_session,"task_id":task.task_id,
                "attempt_id":task.attempt_id,"tool_name":"Write","tool_use_id":"foreign"}),
        );
        assert_eq!(result["decision"], "deny");
        assert_eq!(store.events(&task.task_id).unwrap(), before);
        assert_eq!(store.status(&task.task_id).unwrap(), task);
    }
}

#[test]
fn unbound_pi_branch_must_not_reuse_an_otherwise_editable_session_task() {
    let root = repo("unbound-editable");
    let task = prepared_session(&root, "pi", "s");
    let store = pixel_task::Store::open(&root).unwrap();
    let before = store.events(&task.task_id).unwrap();
    let result = hook(
        &root,
        "pi",
        "pre-tool-use",
        json!({"session_id":"s","branch_unbound":true,"tool_name":"Write","tool_use_id":"unbound"}),
    );
    assert_eq!(result["decision"], "deny");
    assert_eq!(store.events(&task.task_id).unwrap(), before);
    assert_eq!(store.status(&task.task_id).unwrap(), task);
    let bound = hook(
        &root,
        "pi",
        "pre-tool-use",
        json!({"session_id":"s","task_id":task.task_id,"attempt_id":task.attempt_id,
            "tool_name":"Write","tool_use_id":"bound"}),
    );
    assert_eq!(bound["decision"], "allow");
}

#[test]
fn coding_prompt_reuses_active_tasks_but_replaces_terminal_tasks() {
    let root = repo("prompt-after-terminal");
    let first = hook(
        &root,
        "pi",
        "prompt-submit",
        json!({"session_id":"s","event_id":"first","prompt":"fix source"}),
    );
    let id = first["task_id"].as_str().unwrap();
    let active = hook(
        &root,
        "pi",
        "prompt-submit",
        json!({"session_id":"s","event_id":"active","prompt":"fix another file"}),
    );
    assert_eq!(active["task_id"], id);
    good(&root, &["task", "cancel", id, "--json"]);
    let question = hook(
        &root,
        "pi",
        "prompt-submit",
        json!({"session_id":"s","event_id":"question","prompt":"How does this work?"}),
    );
    assert_eq!(question["task_id"], id);
    assert_eq!(question["phase"], "cancelled");
    let next = hook(
        &root,
        "pi",
        "prompt-submit",
        json!({"session_id":"s","event_id":"next","prompt":"fix another file"}),
    );
    let next_id = next["task_id"].as_str().unwrap();
    assert_ne!(next_id, id);
    let store = pixel_task::Store::open(&root).unwrap();
    assert_eq!(
        store.status(id).unwrap().phase,
        pixel_task::Phase::Cancelled
    );
    assert_eq!(
        store.status(next_id).unwrap().contract.objective,
        "fix another file"
    );
    assert_eq!(
        store.find_session("pi", "s").unwrap().unwrap().task_id,
        next_id
    );
}

#[test]
fn only_mutating_tool_outcomes_invalidate_live_preparation() {
    for (event, tool, terminal) in [
        ("post-tool-use", "Read", false),
        ("session-start", "Read", false),
        ("post-tool-use", "Write", true),
    ] {
        let root = repo(&format!("preserve-preparation-{event}-{tool}-{terminal}"));
        let mut task = prepared_session(&root, "pi", "s");
        let store = pixel_task::Store::open(&root).unwrap();
        if terminal {
            good(&root, &["task", "cancel", &task.task_id, "--json"]);
            task = store.status(&task.task_id).unwrap();
        }
        std::fs::write(root.join("source.txt"), "external edit\n").unwrap();
        let result = hook(
            &root,
            "pi",
            event,
            json!({"session_id":"s","event_id":"outcome","tool_name":tool,"tool_use_id":"tool"}),
        );
        assert_eq!(result["task_id"], task.task_id);
        assert_eq!(result["phase"], serde_json::to_value(task.phase).unwrap());
        let after = store.status(&task.task_id).unwrap();
        assert_eq!(after.phase, task.phase);
        assert_eq!(after.source, task.source);
        assert_eq!(after.observations, task.observations);
    }
}

#[test]
fn explicit_cancellation_on_stop_prevents_automatic_continuation() {
    let root = repo("stop-cancelled");
    let started = hook(
        &root,
        "pi",
        "prompt-submit",
        json!({"session_id":"s","event_id":"prompt","prompt":"fix source"}),
    );
    let result = hook(
        &root,
        "pi",
        "stop",
        json!({"session_id":"s","event_id":"stop","cancelled":true}),
    );
    assert_eq!(result["decision"], "allow");
    assert_eq!(result["phase"], "cancelled");
    assert_eq!(
        result["reason"],
        "cancelled; automatic continuation stopped"
    );
    let store = pixel_task::Store::open(&root).unwrap();
    let task = store.status(started["task_id"].as_str().unwrap()).unwrap();
    assert_eq!(task.phase, pixel_task::Phase::Cancelled);
    assert_eq!(task.budget.continuations, 0);
}

#[test]
fn mutation_without_repository_checks_returns_the_existing_contract_gate() {
    let root = repo("unconfigured-preparation");
    let task = hook(
        &root,
        "pi",
        "prompt-submit",
        json!({"session_id":"s","event_id":"prompt","prompt":"fix source"}),
    );
    let result = hook(
        &root,
        "pi",
        "pre-tool-use",
        json!({"session_id":"s","tool_name":"Write","tool_use_id":"edit"}),
    );
    assert_eq!(result["decision"], "deny");
    assert_eq!(result["task_id"], task["task_id"]);
    assert!(
        result["reason"]
            .as_str()
            .unwrap()
            .contains("repository verification checks are not configured")
    );
    let store = pixel_task::Store::open(&root).unwrap();
    let task = store.status(task["task_id"].as_str().unwrap()).unwrap();
    assert!(task.contract.checks.is_empty());
    assert_eq!(task.source, None);
    assert!(task.observations.is_empty());
}

#[test]
fn task_mutation_should_require_the_exact_expected_revision() {
    let root = repo("expected-revision");
    let id = begin(&root);
    let store = pixel_task::Store::open(&root).unwrap();
    let before = store.status(&id).unwrap();
    let wrong = (before.revision + 1).to_string();
    let rejected = command(
        &root,
        &[
            "task",
            "cancel",
            &id,
            "--expected-revision",
            &wrong,
            "--json",
        ],
    );
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("task revision conflict"));
    assert_eq!(store.status(&id).unwrap(), before);
    let accepted = good(
        &root,
        &[
            "task",
            "cancel",
            &id,
            "--expected-revision",
            &before.revision.to_string(),
            "--json",
        ],
    );
    assert_eq!(accepted["phase"], "cancelled");
    assert_eq!(accepted["revision"], before.revision + 1);
}

#[test]
fn task_should_verify_and_finish_an_unborn_repository() {
    let root = Scratch::for_test("task-cli", "unborn");
    git(&root, &["init", "-q"]);
    std::fs::write(root.join("source.txt"), "correct\n").unwrap();
    std::fs::write(root.join(".gitignore"), ".pixel/\n").unwrap();
    git(&root, &["add", "."]);
    std::fs::create_dir(root.join(".pixel")).unwrap();
    let id = begin(&root);
    good(&root, &["task", "prepare", &id, "--json"]);
    good(&root, &["task", "verify", &id, "--json"]);
    let review = good(&root, &["task", "review", &id, "--json"]);
    assert_eq!(review["review"]["passed"], true);
    assert_eq!(review["review"]["findings"], json!([]));
    assert_eq!(
        good(&root, &["task", "finish", &id, "--json"])["phase"],
        "complete"
    );
    assert!(
        pixel_task::snapshot::capture(&root, &serde_json::from_value(contract()).unwrap(), true)
            .unwrap()
            .head
            .is_none()
    );
}

#[test]
fn review_should_reject_staged_and_unstaged_whitespace_errors_independently() {
    for staged in [false, true] {
        let root = repo(if staged {
            "staged-whitespace"
        } else {
            "unstaged-whitespace"
        });
        git(&root, &["config", "core.whitespace", "blank-at-eol"]);
        std::fs::write(root.join("source.txt"), "correct \n").unwrap();
        if staged {
            git(&root, &["add", "source.txt"]);
        }
        let id = begin(&root);
        good(&root, &["task", "prepare", &id, "--json"]);
        let reviewed = good(&root, &["task", "review", &id, "--json"]);
        assert_eq!(reviewed["review"]["passed"], false, "staged={staged}");
        assert_eq!(
            reviewed["review"]["findings"],
            json!(["git diff --check failed"]),
            "staged={staged}"
        );
    }
}

#[test]
fn read_only_prompts_should_stay_free_and_mutation_fallback_should_retain_the_objective() {
    let root = repo("intent");
    for (session, prompt) in [
        ("question", "How does this code implement its cache?"),
        ("request", "Please make the result deterministic"),
    ] {
        hook(
            &root,
            "pi",
            "prompt-submit",
            json!({"session_id":session,"prompt":prompt,"event_id":"prompt"}),
        );
        assert_eq!(
            hook(
                &root,
                "pi",
                "stop",
                json!({"session_id":session,"event_id":"read-only-stop"})
            )["decision"],
            "observe"
        );
        assert!(
            pixel_task::Store::open(&root)
                .unwrap()
                .find_session("pi", session)
                .unwrap()
                .is_none()
        );
    }
    let result = hook(
        &root,
        "pi",
        "pre-tool-use",
        json!({"session_id":"request","tool_name":"Write","tool_use_id":"first-edit"}),
    );
    assert_eq!(result["decision"], "deny");
    let task = pixel_task::Store::open(&root)
        .unwrap()
        .find_session("pi", "request")
        .unwrap()
        .unwrap();
    assert_eq!(
        task.contract.objective,
        "Please make the result deterministic"
    );
    assert_eq!(
        task.contract.criteria[0].description,
        task.contract.objective
    );
}

#[test]
fn enforced_tasks_should_reject_runtime_policy_downgrades() {
    let root = repo("sticky-policy");
    let payload = json!({"session_id":"s","tool_name":"Write","tool_use_id":"first-edit"});
    assert_eq!(
        hook(&root, "pi", "pre-tool-use", payload)["decision"],
        "deny"
    );
    std::fs::write(
        root.join(".pixel/config.json"),
        json!({"task":{"enforcement":"off"}}).to_string(),
    )
    .unwrap();
    assert_eq!(
        hook_with_policy(
            &root,
            "pi",
            "pre-tool-use",
            json!({"session_id":"s","tool_name":"Write","tool_use_id":"second-edit"}),
            "retrieval"
        )["decision"],
        "deny"
    );
    assert_eq!(
        hook_with_policy(
            &root,
            "pi",
            "stop",
            json!({"session_id":"s","event_id":"stop"}),
            "retrieval"
        )["decision"],
        "continue"
    );
    // Disabled policy still applies to a new task, as required for the baseline arm.
    assert_eq!(
        hook_with_policy(
            &root,
            "pi",
            "pre-tool-use",
            json!({"session_id":"baseline","tool_name":"Write","tool_use_id":"first"}),
            "retrieval"
        )["decision"],
        "observe"
    );
    assert_eq!(
        hook_with_policy(
            &root,
            "pi",
            "pre-tool-use",
            json!({"session_id":"baseline","tool_name":"Write","tool_use_id":"second"}),
            "retrieval"
        )["decision"],
        "observe"
    );
}

#[cfg(unix)]
#[test]
fn weakening_should_require_both_terminal_streams_and_the_exact_confirmation() {
    use std::fs::File;
    use std::os::fd::FromRawFd;

    for (input_terminal, output_terminal, correct_answer) in [
        (false, false, true),
        (true, false, true),
        (false, true, true),
        (true, true, true),
        (true, true, false),
    ] {
        let root = repo(&format!(
            "waiver-{input_terminal}-{output_terminal}-{correct_answer}"
        ));
        let id = begin(&root);
        let mut weaker = contract();
        weaker["checks"][0]["argv"] = json!(["/usr/bin/true"]);
        std::fs::write(root.join(".pixel/weaker.json"), weaker.to_string()).unwrap();
        let mut master = -1;
        let mut slave = -1;
        assert_eq!(
            // SAFETY: openpty initializes two owned descriptors; optional attributes are null.
            unsafe {
                libc::openpty(
                    &raw mut master,
                    &raw mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        // SAFETY: each successful openpty descriptor is adopted exactly once.
        let (mut master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
        let mut child = pixel_command()
            .current_dir(&root)
            .env("PIXEL_METRICS", "0")
            .args([
                "task",
                "contract",
                &id,
                "--file",
                ".pixel/weaker.json",
                "--authorize-weakening",
                "--json",
            ])
            .stdin(if input_terminal {
                Stdio::from(slave.try_clone().unwrap())
            } else {
                Stdio::piped()
            })
            .stderr(if output_terminal {
                Stdio::from(slave)
            } else {
                Stdio::piped()
            })
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let answer = if correct_answer {
            id.as_str()
        } else {
            "wrong-task"
        };
        if input_terminal {
            writeln!(master, "{answer}").unwrap();
        } else {
            let _ = writeln!(child.stdin.take().unwrap(), "{answer}");
        }
        let output = child.wait_with_output().unwrap();
        let authorized = input_terminal && output_terminal && correct_answer;
        assert_eq!(
            output.status.success(),
            authorized,
            "terminal tuple {input_terminal}/{output_terminal}/{correct_answer}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if authorized {
            let result: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(
                result["contract"]["checks"][0]["argv"],
                json!(["/usr/bin/true"])
            );
        } else if !input_terminal && !output_terminal {
            assert!(String::from_utf8_lossy(&output.stderr).contains("interactive human terminal"));
        }
    }
}

#[test]
fn native_stop_should_publish_only_verified_completion_and_preserve_live_observation() {
    let root = repo("verified-stop");
    std::fs::write(root.join(".pixel/contract.json"), contract().to_string()).unwrap();
    let task = good(
        &root,
        &[
            "task",
            "begin",
            "fix source",
            "--contract",
            ".pixel/contract.json",
            "--provider",
            "pi",
            "--session",
            "s",
            "--json",
        ],
    );
    let id = task["task_id"].as_str().unwrap();
    good(&root, &["task", "prepare", id, "--json"]);
    good(&root, &["task", "verify", id, "--json"]);
    good(&root, &["task", "review", id, "--json"]);
    let finished = hook(
        &root,
        "pi",
        "stop",
        json!({"session_id":"s","event_id":"verified"}),
    );
    assert_eq!(finished["decision"], "allow");
    assert_eq!(finished["phase"], "complete");
    assert_eq!(finished["reason"], "verified complete");
    let marker: Value = serde_json::from_slice(
        &std::fs::read(root.join(".pixel/task-hook-observations.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(marker["pi"]["session_id"], "s");
    assert_eq!(marker["pi"]["event"], "stop");
    assert!(marker["pi"]["observed_unix"].as_u64().unwrap() > 0);
    let events = good(&root, &["task", "events", id, "--json"]);
    assert_eq!(events["trajectory"]["coordinator_calls"], 1);
    let repeated = hook(
        &root,
        "pi",
        "stop",
        json!({"session_id":"s","event_id":"verified"}),
    );
    assert_eq!(repeated["decision"], "allow");
    assert_eq!(
        good(&root, &["task", "events", id, "--json"])["trajectory"]["coordinator_calls"],
        1
    );
    let store = pixel_task::Store::open(&root).unwrap();
    let complete = store.status(id).unwrap();
    std::fs::write(root.join("source.txt"), "external edit after completion\n").unwrap();
    let edit = hook(
        &root,
        "pi",
        "pre-tool-use",
        json!({"session_id":"s","tool_name":"Write","tool_use_id":"after-completion"}),
    );
    assert_eq!(edit["decision"], "deny");
    assert_eq!(edit["task_id"], id);
    assert_eq!(edit["phase"], "complete");
    assert!(
        edit["reason"]
            .as_str()
            .unwrap()
            .contains("completed task needs a new task binding")
    );
    let after = store.status(id).unwrap();
    assert_eq!(after.source, complete.source);
    assert_eq!(after.observations, complete.observations);
}

#[test]
fn native_outcomes_should_preserve_failures_without_inventing_unknown_success() {
    use pixel_task::replay::{Observation, TelemetryEvent, ToolOutcome};

    let root = repo("outcomes");
    hook(
        &root,
        "pi",
        "prompt-submit",
        json!({"session_id":"s","event_id":"prompt","prompt":"fix source"}),
    );
    for (id, event, result) in [
        ("success", "post-tool-use", json!({"success":true})),
        ("failed", "post-tool-use", json!({"success":false})),
        ("failure-event", "tool-failure", json!({})),
        ("unknown", "post-tool-use", json!({})),
    ] {
        hook(
            &root,
            "pi",
            "pre-tool-use",
            json!({"session_id":"s","tool_name":"Read","tool_use_id":id}),
        );
        let mut payload = json!({"session_id":"s","tool_name":"Read","tool_use_id":id});
        payload
            .as_object_mut()
            .unwrap()
            .extend(result.as_object().unwrap().clone());
        hook(&root, "pi", event, payload);
    }
    let store = pixel_task::Store::open(&root).unwrap();
    let task = store.find_session("pi", "s").unwrap().unwrap();
    let outcomes: std::collections::BTreeMap<_, _> = store
        .events(&task.task_id)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "observation" && event.data["kind"] == "telemetry")
        .filter_map(|event| {
            let telemetry: TelemetryEvent =
                serde_json::from_value(event.data["data"].clone()).unwrap();
            match telemetry.observation {
                Observation::ToolFinished {
                    request_id,
                    outcome,
                    ..
                } => Some((request_id, outcome)),
                _ => None,
            }
        })
        .collect();
    assert_eq!(
        outcomes,
        std::collections::BTreeMap::from([
            ("success".into(), ToolOutcome::Succeeded),
            ("failed".into(), ToolOutcome::Failed),
            ("failure-event".into(), ToolOutcome::Failed)
        ])
    );
}

#[test]
fn repository_toolchain_pins_should_survive_every_contract_entry_path() {
    let root = repo("toolchain");
    let pins = json!({"compiler":"a".repeat(64)});
    std::fs::write(
        root.join(".pixel/config.json"),
        json!({"task":{"toolchain":pins}}).to_string(),
    )
    .unwrap();
    let initial = good(&root, &["task", "begin", "fix source", "--json"]);
    assert_eq!(initial["contract"]["toolchain"], pins);
    let from_file = begin(&root);
    assert_eq!(
        good(&root, &["task", "status", &from_file, "--json"])["task"]["contract"]["toolchain"],
        pins
    );
    let extended = json!({"compiler":"a".repeat(64), "driver":"b".repeat(64)});
    std::fs::write(
        root.join(".pixel/config.json"),
        json!({"task":{"toolchain":extended}}).to_string(),
    )
    .unwrap();
    assert_eq!(
        good(&root, &["task", "status", &from_file, "--json"])["task"]["contract"]["toolchain"],
        extended
    );
    let mut replacement = contract();
    replacement["toolchain"] = json!({"compiler":"c".repeat(64)});
    std::fs::write(root.join(".pixel/contract.json"), replacement.to_string()).unwrap();
    assert!(
        !command(
            &root,
            &[
                "task",
                "contract",
                &from_file,
                "--file",
                ".pixel/contract.json",
                "--json"
            ]
        )
        .status
        .success()
    );
}

#[test]
fn unconfigured_task_should_accept_an_inline_contract_without_authorizing_a_file_write() {
    let root = repo("inline-contract");
    let task = good(
        &root,
        &[
            "task",
            "begin",
            "fix source",
            "--provider",
            "pi",
            "--session",
            "s",
            "--json",
        ],
    );
    let id = task["task_id"].as_str().unwrap();
    let definition = contract().to_string();
    let denied = hook(
        &root,
        "pi",
        "pre-tool-use",
        json!({"session_id":"s","tool_name":"Write","tool_use_id":"write"}),
    );
    assert_eq!(denied["decision"], "deny");
    let reason = denied["reason"].as_str().unwrap();
    assert!(reason.contains(id));
    assert!(reason.contains("--definition '<JSON>'"));
    let declaration = format!("pixel task contract {id} --definition '{definition}' --json");
    assert_ne!(
        hook(
            &root,
            "pi",
            "pre-tool-use",
            json!({"session_id":"s","tool_name":"bash","tool_use_id":"declare","tool_input":{"command":declaration}})
        )["decision"],
        "deny"
    );
    let strengthened = good(
        &root,
        &[
            "task",
            "contract",
            id,
            "--definition",
            &definition,
            "--json",
        ],
    );
    assert_eq!(
        strengthened["contract"]["checks"],
        json!([{"id":"content","kind":"argv","argv":["/bin/sh","-c","test \"$(cat source.txt)\" = correct"],"cwd":".","timeout_ms":5000,"required":true}])
    );
    assert!(!root.join(".pixel/contract.json").exists());
    assert!(
        !command(
            &root,
            &["task", "contract", id, "--definition", "{", "--json"]
        )
        .status
        .success()
    );
    let mut weaker = contract();
    weaker["checks"] = json!([]);
    weaker["criteria"][0]["checks"] = json!([]);
    weaker["conservative_checks"] = json!([]);
    assert!(
        !command(
            &root,
            &[
                "task",
                "contract",
                id,
                "--definition",
                &weaker.to_string(),
                "--json"
            ]
        )
        .status
        .success()
    );
}

#[test]
fn task_should_require_private_check_and_review_then_reject_same_head_edits() {
    let root = repo("lifecycle");
    let id = begin(&root);
    let task = good(&root, &["task", "status", &id, "--json"]);
    let attempt = task["task"]["attempt_id"].clone();
    let correlated = pixel_command()
        .current_dir(&root)
        .env("PIXEL_METRICS", "0")
        .env_remove("PIXEL_TASK_ID")
        .env("PIXEL_TASK_SPAN_ID", "fixture-span")
        .env("PIXEL_TASK_PARENT_SPAN_ID", "fixture-parent")
        .env("PIXEL_TASK_HOST_CALL_ID", "fixture-call")
        .args(["task", "status", &id, "--json"])
        .output()
        .unwrap();
    assert!(
        correlated.status.success(),
        "{}",
        String::from_utf8_lossy(&correlated.stderr)
    );
    let actions: Vec<Value> = std::fs::read_to_string(root.join(".pixel/actions.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let correlated = actions
        .iter()
        .find(|event| event["task"]["span_id"] == "fixture-span")
        .unwrap();
    assert_eq!(
        correlated["task"],
        json!({"task_id":id,"attempt_id":attempt,"span_id":"fixture-span","parent_span_id":"fixture-parent","host_call_id":"fixture-call"})
    );
    assert!(
        actions
            .iter()
            .any(|event| event["task"]["task_id"] == id && event["task"]["span_id"] == "root")
    );
    let duplicate = good(
        &root,
        &[
            "task",
            "begin",
            "fix source",
            "--contract",
            ".pixel/contract.json",
            "--request-id",
            "begin",
            "--json",
        ],
    );
    assert_eq!(duplicate["task_id"], id);
    assert_eq!(duplicate["revision"], 1);
    assert!(
        !command(&root, &["task", "finish", &id, "--json"])
            .status
            .success()
    );
    let prepared = good(
        &root,
        &["task", "prepare", &id, "--request-id", "prepare", "--json"],
    );
    assert_eq!(prepared["phase"], "prepared");
    let verified = good(
        &root,
        &["task", "verify", &id, "--request-id", "verify", "--json"],
    );
    assert_eq!(verified["receipts"][0]["outcome"], "passed");
    assert_ne!(
        verified["receipts"][0]["execution_root"],
        json!(root.as_ref())
    );
    assert!(
        !command(&root, &["task", "finish", &id, "--json"])
            .status
            .success()
    );
    let reviewed = good(
        &root,
        &["task", "review", &id, "--request-id", "review", "--json"],
    );
    assert_eq!(reviewed["review"]["passed"], true);
    let finished = good(
        &root,
        &["task", "finish", &id, "--request-id", "finish", "--json"],
    );
    assert_eq!(finished["phase"], "complete");
    std::fs::write(root.join("source.txt"), "regression\n").unwrap();
    let status = good(&root, &["task", "status", &id, "--json"]);
    assert_eq!(status["finish"]["allowed"], false);
    assert!(
        !command(&root, &["task", "finish", &id, "--json"])
            .status
            .success()
    );
}

#[test]
fn task_should_reject_failed_check_and_keep_live_source_unchanged() {
    let root = repo("failed");
    let id = begin(&root);
    std::fs::write(root.join("source.txt"), "wrong\n").unwrap();
    good(&root, &["task", "prepare", &id, "--json"]);
    let output = command(&root, &["task", "verify", &id, "--json"]);
    assert!(!output.status.success());
    let verified: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(verified["receipts"][0]["outcome"], "failed");
    assert_eq!(
        std::fs::read_to_string(root.join("source.txt")).unwrap(),
        "wrong\n"
    );
    assert!(
        !command(&root, &["task", "finish", &id, "--json"])
            .status
            .success()
    );
}

#[test]
fn contract_should_map_initial_criteria_preserve_repository_checks_and_reject_noninteractive_waivers()
 {
    let root = repo("contract");
    let requirement = contract()["checks"].clone();
    std::fs::write(
        root.join(".pixel/config.json"),
        json!({"task":{"checks":requirement,"conservative_checks":["content"]}}).to_string(),
    )
    .unwrap();
    let task = good(&root, &["task", "begin", "fix source", "--json"]);
    let id = task["task_id"].as_str().unwrap();
    assert_eq!(task["contract"]["criteria"][0]["checks"], json!([]));
    // Omitted default fields and their serialized forms must be equivalent requirements.
    let status = good(&root, &["task", "status", id, "--json"]);
    assert_eq!(status["task"]["contract"], task["contract"]);
    assert_eq!(
        good(&root, &["task", "status", id, "--json"])["task"]["revision"],
        status["task"]["revision"]
    );
    std::fs::write(root.join(".pixel/contract.json"), contract().to_string()).unwrap();
    let updated = good(
        &root,
        &[
            "task",
            "contract",
            id,
            "--file",
            ".pixel/contract.json",
            "--json",
        ],
    );
    assert_eq!(
        updated["contract"]["criteria"][0]["checks"],
        json!(["content"])
    );
    let mut weakened = contract();
    weakened["checks"][0]["argv"] = json!(["/usr/bin/true"]);
    std::fs::write(root.join(".pixel/contract.json"), weakened.to_string()).unwrap();
    assert!(
        !command(
            &root,
            &[
                "task",
                "contract",
                id,
                "--file",
                ".pixel/contract.json",
                "--json"
            ]
        )
        .status
        .success()
    );
    std::fs::write(root.join(".pixel/contract.json"), contract().to_string()).unwrap();
    assert!(
        !command(
            &root,
            &[
                "task",
                "contract",
                id,
                "--file",
                ".pixel/contract.json",
                "--authorize-weakening",
                "--json"
            ]
        )
        .status
        .success()
    );
}

#[test]
fn native_hosts_should_deduplicate_denied_requests_and_stop_at_the_persistent_budget() {
    for provider in ["claude", "codex", "pi"] {
        let root = repo(&format!("host-{provider}"));
        let payload = json!({"session_id":"session","tool_use_id":"request-1","tool_name":"Write","tool_input":{"file_path":"source.txt","content":"private"}});
        for _ in 0..2 {
            let result = hook(&root, provider, "pre-tool-use", payload.clone());
            if provider == "pi" {
                assert_eq!(result["decision"], "deny");
            } else {
                assert_eq!(result["hookSpecificOutput"]["permissionDecision"], "deny");
            }
        }
        let store = pixel_task::Store::open(&root).unwrap();
        let task = store.find_session(provider, "session").unwrap().unwrap();
        let id = &task.task_id;
        let events = good(&root, &["task", "events", id, "--json"]);
        assert_eq!(events["trajectory"]["observed_model_tool_requests"], 1);
        assert_eq!(events["trajectory"]["blocked_requests"], 1);
        assert_eq!(events["trajectory"]["coverage"], "partial");
        assert_eq!(events["trajectory"]["model_tool_requests"], Value::Null);
        for index in 0..3 {
            let result = hook(
                &root,
                provider,
                "stop",
                json!({"session_id":"session","event_id":format!("stop-{index}")}),
            );
            if index < 2 {
                assert_eq!(
                    result["decision"],
                    if provider == "pi" {
                        "continue"
                    } else {
                        "block"
                    }
                );
            } else if provider == "pi" {
                assert_eq!(result["decision"], "deny");
            } else {
                assert_eq!(result["continue"], false);
                assert!(
                    result["stopReason"]
                        .as_str()
                        .unwrap()
                        .contains("unverified")
                );
            }
        }
        assert_eq!(store.status(id).unwrap().budget.continuations, 2);
        let replay = good(&root, &["task", "replay", id, "--json"]);
        assert_eq!(replay["first_divergence"], Value::Null);
        assert_eq!(replay["counterfactual_outcome_established"], false);
        assert!(replay["frames_evaluated"].as_u64().unwrap() > 0);
    }
}

#[test]
fn first_mutation_should_never_authorize_itself_and_failed_writes_should_invalidate_preparation() {
    let root = repo("fallback");
    std::fs::write(
        root.join(".pixel/config.json"),
        json!({"task":{"enforcement":"enforce","checks":contract()["checks"],"conservative_checks":["content"]}})
            .to_string(),
    )
    .unwrap();
    let mut payload = json!({"session_id":"s","tool_use_id":"one","tool_name":"Write","tool_input":{"file_path":"source.txt"}});
    assert_eq!(
        hook(&root, "pi", "pre-tool-use", payload.clone())["decision"],
        "deny"
    );
    payload["tool_use_id"] = json!("two");
    assert_eq!(
        hook(&root, "pi", "pre-tool-use", payload.clone())["decision"],
        "allow"
    );
    std::fs::write(root.join("source.txt"), "partial write\n").unwrap();
    hook(&root, "pi", "tool-failure", payload);
    let store = pixel_task::Store::open(&root).unwrap();
    let task = store.find_session("pi", "s").unwrap().unwrap();
    assert_eq!(task.phase, pixel_task::Phase::Editing);
    assert_eq!(task.source, None);
    hook(
        &root,
        "pi",
        "interrupt",
        json!({"session_id":"s","event_id":"cancel"}),
    );
    let result = hook(
        &root,
        "pi",
        "pre-tool-use",
        json!({"session_id":"s","tool_use_id":"three","tool_name":"Write"}),
    );
    assert_eq!(result["decision"], "deny");
    assert_eq!(
        store.find_session("pi", "s").unwrap().unwrap().task_id,
        task.task_id
    );
}

#[test]
fn pi_forks_should_keep_budget_and_reject_foreign_or_unbound_tasks() {
    let root = repo("fork");
    let started = hook(
        &root,
        "pi",
        "prompt-submit",
        json!({"session_id":"origin","event_id":"prompt","prompt":"fix source"}),
    );
    let id = started["task_id"].as_str().unwrap();
    let attempt = started["attempt_id"].as_str().unwrap();
    let binding = json!({"session_id":"fork","binding_session_id":"origin","task_id":id,"attempt_id":attempt,"branch_id":"fork-branch","event_id":"stop-1"});
    assert_eq!(
        hook(&root, "pi", "stop", binding.clone())["decision"],
        "continue"
    );
    let mut second = binding;
    second["event_id"] = json!("stop-2");
    assert_eq!(
        hook(&root, "pi", "stop", second.clone())["decision"],
        "continue"
    );
    second["event_id"] = json!("stop-3");
    assert_eq!(hook(&root, "pi", "stop", second)["decision"], "deny");
    let store = pixel_task::Store::open(&root).unwrap();
    assert_eq!(store.status(id).unwrap().budget.continuations, 2);
    let rejected = hook(
        &root,
        "pi",
        "pre-tool-use",
        json!({"session_id":"foreign","task_id":id,"attempt_id":attempt,"tool_name":"Write","tool_use_id":"bad"}),
    );
    assert_eq!(rejected["decision"], "deny");
    let unbound = hook(
        &root,
        "pi",
        "pre-tool-use",
        json!({"session_id":"origin","branch_unbound":true,"tool_name":"Write","tool_use_id":"unbound"}),
    );
    assert_eq!(unbound["decision"], "deny");
    let stale = hook(
        &root,
        "pi",
        "pre-tool-use",
        json!({"session_id":"origin","task_id":id,"attempt_id":"old","tool_name":"Write","tool_use_id":"stale"}),
    );
    assert_eq!(stale["decision"], "deny");
    assert_eq!(stale["attempt_id"], attempt);
}

#[test]
fn task_model_request_and_native_hook_should_share_one_count_and_keep_available_usage() {
    let root = repo("coverage");
    hook(
        &root,
        "pi",
        "prompt-submit",
        json!({"session_id":"s","event_id":"p","prompt":"fix source"}),
    );
    hook(
        &root,
        "pi",
        "model-response",
        json!({"session_id":"s","event_id":"m","response_id":"message-1","request_ids":["call-1"],"request_tools":{"call-1":"Read"},"coverage_complete":true,"usage":{"input":10,"output":4,"cache_read":2,"cache_write":0}}),
    );
    hook(
        &root,
        "pi",
        "pre-tool-use",
        json!({"session_id":"s","tool_use_id":"call-1","tool_name":"Read"}),
    );
    hook(
        &root,
        "pi",
        "user-bash",
        json!({"session_id":"s","event_id":"private-shell","command":"private command must not persist"}),
    );
    let task = pixel_task::Store::open(&root)
        .unwrap()
        .find_session("pi", "s")
        .unwrap()
        .unwrap();
    let events = good(&root, &["task", "events", &task.task_id, "--json"]);
    assert_eq!(events["trajectory"]["observed_model_tool_requests"], 1);
    assert_eq!(events["trajectory"]["model_responses"], 1);
    assert_eq!(
        events["trajectory"]["tokens"],
        json!({"input":10,"output":4,"cache_read":2,"cache_write":0})
    );
    assert_eq!(events["trajectory"]["model_tool_requests"], Value::Null);
    assert!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["data"]["kind"] == "external_action")
    );
    assert!(
        !events
            .to_string()
            .contains("private command must not persist")
    );
}

/// The product default is advisory: with no `task.enforcement` key a mutation
/// is observed and recorded, never denied. `enforce` opts the hard gate in.
#[test]
fn default_policy_observes_mutations_without_denying_them() {
    let root = repo("default-advisory");
    std::fs::remove_file(root.join(".pixel/config.json")).unwrap();
    let result = hook(
        &root,
        "pi",
        "pre-tool-use",
        json!({"session_id":"s","tool_name":"Write","tool_use_id":"first-edit"}),
    );
    assert_eq!(result["decision"], "observe");
    // The task still bound and recorded — advisory is not absent.
    assert!(
        pixel_task::Store::open(&root)
            .unwrap()
            .find_session("pi", "s")
            .unwrap()
            .is_some()
    );
}
