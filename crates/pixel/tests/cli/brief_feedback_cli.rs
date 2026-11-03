// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The read-feedback log through the real binary: the registered `task-event`
//! hooks write what the brief injected, what the agent read after it and how
//! many tool calls came before the first answer to `.pixel/brief-feedback.jsonl`,
//! never a prompt, a command or a credential, and `PIXEL_BRIEF_LOG=0` writes none.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Stdio;

use serde_json::{Value, json};

use crate::support::{Scratch, git, pixel_command};

const PROMPT: &str = "where is `handleError` defined?";
const SESSION: &str = "feedback-session";

fn indexed(tag: &str) -> Scratch {
    let root = Scratch::for_test("brief-feedback-cli", tag);
    std::fs::create_dir_all(root.join("packages/ui")).unwrap();
    std::fs::create_dir_all(root.join("apps/web")).unwrap();
    std::fs::write(
        root.join("packages/ui/handleError.ts"),
        "export function handleError(e: Error): string {\n  return \"boom \" + e.message;\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("apps/web/page.tsx"),
        "import { handleError } from \"../../packages/ui/handleError\";\nexport function Page() {\n  return handleError(new Error(\"x\"));\n}\n",
    )
    .unwrap();
    std::fs::write(root.join(".env"), "TOKEN=do-not-log-this\n").unwrap();
    // A tree of its own: the base-shard cache keys a fixture by its tree.
    std::fs::write(root.join("fixture.txt"), format!("{tag}\n")).unwrap();
    std::fs::write(root.join(".gitignore"), ".pixel/\n").unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "fixture"]);
    let search = pixel_command()
        .args(["search-content", "-F", "handleError", "-l", "--no-daemon"])
        .args(["--metrics", "off"])
        .arg(&*root)
        .output()
        .unwrap();
    assert!(search.status.success(), "{search:?}");
    let graph = pixel_command()
        .args(["rebuild-graph", "--metrics", "off"])
        .arg(&*root)
        .output()
        .unwrap();
    assert!(graph.status.success(), "{graph:?}");
    root
}

/// Run `run-hook task-event` for `event` with `payload`; return what it printed.
fn hook(
    root: &Path,
    event: &str,
    part: Option<&str>,
    payload: &Value,
    configure: impl FnOnce(&mut std::process::Command),
) -> Value {
    hook_as("codex", root, event, part, payload, configure)
}

/// [`hook`] for another `--provider`.
fn hook_as(
    provider: &str,
    root: &Path,
    event: &str,
    part: Option<&str>,
    payload: &Value,
    configure: impl FnOnce(&mut std::process::Command),
) -> Value {
    let mut command = pixel_command();
    command
        .current_dir(root)
        .env("PIXEL_METRICS", "0")
        .env("CODEX_THREAD_ID", "inherited-session-must-not-win")
        .env_remove("PIXEL_BRIEF")
        .env_remove("PIXEL_BRIEF_LOG")
        .env("PIXEL_BRIEF_PACK", "0")
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
        .stderr(Stdio::piped());
    if let Some(part) = part {
        command.args(["--part", part]);
    }
    configure(&mut command);
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout).unwrap_or(Value::Null)
}

fn prompt_payload(root: &Path, session: &str) -> Value {
    json!({"session_id": session, "prompt": PROMPT, "cwd": root, "hook_event_name": "UserPromptSubmit"})
}

fn tool_payload(root: &Path, session: &str, tool: &str, input: &Value) -> Value {
    json!({"session_id": session, "cwd": root, "tool_name": tool, "tool_input": input,
           "tool_response": "private output nobody may log", "hook_event_name": "PostToolUse"})
}

fn stop_payload(root: &Path, session: &str) -> Value {
    json!({"session_id": session, "cwd": root, "hook_event_name": "Stop"})
}

fn log_lines(root: &Path) -> Vec<Value> {
    let path = root.join(".pixel/brief-feedback.jsonl");
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn of_kind<'a>(lines: &'a [Value], kind: &str) -> Vec<&'a Value> {
    lines.iter().filter(|line| line["t"] == kind).collect()
}

fn strings(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap())
        .collect()
}

