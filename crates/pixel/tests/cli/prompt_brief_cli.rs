// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The evidence brief rides the registered `task-event` prompt hook: a code
//! prompt in an indexed repository gets `[PIXEL:BRIEF]` as additional context,
//! every other case gets nothing extra, and no case builds or refreshes an index.

use std::io::Write;
use std::path::Path;
use std::process::Stdio;
use std::time::SystemTime;

use serde_json::{Value, json};

use crate::support::{Scratch, git, pixel_command};

const RENAME: &str = "handleError in packages/ui/handleError.ts is being renamed to reportError. Which files change?";

fn fixture(tag: &str) -> Scratch {
    let root = Scratch::for_test("prompt-brief-cli", tag);
    std::fs::create_dir_all(root.join("packages/ui")).unwrap();
    std::fs::create_dir_all(root.join("apps/web")).unwrap();
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::fs::write(
        root.join("packages/ui/handleError.ts"),
        "export function handleError(e: Error): string {\n  return \"boom \" + e.message;\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("apps/web/page.tsx"),
        [
            "import { handleError } from \"../../packages/ui/handleError\";",
            "export function Page() {",
            "  try { return 1; } catch (e) { return handleError(e as Error); }",
            "}",
            "export function Other() { return handleError(new Error(\"x\")); }",
            "",
        ]
        .join("\n"),
    )
    .unwrap();
    std::fs::write(root.join("data/out.json"), "{\"handleError\": 1}\n").unwrap();
    std::fs::write(root.join(".gitignore"), ".pixel/\n").unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "fixture"]);
    root
}

