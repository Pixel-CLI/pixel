// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `pixel hook`: a host payload on stdin, one envelope or nothing on stdout,
//! exit status 0 whatever happens (`specs/hosts.md`).

use std::io::Write;
use std::process::{Output, Stdio};

use serde_json::{Value, json};

use crate::support::{Fixture, project, stderr, stdout};

const ASK: &str = "who calls `run` and what breaks if I rename it";

/// `pixel hook <args>` with `payload` on stdin, closed after it.
fn hook(fixture: &Fixture, args: &[&str], payload: &str, env: &[(&str, &str)]) -> Output {
    let mut all = vec!["hook"];
    all.extend_from_slice(args);
    let mut command = fixture.pixel(&all);
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("pixel runs");
    // A hook that rejects its command line exits without reading its input.
    let _ = child
        .stdin
        .take()
        .expect("stdin")
        .write_all(payload.as_bytes());
    child.wait_with_output().unwrap()
}

fn home_dir(fixture: &Fixture) -> std::path::PathBuf {
    fixture.dir.path().join("home")
}

fn prompt_payload(fixture: &Fixture, session: &str, prompt: &str) -> String {
    json!({
        "hook_event_name": "UserPromptSubmit",
        "session_id": session,
        "cwd": fixture.root(),
        "prompt": prompt,
    })
    .to_string()
}

fn envelope(output: &Output) -> Value {
    serde_json::from_str(stdout(output).trim())
        .unwrap_or_else(|error| panic!("one JSON object expected: {error}: {}", stdout(output)))
}