#[test]
fn a_turn_should_join_what_the_brief_injected_to_what_the_agent_read_before_answering() {
    let root = indexed("turn");
    hook(
        &root,
        "prompt-submit",
        None,
        &prompt_payload(&root, SESSION),
        |_| {},
    );
    let page = root.join("apps/web/page.tsx");
    hook(
        &root,
        "post-tool-use",
        None,
        &tool_payload(&root, SESSION, "Read", &json!({"file_path": page})),
        |_| {},
    );
    hook(
        &root,
        "post-tool-use",
        None,
        &tool_payload(
            &root,
            SESSION,
            "Bash",
            &json!({"command": "sed -n '1,5p' packages/ui/handleError.ts .env | rg secret"}),
        ),
        |_| {},
    );
    hook(&root, "stop", None, &stop_payload(&root, SESSION), |_| {});

    let lines = log_lines(&root);
    let prompts = of_kind(&lines, "prompt");
    assert_eq!(prompts.len(), 1, "{lines:?}");
    let prompt = prompts[0];
    for line in &lines {
        assert_eq!(line["provenance"]["schema_version"], 1);
        assert_eq!(line["provenance"]["source"], "hook");
        assert_eq!(line["provenance"]["provider"], "codex");
        assert_eq!(line["provenance"]["session"], line["session"]);
        assert_eq!(
            line["provenance"]["build_commit"],
            env!("PIXEL_BUILD_COMMIT")
        );
        assert!(line["provenance"]["invocation_id"].as_str().is_some());
    }
    let decisions = std::fs::read_to_string(root.join(".pixel/brief-decisions.jsonl")).unwrap();
    let decision: Value = decisions
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|line| line["provenance"]["invocation_id"] == prompt["provenance"]["invocation_id"])
        .unwrap();
    assert_eq!(decision["provenance"]["session"], prompt["session"]);
    assert_eq!(decision["provenance"]["source"], "brief");
    assert!(!decisions.contains(SESSION));
    assert!(prompt["bytes"].as_u64().unwrap() > 0, "{prompt}");
    assert_eq!(prompt["gate"], "unjudged");
    assert!(
        strings(&prompt["injected"]).contains(&"packages/ui/handleError.ts"),
        "{prompt}"
    );
    let tools = of_kind(&lines, "tool");
    assert_eq!(tools.len(), 2);
    assert_eq!(strings(&tools[0]["paths"]), vec!["apps/web/page.tsx"]);
    assert_eq!(
        strings(&tools[1]["paths"]),
        vec!["packages/ui/handleError.ts"]
    );
    let turns = of_kind(&lines, "turn");
    assert_eq!(turns.len(), 1, "{lines:?}");
    let turn = turns[0];
    assert_eq!(turn["calls"], 2);
    assert_eq!(turn["briefed"], true);
    assert_eq!(turn["decision"], prompt["decision"]);
    assert!(strings(&turn["overlap"]).contains(&"packages/ui/handleError.ts"));
    assert_eq!(of_kind(&lines, "stop").len(), 1);
}

#[test]
fn an_answer_with_no_tool_call_should_close_a_zero_call_turn() {
    let root = indexed("zero");
    hook(
        &root,
        "prompt-submit",
        None,
        &prompt_payload(&root, SESSION),
        |_| {},
    );
    hook(&root, "stop", None, &stop_payload(&root, SESSION), |_| {});
    // A second stop for the same prompt closes nothing twice.
    hook(&root, "stop", None, &stop_payload(&root, SESSION), |_| {});
    let lines = log_lines(&root);
    let turns = of_kind(&lines, "turn");
    assert_eq!(turns.len(), 1, "{lines:?}");
    assert_eq!(turns[0]["calls"], 0);
    assert_eq!(turns[0]["briefed"], true);
    assert_eq!(strings(&turns[0]["read"]), Vec::<&str>::new());
    assert_eq!(of_kind(&lines, "stop").len(), 2);
}