/// A fixture with the text index and the graph built, as a repository that
/// has been used with Pixel before.
fn indexed(tag: &str) -> Scratch {
    let root = fixture(tag);
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

fn hook(root: &Path, provider: &str, prompt: &str, env: &[(&str, &str)]) -> Value {
    let payload = json!({
        "session_id": "brief-session",
        "prompt": prompt,
        "cwd": root,
        "hook_event_name": "UserPromptSubmit",
    });
    let mut command = pixel_command();
    command
        .current_dir(root)
        .env("PIXEL_METRICS", "0")
        .env_remove("PIXEL_BRIEF")
        .args([
            "run-hook",
            "task-event",
            "--provider",
            provider,
            "--event",
            "prompt-submit",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command.spawn().unwrap();
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

fn context(output: &Value) -> &str {
    output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_else(|| panic!("no additional context in {output}"))
}

fn stamp(path: &Path) -> (u64, SystemTime) {
    let meta = std::fs::metadata(path).unwrap();
    (meta.len(), meta.modified().unwrap())
}

#[test]
fn rename_prompt_should_carry_files_definition_and_callers_in_the_hook_context() {
    let root = indexed("rename");
    for provider in ["claude", "codex"] {
        let output = hook(&root, provider, RENAME, &[]);
        assert_eq!(
            output["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit",
            "{provider}"
        );
        assert_eq!(
            context(&output),
            [
                "[PIXEL:BRIEF]",
                "kind: lookup",
                "anchors: handleError, reportError, packages/ui/handleError.ts",
                "defined: function handleError packages/ui/handleError.ts:1-3 — export function handleError(e: Error): string {   return \"boom \" + e.message; }",
                "files: apps/web/page.tsx:1 — import { handleError } from \"../../packages/ui/handleError\";; apps/web/page.tsx:3 —   try { return 1; } catch (e) { return handleError(e as Error); }; apps/web/page.tsx:5 — export function Other() { return handleError(new Error(\"x\")); }; packages/ui/handleError.ts:1 — export function handleError(e: Error): string {",
                "callers (impact d1): apps/web/page.tsx -> Page:2; apps/web/page.tsx -> Other:5",
                "excluded (generated): data/out.json",
                "coverage: 4/4 ops answered",
                "Answer from this evidence; open a file only if it contradicts you. 0 hits or 0 callers: verify with rg before concluding.",
            ]
            .join("\n"),
            "{provider}"
        );
    }
}

#[test]
fn literal_lookup_should_skip_impact_and_answer_every_op() {
    let root = indexed("lookup");
    let output = hook(&root, "claude", "where is `handleError` defined?", &[]);
    let text = context(&output);
    assert!(text.contains("\ndefined: function handleError packages/ui/handleError.ts:1-3 — export function handleError"));
    assert!(!text.contains("callers (impact"), "{text}");
    assert!(text.contains("\ncoverage: 3/3 ops answered\n"), "{text}");
}

#[test]
fn an_unindexed_repository_should_get_no_brief_and_no_index() {
    let root = fixture("unindexed");
    let output = hook(&root, "claude", RENAME, &[]);
    assert_eq!(output, json!({}));
    assert!(!root.join(".pixel/base.shard").exists());
    assert!(!root.join(".pixel/graph.v2.db").exists());
}

#[test]
fn a_prompt_that_asks_nothing_about_code_should_get_no_brief() {
    let root = indexed("trivial");
    for prompt in ["thanks, that looks good", "commit this and push it"] {
        assert_eq!(hook(&root, "claude", prompt, &[]), json!({}), "{prompt}");
    }
}

#[test]
fn the_environment_and_the_repository_setting_should_each_switch_the_brief_off() {
    let root = indexed("opt-out");
    for value in ["0", "off", "false"] {
        assert_eq!(
            hook(&root, "claude", RENAME, &[("PIXEL_BRIEF", value)]),
            json!({}),
            "PIXEL_BRIEF={value}"
        );
    }
    assert!(
        context(&hook(&root, "claude", RENAME, &[("PIXEL_BRIEF", "1")]))
            .starts_with("[PIXEL:BRIEF]")
    );
    std::fs::write(root.join(".pixel/config.yaml"), "brief: false\n").unwrap();
    assert_eq!(hook(&root, "claude", RENAME, &[]), json!({}));
}

#[test]
fn pi_should_keep_its_own_decision_without_a_brief() {
    let root = indexed("pi");
    let output = hook(&root, "pi", RENAME, &[]);
    assert!(output.get("context").is_none(), "{output}");
    assert!(output.get("hookSpecificOutput").is_none(), "{output}");
}

#[test]
fn a_stale_graph_should_be_named_and_never_rebuilt_by_the_hook() {
    let root = indexed("stale");
    std::fs::write(
        root.join("apps/web/page.tsx"),
        "export function Page() { return 2; }\n",
    )
    .unwrap();
    let graph = root.join(".pixel/graph.v2.db");
    let shard = root.join(".pixel/base.shard");
    let before = (stamp(&graph), stamp(&shard));
    let output = hook(&root, "claude", RENAME, &[]);
    let text = context(&output);
    assert!(
        text.contains("\nunresolved: find-symbol handleError: the graph is stale\n"),
        "{text}"
    );
    assert!(!text.contains("callers (impact"), "{text}");
    assert!(text.contains("\ncoverage: 1/2 ops answered\n"), "{text}");
    assert!(text.contains("\npacket partial —"), "{text}");
    assert_eq!(before, (stamp(&graph), stamp(&shard)));
}

#[test]
fn no_file_anchor_in_a_hook_brief_should_carry_a_zero_line() {
    let root = indexed("no-zero-line");
    for provider in ["claude", "codex"] {
        let text = context(&hook(&root, provider, RENAME, &[])).to_string();
        // Every `path:line` and `via:line` the block renders, in either list
        // it can appear in. A row the search answered without a line number
        // has to render as a bare path.
        for anchor in anchors(&text) {
            let line = anchor
                .rsplit_once(':')
                .unwrap_or_else(|| panic!("no line on {anchor:?} in\n{text}"));
            assert_ne!(line.1, "0", "a zero line anchor {anchor:?} in\n{text}");
            assert!(
                line.1.chars().all(|ch| ch.is_ascii_digit()),
                "a non-numeric line {anchor:?} in\n{text}"
            );
        }
    }
}

/// The `path:line` and `via:line` tokens of a rendered brief.
fn anchors(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.split_once(": "))
        .find(|(label, _)| matches!(*label, "files" | "callers (impact d1)"))
        .map(|(_, body)| body)
        .unwrap_or_default()
        .split([' ', ';'])
        .map(str::trim)
        .filter(|token| token.contains('.') && token.contains(':'))
        .map(ToString::to_string)
        .collect()
}

#[test]
fn a_missing_graph_should_leave_the_text_evidence_and_build_nothing() {
    let root = fixture("no-graph");
    let search = pixel_command()
        .args(["search-content", "-F", "handleError", "-l", "--no-daemon"])
        .args(["--metrics", "off"])
        .arg(&*root)
        .output()
        .unwrap();
    assert!(search.status.success(), "{search:?}");
    let output = hook(&root, "claude", RENAME, &[]);
    let text = context(&output);
    assert!(
        text.contains("apps/web/page.tsx:1 — import { handleError }"),
        "{text}"
    );
    assert!(
        text.contains("packages/ui/handleError.ts:1 — export function handleError"),
        "{text}"
    );
    assert!(
        text.contains("\nunresolved: find-symbol handleError: the graph is not built\n"),
        "{text}"
    );
    assert!(!root.join(".pixel/graph.v2.db").exists());
}

/// `pixel brief "<prompt>"` is the same chain, on stdout, for harnesses
/// without a prompt-submit context channel (Pi's extension calls it).
#[test]
fn the_brief_command_should_print_the_brief_or_stay_silent() {
    let root = indexed("brief-cmd");
    let output = pixel_command()
        .current_dir(&*root)
        .args(["brief", RENAME, "--metrics", "off"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.starts_with("[PIXEL:BRIEF]"), "{text}");
    assert!(text.contains("coverage:"), "{text}");

    let silent = pixel_command()
        .current_dir(&*root)
        .args(["brief", "thanks, that looks good", "--metrics", "off"])
        .output()
        .unwrap();
    assert!(silent.status.success());
    assert!(silent.stdout.is_empty());
}

#[test]
fn the_brief_command_should_leave_an_unindexed_repository_untouched() {
    let root = fixture("brief-cmd-unindexed");
    let output = pixel_command()
        .current_dir(&*root)
        .args(["brief", RENAME, "--metrics", "off"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(!root.join(".pixel").exists());
}

fn brief_json(root: &Path, prompt: &str, env: &[(&str, &str)]) -> Value {
    let mut command = pixel_command();
    command
        .current_dir(root)
        .args(["brief", prompt, "--json", "--metrics", "off"]);
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&output.stdout)))
}

fn logged(root: &Path) -> Vec<Value> {
    std::fs::read_to_string(root.join(".pixel/brief-decisions.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn the_brief_command_should_print_its_decision_as_json_beside_the_brief() {
    let root = indexed("brief-json");
    let decision = brief_json(&root, RENAME, &[]);
    assert_eq!(decision["signal"], "strong");
    assert_eq!(decision["gate"], "unjudged");
    assert_eq!(decision["enforced"], false);
    assert_eq!(decision["typed"], RENAME);
    let text = decision["brief"].as_str().unwrap();
    assert!(text.starts_with("[PIXEL:BRIEF]"), "{text}");
    assert_eq!(decision["bytes"].as_u64().unwrap(), text.len() as u64);
    assert_eq!(decision["answered"], 4);
    assert_eq!(decision["sha256"].as_str().unwrap().len(), 64);
    // The decision is also the one line the log kept, minus the brief.
    let mut kept = decision.clone();
    kept.as_object_mut().unwrap().remove("brief");
    assert_eq!(logged(&root), [kept]);
}

#[test]
fn the_brief_command_should_say_why_a_prompt_got_no_brief() {
    let root = indexed("brief-json-declined");
    let quiet = brief_json(&root, "thanks, that looks good", &[]);
    assert_eq!(quiet["gate"], "declined");
    assert_eq!(quiet["reason"], "not_about_code");
    assert_eq!(quiet["brief"], Value::Null);
    assert_eq!(quiet["signal"], Value::Null);
    let continuation = brief_json(&root, "ok", &[]);
    assert_eq!(continuation["reason"], "continuation");
    // A prompt about nothing is a decision worth keeping; an "ok" is not.
    let kept = logged(&root);
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(kept[0]["typed"], "thanks, that looks good");

    let off = brief_json(&root, RENAME, &[("PIXEL_BRIEF", "0")]);
    assert_eq!(off["reason"], "disabled");
    let bare = fixture("brief-json-unindexed");
    let unindexed = brief_json(&bare, RENAME, &[]);
    assert_eq!(unindexed["reason"], "unindexed");
    assert!(!bare.join(".pixel").exists());
}

#[test]
fn the_decision_log_should_stay_off_when_pixel_brief_log_says_so() {
    let root = indexed("brief-json-log-off");
    let decision = brief_json(&root, RENAME, &[("PIXEL_BRIEF_LOG", "0")]);
    assert_eq!(decision["gate"], "unjudged");
    assert!(decision["brief"].is_string());
    assert!(!root.join(".pixel/brief-decisions.jsonl").exists());
}