fn journal(fixture: &Fixture) -> Vec<Value> {
    std::fs::read_to_string(fixture.root().join(".pixel/journal.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn lines_of<'a>(lines: &'a [Value], kind: &str) -> Vec<&'a Value> {
    lines.iter().filter(|line| line["t"] == kind).collect()
}

#[test]
fn claude_gets_the_brief_as_additional_context_and_a_line_for_the_user() {
    let fixture = project();
    let output = hook(
        &fixture,
        &["prompt-submit", "--host", "claude"],
        &prompt_payload(&fixture, "s1", ASK),
        &[],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stderr(&output), "");
    let out = envelope(&output);
    let context = out["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(context.starts_with("[PIXEL:BRIEF]\n"), "{context}");
    assert!(context.contains("### src/a.rs:1-3 — run definition (function)"));
    assert_eq!(
        out["hookSpecificOutput"]["hookEventName"],
        "UserPromptSubmit"
    );
    assert!(
        out["systemMessage"]
            .as_str()
            .unwrap()
            .starts_with("🟩 pixel brief ❀ "),
        "{out}"
    );
    assert_eq!(out.as_object().unwrap().len(), 2);
}

#[test]
fn a_hook_journals_the_decision_and_the_prompt_under_a_hashed_session() {
    let fixture = project();
    hook(
        &fixture,
        &["prompt-submit", "--host", "codex"],
        &prompt_payload(&fixture, "secret-session-id", ASK),
        &[],
    );
    let lines = journal(&fixture);
    let briefs = lines_of(&lines, "brief");
    assert_eq!(briefs.len(), 1, "{lines:?}");
    assert_eq!(briefs[0]["provenance"]["source"], "hook");
    assert_eq!(briefs[0]["provenance"]["provider"], "codex");
    let prompts = lines_of(&lines, "prompt");
    assert_eq!(prompts.len(), 1, "{lines:?}");
    assert_eq!(prompts[0]["provider"], "codex");
    assert_eq!(prompts[0]["session"].as_str().unwrap().len(), 16);
    assert!(prompts[0]["bytes"].as_u64().unwrap() > 0);
    assert!(
        prompts[0]["injected"]
            .as_array()
            .unwrap()
            .contains(&json!("src/a.rs")),
        "{prompts:?}"
    );
    let text = std::fs::read_to_string(fixture.root().join(".pixel/journal.jsonl")).unwrap();
    assert!(!text.contains("secret-session-id"), "the session is hashed");
}

#[test]
fn each_host_gets_the_envelope_it_reads() {
    let fixture = project();
    let run = |host: &str| {
        envelope(&hook(
            &fixture,
            &["prompt-submit", "--host", host],
            &prompt_payload(&fixture, "s", ASK),
            &[],
        ))
    };
    let gemini = run("gemini");
    assert_eq!(gemini["hookSpecificOutput"]["hookEventName"], "BeforeAgent");
    assert!(gemini["systemMessage"].is_string());

    let devin = run("devin");
    assert!(devin.get("systemMessage").is_none(), "{devin}");
    let text = devin["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(text.starts_with("[PIXEL:BRIEF]"));
    assert!(text.contains("\n\nEnd your answer with this line, verbatim: 🟩 pixel brief ❀ "));

    let pi = run("pi");
    assert!(pi["context"].as_str().unwrap().starts_with("[PIXEL:BRIEF]"));
    assert!(pi["notice"].as_str().unwrap().starts_with("🟩 pixel brief"));
}

#[test]
fn antigravity_briefs_the_first_invocation_from_its_transcript() {
    let fixture = project();
    let transcript = fixture.dir.path().join("transcript.jsonl");
    std::fs::write(
        &transcript,
        json!({"type": "USER_INPUT", "source": "USER_EXPLICIT",
            "content": format!("<USER_REQUEST>{ASK}</USER_REQUEST>\n<ADDITIONAL_METADATA>t</ADDITIONAL_METADATA>")})
        .to_string(),
    )
    .unwrap();
    let payload = |invocation: u64| {
        json!({
            "conversationId": "c1", "invocationNum": invocation,
            "workspacePaths": [fixture.root()], "transcriptPath": transcript,
        })
        .to_string()
    };
    let first = hook(
        &fixture,
        &["prompt-submit", "--host", "antigravity"],
        &payload(0),
        &[],
    );
    let out = envelope(&first);
    let step = out["injectSteps"][0]["ephemeralMessage"].as_str().unwrap();
    assert!(step.starts_with("[PIXEL:BRIEF]"), "{step}");
    assert!(out.get("systemMessage").is_none());
    let later = hook(
        &fixture,
        &["prompt-submit", "--host", "antigravity"],
        &payload(1),
        &[],
    );
    assert_eq!(
        stdout(&later),
        "",
        "only a turn's first invocation is briefed"
    );
}

#[test]
fn a_prompt_that_asks_nothing_about_code_prints_nothing_and_journals_why() {
    let fixture = project();
    let output = hook(
        &fixture,
        &["prompt-submit", "--host", "claude"],
        &prompt_payload(&fixture, "s", "thanks, that works"),
        &[],
    );
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(stdout(&output), "");
    assert_eq!(stderr(&output), "");
    let lines = journal(&fixture);
    let briefs = lines_of(&lines, "brief");
    assert_eq!(briefs.len(), 1, "{lines:?}");
    assert_eq!(briefs[0]["gate"], "declined");
    assert_eq!(lines_of(&lines, "prompt")[0]["bytes"], 0);
}

#[test]
fn the_switches_silence_the_line_or_the_whole_brief() {
    let fixture = project();
    let payload = prompt_payload(&fixture, "s", ASK);
    let quiet = hook(
        &fixture,
        &["prompt-submit", "--host", "claude"],
        &payload,
        &[("PIXEL_METRICS", "0")],
    );
    let out = envelope(&quiet);
    assert!(out.get("systemMessage").is_none(), "{out}");
    assert!(out["hookSpecificOutput"]["additionalContext"].is_string());

    let before = journal(&fixture).len();
    let off = hook(
        &fixture,
        &["prompt-submit", "--host", "claude"],
        &payload,
        &[("PIXEL_BRIEF", "0")],
    );
    assert_eq!(stdout(&off), "");
    assert_eq!(
        journal(&fixture).len(),
        before,
        "nothing journaled when off"
    );
}

#[test]
fn a_hook_that_cannot_use_its_input_is_silence_with_status_zero() {
    let fixture = project();
    for payload in ["", "   ", "not json", "[1, 2]", "{\"prompt\": 1"] {
        let output = hook(
            &fixture,
            &["prompt-submit", "--host", "claude"],
            payload,
            &[],
        );
        assert_eq!(output.status.code(), Some(0), "{payload:?}");
        assert_eq!(stdout(&output), "", "{payload:?}");
    }
    // A flag this binary does not know, a host it has not heard of, an
    // unknown event: the host's command line must never fail its prompt.
    let good = prompt_payload(&fixture, "s", ASK);
    for args in [
        vec!["prompt-submit", "--host", "claude", "--bogus"],
        vec!["prompt-submit", "--host", "cursor"],
        vec!["nonsense", "--host", "claude"],
        vec!["prompt-submit"],
        vec!["prompt-submit", "--host", "claude", "--part", "9/3"],
    ] {
        let output = hook(&fixture, &args, &good, &[]);
        assert_eq!(output.status.code(), Some(0), "{args:?}");
        assert_eq!(stdout(&output), "", "{args:?}");
    }
}

#[test]
fn a_panic_inside_the_hook_still_exits_zero_and_adds_nothing() {
    let fixture = project();
    for stage in ["before-response", "after-response"] {
        let output = hook(
            &fixture,
            &["prompt-submit", "--host", "claude"],
            &prompt_payload(&fixture, "s", ASK),
            &[("PIXEL_HOOK_TEST_PANIC", stage)],
        );
        assert_eq!(output.status.code(), Some(0), "{stage}");
        let printed = stdout(&output);
        if stage == "before-response" {
            assert_eq!(printed, "");
        } else {
            assert_eq!(
                printed.lines().count(),
                1,
                "one envelope, nothing after: {printed}"
            );
        }
    }
}

#[test]
fn a_claude_entry_imported_by_devin_does_not_act() {
    let fixture = project();
    let output = hook(
        &fixture,
        &["prompt-submit", "--host", "claude"],
        &prompt_payload(&fixture, "s", ASK),
        &[("DEVIN_PROJECT_DIR", "/tmp/devin-repo")],
    );
    assert_eq!(stdout(&output), "");
    assert!(journal(&fixture).is_empty());
}

#[test]
fn a_split_brief_is_delivered_in_order_over_parallel_hooks() {
    let fixture = project();
    let payload = prompt_payload(&fixture, "split", ASK);
    let env = [("PIXEL_BRIEF_TEST_PAD_PARTS", "3")];
    let outputs: Vec<Output> = std::thread::scope(|scope| {
        let handles: Vec<_> = ["1/3", "2/3", "3/3"]
            .into_iter()
            .map(|part| {
                let (fixture, payload) = (&fixture, &payload);
                scope.spawn(move || {
                    hook(
                        fixture,
                        &["prompt-submit", "--host", "claude", "--part", part],
                        payload,
                        &env,
                    )
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });
    let slices: Vec<String> = outputs
        .iter()
        .map(|output| {
            assert_eq!(output.status.code(), Some(0), "{}", stderr(output));
            envelope(output)["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert!(
        slices[0].starts_with("[PIXEL:BRIEF]\n"),
        "{}",
        &slices[0][..40]
    );
    for (index, slice) in slices.iter().enumerate() {
        assert!(slice.chars().count() <= 9_800, "part {}", index + 1);
        assert!(
            slice.ends_with(&format!("PIXEL-PART-MARKER-{}-OF-3\n", index + 1)),
            "part {}: {}",
            index + 1,
            &slice[slice.len().saturating_sub(60)..]
        );
    }
    // Only the first part carries the line the user sees.
    let first = envelope(&outputs[0]);
    assert!(first["systemMessage"].is_string());
    let parts = journal(&fixture);
    let parts = lines_of(&parts, "part");
    assert_eq!(parts.len(), 3, "{parts:?}");
    assert!(
        parts
            .iter()
            .all(|line| line["of"] == 3 && line["late"] == false)
    );
}

#[test]
fn a_tool_call_leaves_a_line_naming_the_repository_files_and_prints_nothing() {
    let fixture = project();
    let edit = json!({
        "session_id": "s1", "cwd": fixture.root(), "tool_name": "Edit",
        "tool_input": {"file_path": fixture.root().join("src/a.rs"), "old_string": "x", "new_string": "y"},
    });
    let read = json!({
        "session_id": "s1", "cwd": fixture.root(), "tool_name": "Bash",
        "tool_input": {"command": "cat src/b.rs /etc/passwd > /dev/null"},
    });
    for payload in [edit, read] {
        let output = hook(
            &fixture,
            &["post-tool", "--host", "claude"],
            &payload.to_string(),
            &[],
        );
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(stdout(&output), "");
    }
    let lines = journal(&fixture);
    let tools = lines_of(&lines, "tool");
    assert_eq!(tools.len(), 2, "{lines:?}");
    assert_eq!(tools[0]["tool"], "Edit");
    assert_eq!(tools[0]["write"], true);
    assert_eq!(tools[0]["paths"], json!(["src/a.rs"]));
    assert_eq!(tools[1]["write"], false);
    assert_eq!(
        tools[1]["paths"],
        json!(["src/b.rs"]),
        "a path outside the repository is never named"
    );
    let text = std::fs::read_to_string(fixture.root().join(".pixel/journal.jsonl")).unwrap();
    assert!(!text.contains("old_string") && !text.contains("passwd"));
}

#[test]
fn a_stop_closes_the_turn_of_the_prompt_that_was_briefed() {
    let fixture = project();
    hook(
        &fixture,
        &["prompt-submit", "--host", "claude"],
        &prompt_payload(&fixture, "s1", ASK),
        &[],
    );
    let read = json!({
        "session_id": "s1", "cwd": fixture.root(), "tool_name": "Read",
        "tool_input": {"file_path": "src/a.rs"},
    });
    hook(
        &fixture,
        &["post-tool", "--host", "claude"],
        &read.to_string(),
        &[],
    );
    let stop = json!({"session_id": "s1", "cwd": fixture.root(), "stop_hook_active": false});
    let output = hook(
        &fixture,
        &["stop", "--host", "claude"],
        &stop.to_string(),
        &[],
    );
    assert_eq!(stdout(&output), "");
    let lines = journal(&fixture);
    assert_eq!(lines_of(&lines, "stop").len(), 1, "{lines:?}");
    let turns = lines_of(&lines, "turn");
    assert_eq!(turns.len(), 1, "{lines:?}");
    assert_eq!(turns[0]["calls"], 1);
}

#[test]
fn a_session_start_that_clears_forgets_what_the_session_was_sent() {
    let fixture = project();
    hook(
        &fixture,
        &["prompt-submit", "--host", "claude"],
        &prompt_payload(&fixture, "s1", ASK),
        &[],
    );
    let dir = fixture.root().join(".pixel/brief-sessions");
    let files = |dir: &std::path::Path| std::fs::read_dir(dir).map_or(0, Iterator::count);
    assert_eq!(files(&dir), 1, "the brief remembers what it sent");
    let start = |source: &str| {
        json!({"session_id": "s1", "cwd": fixture.root(), "source": source}).to_string()
    };
    hook(
        &fixture,
        &["session-start", "--host", "claude"],
        &start("resume"),
        &[],
    );
    assert_eq!(files(&dir), 1, "a resumed session keeps its context");
    let output = hook(
        &fixture,
        &["session-start", "--host", "claude"],
        &start("clear"),
        &[],
    );
    assert_eq!(stdout(&output), "");
    assert_eq!(files(&dir), 0, "a cleared session starts over");
}

const BLOCK: &str = "🟩 pixel find-symbol ❀ 55.2ms ❀ #753329\n  │\n  ├─ ⏱ 97% faster\n  ├─ § saved ~1269 tok\n  │\n  └────────────";
const RELAYED: &str =
    "🟩 pixel find-symbol ❀ 55.2ms ❀ #753329\n  ├─ ⏱ 97% faster\n  ├─ § saved ~1269 tok";

fn tool_payload(fixture: &Fixture, response: &str) -> String {
    json!({
        "session_id": "s1", "cwd": fixture.root(), "tool_name": "Bash",
        "tool_input": {"command": "pixel find-symbol run"},
        "tool_response": {"stdout": "src/a.rs:1 run", "stderr": response},
    })
    .to_string()
}

#[test]
fn a_tool_result_carrying_the_block_is_relayed_to_the_user_where_the_host_draws_it() {
    let fixture = project();
    let payload = tool_payload(&fixture, &format!("\n{BLOCK}\n"));
    for host in ["claude", "gemini", "codex"] {
        let output = hook(&fixture, &["post-tool", "--host", host], &payload, &[]);
        assert_eq!(output.status.code(), Some(0), "{host}");
        assert_eq!(
            envelope(&output),
            json!({"systemMessage": RELAYED}),
            "{host}"
        );
    }
    for host in ["devin", "pi", "opencode"] {
        let output = hook(&fixture, &["post-tool", "--host", host], &payload, &[]);
        assert_eq!(stdout(&output), "", "{host}");
    }
}

#[test]
fn the_relay_is_silent_without_a_block_with_metrics_off_or_for_a_quoted_block() {
    let fixture = project();
    let args = ["post-tool", "--host", "claude"];
    let with_block = tool_payload(&fixture, BLOCK);
    let off = hook(&fixture, &args, &with_block, &[("PIXEL_METRICS", "0")]);
    assert_eq!(stdout(&off), "");
    let none = hook(
        &fixture,
        &args,
        &tool_payload(&fixture, "nothing here"),
        &[],
    );
    assert_eq!(stdout(&none), "");
    let quoted = hook(
        &fixture,
        &args,
        &tool_payload(&fixture, &format!("a hit\n{BLOCK}\nmore")),
        &[],
    );
    assert_eq!(
        stdout(&quoted),
        "",
        "a block that is not at the end is a quotation"
    );
    let sent = json!({
        "session_id": "s1", "cwd": fixture.root(), "tool_name": "Bash",
        "tool_input": {"command": BLOCK}, "tool_response": "ok",
    });
    assert_eq!(stdout(&hook(&fixture, &args, &sent.to_string(), &[])), "");
    let two = json!({
        "session_id": "s1", "cwd": fixture.root(), "tool_name": "Bash",
        "tool_input": {"command": "pixel find-symbol run; pixel who-calls run"},
        "tool_response": {"stdout": BLOCK, "stderr": BLOCK.replace("753329", "0a1b2c")},
    });
    let ambiguous = hook(
        &fixture,
        &["post-tool", "--host", "codex"],
        &two.to_string(),
        &[],
    );
    assert_eq!(stdout(&ambiguous), "", "Codex relays only a unique block");
}

#[test]
fn antigravity_delivers_the_blocks_of_an_invocation_when_it_ends() {
    let fixture = project();
    let home = home_dir(&fixture);
    let artifacts = home.join(".gemini/artifacts/c1");
    std::fs::create_dir_all(&artifacts).unwrap();
    let step = |index: u64, response: &str| {
        json!({
            "conversationId": "c1", "stepIdx": index,
            "artifactDirectoryPath": artifacts.display().to_string(),
            "workspacePaths": [fixture.root()],
            "toolCall": {"name": "run_command", "args": {"CommandLine": "pixel find-symbol run"}},
            "result": response,
        })
        .to_string()
    };
    let env = [("HOME", home.to_str().unwrap())];
    for (index, response) in [(5, BLOCK), (4, "no block")] {
        let output = hook(
            &fixture,
            &["post-tool", "--host", "antigravity"],
            &step(index, response),
            &env,
        );
        assert_eq!(stdout(&output), "", "storing prints nothing");
    }
    let end = json!({
        "conversationId": "c1", "initialNumSteps": 3,
        "artifactDirectoryPath": artifacts.display().to_string(),
        "workspacePaths": [fixture.root()],
    });
    let output = hook(
        &fixture,
        &["stop", "--host", "antigravity"],
        &end.to_string(),
        &env,
    );
    assert_eq!(
        envelope(&output),
        json!({"injectSteps": [{"ephemeralMessage": RELAYED}]})
    );
    let again = hook(
        &fixture,
        &["stop", "--host", "antigravity"],
        &end.to_string(),
        &env,
    );
    assert_eq!(stdout(&again), "", "delivered once");
}