#[test]
fn the_log_should_hold_paths_and_counts_but_no_prompt_command_output_or_credential() {
    let root = indexed("private");
    hook(
        &root,
        "prompt-submit",
        None,
        &prompt_payload(&root, SESSION),
        |_| {},
    );
    hook(
        &root,
        "post-tool-use",
        None,
        &tool_payload(
            &root,
            SESSION,
            "Bash",
            &json!({"command": "cat .env packages/ui/handleError.ts"}),
        ),
        |_| {},
    );
    hook(&root, "stop", None, &stop_payload(&root, SESSION), |_| {});
    let text = std::fs::read_to_string(root.join(".pixel/brief-feedback.jsonl")).unwrap();
    for secret in [
        "do-not-log-this",
        "private output",
        "where is",
        SESSION,
        "cat ",
    ] {
        assert!(!text.contains(secret), "`{secret}` leaked into {text}");
    }
    assert!(
        !text.contains("\".env\""),
        "a credential path is never named: {text}"
    );
    let mode = std::fs::metadata(root.join(".pixel/brief-feedback.jsonl"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn the_switch_should_write_no_feedback_at_all() {
    let root = indexed("off");
    let off = |command: &mut std::process::Command| {
        command.env("PIXEL_BRIEF_LOG", "0");
    };
    hook(
        &root,
        "prompt-submit",
        None,
        &prompt_payload(&root, SESSION),
        off,
    );
    hook(&root, "stop", None, &stop_payload(&root, SESSION), off);
    assert!(!root.join(".pixel/brief-feedback.jsonl").exists());
    // And no session id means nothing to join by.
    hook(
        &root,
        "stop",
        None,
        &json!({"cwd": &*root, "hook_event_name": "Stop"}),
        |_| {},
    );
    assert!(!root.join(".pixel/brief-feedback.jsonl").exists());
}

#[test]
fn every_hook_part_should_log_the_bytes_it_printed_to_the_host() {
    let root = indexed("parts");
    let payload = prompt_payload(&root, SESSION);
    // A cold repository takes part 1 longer than a later part waits for it:
    // warm it with one whole-brief hook, and start the log over.
    hook(&root, "prompt-submit", None, &payload, |_| {});
    std::fs::remove_file(root.join(".pixel/brief-feedback.jsonl")).unwrap();
    // Part 1 first: it stores the brief and marks itself done, so part 2 does
    // not race a slow part 1 for its two-second wait on a loaded machine.
    let first = hook(&root, "prompt-submit", Some("1/2"), &payload, |_| {});
    let second = hook(&root, "prompt-submit", Some("2/2"), &payload, |_| {});
    let printed = first["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .len();
    assert!(printed > 0);
    assert_eq!(
        second,
        json!({}),
        "a short brief leaves part 2 nothing to print"
    );
    let lines = log_lines(&root);
    let mut parts = of_kind(&lines, "part");
    parts.sort_by_key(|part| part["part"].as_u64());
    assert_eq!(parts.len(), 2, "{lines:?}");
    assert_eq!(
        (parts[0]["part"].as_u64(), parts[0]["of"].as_u64()),
        (Some(1), Some(2))
    );
    assert_eq!(parts[0]["bytes"].as_u64(), Some(printed as u64));
    assert_eq!(
        (parts[1]["part"].as_u64(), parts[1]["of"].as_u64()),
        (Some(2), Some(2))
    );
    assert_eq!(parts[1]["bytes"].as_u64(), Some(0));
    assert_eq!(parts[1]["late"], false);
    assert_eq!(
        parts[0]["prompt"], parts[1]["prompt"],
        "parts join by the prompt"
    );
}

/// A transcript outside the repository (a file inside it would be a dirty file
/// of the tree) whose last tool call read `file`.
fn transcript_reading(tag: &str, file: &Path) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "pixel-brief-feedback-{tag}-{}.jsonl",
        std::process::id()
    ));
    let line = json!({"type": "assistant", "message": {"content": [
        {"type": "tool_use", "name": "Read", "input": {"file_path": file}}]}});
    std::fs::write(&path, format!("{line}\n")).unwrap();
    path
}

fn follow_up_payload(root: &Path, transcript: Option<&Path>) -> Value {
    json!({"session_id": SESSION, "prompt": "now fix the test", "cwd": root,
           "transcript_path": transcript, "hook_event_name": "UserPromptSubmit"})
}

#[test]
fn a_short_follow_up_should_be_briefed_on_the_files_the_transcript_names() {
    let root = indexed("follow-up");
    let page = root.join("apps/web/page.tsx");
    let transcript = transcript_reading("follow-up", &page);
    let briefed = hook(
        &root,
        "prompt-submit",
        None,
        &follow_up_payload(&root, Some(&transcript)),
        |_| {},
    );
    let text = briefed["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_else(|| panic!("no brief in {briefed}"));
    assert!(text.contains("files: apps/web/page.tsx"), "{text}");
    assert!(
        !text.contains("now fix the test"),
        "no prompt text in the brief: {text}"
    );
    // The same words with no transcript, and a clean tree, have nothing to lean on.
    let bare = hook(
        &root,
        "prompt-submit",
        None,
        &follow_up_payload(&root, None),
        |_| {},
    );
    assert_eq!(bare.get("hookSpecificOutput"), None, "{bare}");
    // And the switch turns the lift off.
    let off = hook(
        &root,
        "prompt-submit",
        None,
        &follow_up_payload(&root, Some(&transcript)),
        |command| {
            command.env("PIXEL_BRIEF_CONTEXT", "0");
        },
    );
    assert_eq!(off.get("hookSpecificOutput"), None, "{off}");
    let _ = std::fs::remove_file(&transcript);
}

#[test]
fn a_dirty_file_should_brief_a_short_follow_up_in_a_session_with_no_transcript() {
    let root = indexed("dirty");
    std::fs::write(root.join("apps/web/page.tsx"), "export const edited = 1;\n").unwrap();
    let briefed = hook(
        &root,
        "prompt-submit",
        None,
        &follow_up_payload(&root, None),
        |_| {},
    );
    let text = briefed["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_else(|| panic!("no brief in {briefed}"));
    assert!(text.contains("files: apps/web/page.tsx"), "{text}");
}

fn blocks_of(output: &Value) -> usize {
    output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_default()
        .matches("\n### ")
        .count()
}

fn with_pack(command: &mut std::process::Command) {
    command.env("PIXEL_BRIEF_PACK", "1");
}

/// The blocks of the pack `ask` gets, asked again while a loaded machine ends
/// the brief's window before the pack has any (nothing was sent then, so
/// asking again sends the same pack).
fn blocks_eventually(ask: &dyn Fn() -> Value) -> usize {
    (0..5)
        .map(|_| blocks_of(&ask()))
        .find(|blocks| *blocks > 0)
        .unwrap_or(0)
}

#[test]
fn a_session_should_not_be_sent_the_same_source_twice_until_the_host_starts_it_over() {
    let root = indexed("dedupe");
    let ask = |session: &str, configure: &dyn Fn(&mut std::process::Command)| {
        let payload = prompt_payload(&root, session);
        hook(&root, "prompt-submit", None, &payload, |command| {
            with_pack(command);
            configure(command);
        })
    };
    let first = blocks_eventually(&|| ask(SESSION, &|_| {}));
    assert!(first >= 1, "the first prompt of a session gets the pack");
    let sessions = root.join(".pixel/brief-sessions");
    let files: Vec<_> = std::fs::read_dir(&sessions).unwrap().flatten().collect();
    assert_eq!(files.len(), 1, "one file for the session");
    let text = std::fs::read_to_string(files[0].path()).unwrap();
    assert!(
        !text.contains("handleError") && !text.contains(SESSION),
        "keys only: {text}"
    );

    // The same question, the same session: the source it was sent is not repeated.
    assert_eq!(blocks_of(&ask(SESSION, &|_| {})), 0);
    // Another session holds none of it; the switch sends it all again.
    assert!(blocks_eventually(&|| ask("another-session", &|_| {})) > 0);
    let off = |command: &mut std::process::Command| {
        command.env("PIXEL_BRIEF_DEDUPE", "0");
    };
    assert!(blocks_eventually(&|| ask(SESSION, &off)) > 0);

    // A resume keeps the same context; a compaction or a clear does not.
    for (source, again) in [("resume", false), ("compact", true), ("clear", true)] {
        hook(
            &root,
            "session-start",
            None,
            &json!({"session_id": SESSION, "cwd": &*root, "source": source,
                    "hook_event_name": "SessionStart"}),
            |_| {},
        );
        if again {
            assert!(
                blocks_eventually(&|| ask(SESSION, &|_| {})) > 0,
                "{source}: sent again after a start-over"
            );
        }
        // Whatever was just sent, or kept, is withheld from the next prompt.
        assert_eq!(blocks_of(&ask(SESSION, &|_| {})), 0, "{source}");
    }
}

/// A Pi prompt as Pixel's Pi package sends it: the brief alone, no ledger.
fn pi_prompt(root: &Path, session: &str) -> Value {
    let mut payload = prompt_payload(root, session);
    payload["brief_only"] = json!(true);
    payload
}

/// The brief a Pi hook answer carries: Pi's envelope is the bare decision.
fn pi_context(output: &Value) -> Option<&str> {
    output["context"].as_str()
}

/// Ask `run` until the brief arrives (a loaded machine ends its window first).
fn pi_brief_eventually(run: &dyn Fn() -> Value) -> Value {
    (0..5)
        .map(|_| run())
        .find(|output| pi_context(output).is_some())
        .expect("the brief arrives within five asks")
}

#[test]
fn a_pi_prompt_that_asks_for_the_brief_alone_should_get_it_and_the_metrics_line_with_no_task_bound()
{
    let root = indexed("pi-brief");
    let output = pi_brief_eventually(&|| {
        hook_as(
            "pi",
            &root,
            "prompt-submit",
            None,
            &pi_prompt(&root, "pi-session"),
            |command| {
                command.env_remove("PIXEL_METRICS");
            },
        )
    });
    let brief = pi_context(&output).unwrap();
    assert!(brief.starts_with("[PIXEL:BRIEF]"), "{brief}");
    assert_eq!(output["decision"], "observe");
    assert!(
        output.get("hookSpecificOutput").is_none(),
        "Pi's envelope is the bare decision: {output}"
    );
    let line = output["systemMessage"].as_str().unwrap();
    assert!(line.starts_with("🟩 pixel prompt-submit ❀ "), "{line}");
    assert!(
        !brief.contains('🟩'),
        "the metrics line is for the user, not the model"
    );
    assert!(
        !root.join(".pixel/tasks").exists(),
        "no task is bound for a brief-only event"
    );
    let lines = log_lines(&root);
    let prompts = of_kind(&lines, "prompt");
    assert!(!prompts.is_empty(), "{lines:?}");
    assert!(prompts.iter().all(|line| line["provider"] == "pi"));
    assert!(
        prompts
            .iter()
            .any(|line| line["bytes"].as_u64() == Some(brief.len() as u64)),
        "{prompts:?}"
    );
    // The part line of the delivering call counts the bare `context` Pi reads,
    // not the wrapped envelope's `additionalContext` it never carries.
    let parts = of_kind(&lines, "part");
    let last = parts.last().unwrap();
    assert_eq!(last["provider"], "pi", "{last}");
    assert_eq!(last["bytes"].as_u64(), Some(brief.len() as u64), "{last}");
    assert_eq!(last["late"], false, "{last}");
}

#[test]
fn a_pi_brief_should_carry_no_metrics_line_when_metrics_are_off() {
    let root = indexed("pi-metrics-off");
    let output = pi_brief_eventually(&|| {
        hook_as(
            "pi",
            &root,
            "prompt-submit",
            None,
            &pi_prompt(&root, "pi-session"),
            |_| {},
        )
    });
    assert!(pi_context(&output).unwrap().starts_with("[PIXEL:BRIEF]"));
    assert!(output.get("systemMessage").is_none(), "{output}");
}

#[test]
fn a_pi_prompt_without_the_marker_should_keep_the_ledger_and_get_no_brief() {
    let root = indexed("pi-ledger");
    let output = hook_as(
        "pi",
        &root,
        "prompt-submit",
        None,
        &prompt_payload(&root, "pi-session"),
        |command| {
            command.env_remove("PIXEL_METRICS");
        },
    );
    assert_eq!(pi_context(&output), None, "{output}");
    assert!(output.get("systemMessage").is_none(), "{output}");
    assert!(
        root.join(".pixel/tasks").exists(),
        "the ledger ran for a host that drives the lifecycle itself"
    );
    assert!(of_kind(&log_lines(&root), "prompt").is_empty());
}

#[test]
fn a_pi_session_start_should_forget_what_a_compaction_or_a_new_session_dropped_and_bind_no_task() {
    let root = indexed("pi-session");
    let ask = || {
        hook_as(
            "pi",
            &root,
            "prompt-submit",
            None,
            &pi_prompt(&root, SESSION),
            with_pack,
        )
    };
    let blocks = |output: &Value| {
        pi_context(output)
            .unwrap_or_default()
            .matches("\n### ")
            .count()
    };
    assert!(
        (0..5).map(|_| blocks(&ask())).any(|blocks| blocks > 0),
        "the first prompt of a session gets the pack"
    );
    assert_eq!(blocks(&ask()), 0, "the same source is not sent twice");
    for (source, again) in [("startup", false), ("compact", true), ("clear", true)] {
        let start = hook_as(
            "pi",
            &root,
            "session-start",
            None,
            &json!({"session_id": SESSION, "cwd": &*root, "source": source,
                    "hook_event_name": "SessionStart", "brief_only": true}),
            |_| {},
        );
        assert_eq!(start, json!({"decision": "observe"}), "{source}");
        if again {
            assert!(
                (0..5).map(|_| blocks(&ask())).any(|blocks| blocks > 0),
                "{source}: sent again after a start-over"
            );
        }
        assert_eq!(blocks(&ask()), 0, "{source}");
    }
    assert!(!root.join(".pixel/tasks").exists());
}

/// An Antigravity prompt as its `PreInvocation` hook sends it: no
/// `session_id` and no `cwd`, the conversation, the workspace and the
/// transcript the typed prompt is read from.
fn antigravity_prompt(root: &Path, conversation: &str) -> Value {
    let transcript = root.join(".pixel/agy-transcript.jsonl");
    std::fs::write(
        &transcript,
        json!({"role": "user", "content": PROMPT}).to_string(),
    )
    .unwrap();
    json!({"conversationId": conversation, "workspacePaths": [root], "invocationNum": 0,
           "transcriptPath": transcript, "artifactDirectoryPath": root.join(".pixel"),
           "modelName": "test"})
}

#[test]
fn an_antigravity_brief_should_leave_its_feedback_lines_under_the_conversation() {
    let root = indexed("antigravity-brief");
    let output = (0..5)
        .map(|_| {
            hook_as(
                "antigravity",
                &root,
                "prompt-submit",
                None,
                &antigravity_prompt(&root, "agy-conversation"),
                |_| {},
            )
        })
        .find(|output| output["injectSteps"][0]["ephemeralMessage"].is_string())
        .expect("the brief arrives within five asks");
    let brief = output["injectSteps"][0]["ephemeralMessage"]
        .as_str()
        .unwrap();
    assert!(brief.starts_with("[PIXEL:BRIEF]"), "{brief}");
    let lines = log_lines(&root);
    let prompts = of_kind(&lines, "prompt");
    assert!(
        prompts.iter().any(|line| line["provider"] == "antigravity"
            && line["bytes"].as_u64() == Some(brief.len() as u64)),
        "{lines:?}"
    );
    // The part line of the delivering call counts the `ephemeralMessage`.
    let last = *of_kind(&lines, "part").last().unwrap();
    assert_eq!(last["provider"], "antigravity", "{last}");
    assert_eq!(last["bytes"].as_u64(), Some(brief.len() as u64), "{last}");
    // Keyed by the conversation, hashed like every host's session id.
    let session = last["session"].as_str().unwrap();
    assert_eq!(session.len(), 16, "{last}");
    assert!(!session.contains("agy-conversation"), "{last}");
    assert!(
        prompts
            .iter()
            .all(|line| line["session"] == last["session"])
    );
}

#[test]
fn metrics_should_report_the_brief_and_turn_the_hooks_wrote() {
    let root = indexed("metrics");
    hook(
        &root,
        "prompt-submit",
        None,
        &prompt_payload(&root, SESSION),
        |_| {},
    );
    hook(
        &root,
        "post-tool-use",
        None,
        &tool_payload(
            &root,
            SESSION,
            "Read",
            &json!({"file_path": root.join("packages/ui/handleError.ts")}),
        ),
        |_| {},
    );
    hook(&root, "stop", None, &stop_payload(&root, SESSION), |_| {});

    let lines = log_lines(&root);
    let turn = of_kind(&lines, "turn")[0];
    let out = pixel_command()
        .current_dir(&*root)
        .args(["metrics", "--json", "--metrics=off"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["briefs"]["delivered"], 1, "{report}");
    assert_eq!(report["turns"]["briefed"], 1);
    assert_eq!(
        report["turns"]["zero_call"], 0,
        "one Read came before the answer"
    );
    assert_eq!(
        report["files"]["injected"].as_u64(),
        Some(turn["injected"].as_array().unwrap().len() as u64)
    );
    assert_eq!(report["files"]["read_or_edited"], 1);
    assert_eq!(
        report["files"]["injected_and_read"], 1,
        "the Read file was injected"
    );
}
