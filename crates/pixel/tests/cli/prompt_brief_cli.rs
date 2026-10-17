// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The evidence brief rides the registered `task-event` prompt hook: a code
//! prompt in an indexed repository gets `[PIXEL:BRIEF]` as additional context,
//! every other case gets nothing extra, and no case builds or refreshes an index.

use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

use crate::support::{Scratch, daemons_serving, git, pixel_command};

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
    // A tree of its own: two fixtures with the same tree committed in the same
    // second share a commit, so the shared base-shard cache answers both and a
    // test that asserts its shard did not move reads another test's write.
    std::fs::write(root.join("data/fixture.txt"), format!("{tag}\n")).unwrap();
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
    run_hook(root, provider, "prompt-submit", &payload, |command| {
        command.env_remove("PIXEL_BRIEF");
        // These tests are about the packet; the region pack has its own.
        command.env("PIXEL_BRIEF_PACK", "0");
        for (key, value) in env {
            command.env(key, value);
        }
    })
}

/// Run `pixel run-hook task-event` for `event` with `payload` on stdin;
/// `configure` adjusts the environment of the hook after the defaults.
fn run_hook(
    root: &Path,
    provider: &str,
    event: &str,
    payload: &Value,
    configure: impl FnOnce(&mut Command),
) -> Value {
    let mut command = pixel_command();
    command
        .current_dir(root)
        .env("PIXEL_METRICS", "0")
        .args(["run-hook", "task-event", "--provider", provider])
        .args(["--event", event])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure(&mut command);
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

#[test]
fn a_delivered_brief_carries_the_metrics_line_as_a_system_message() {
    let root = indexed("metrics-line");
    for provider in ["claude", "codex"] {
        let output = hook(&root, provider, RENAME, &[("PIXEL_METRICS", "")]);
        // env_remove cannot be passed through `env`, so clear the veto by
        // pointing it at a non-"0" value.
        let message = output["systemMessage"]
            .as_str()
            .unwrap_or_else(|| panic!("no systemMessage in {output}"));
        assert!(
            message.starts_with("🟩 pixel prompt-submit ❀ "),
            "{provider}: {message}"
        );
    }

    let silenced = hook(&root, "codex", RENAME, &[("PIXEL_METRICS", "0")]);
    assert!(
        silenced.get("systemMessage").is_none(),
        "the veto silences the line: {silenced}"
    );
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
                "read: packages/ui/handleError.ts:1-3 — handleError",
                "also: apps/web/page.tsx:2-4 — Page",
                "Read these ranges (in parallel, one turn) before any search. If they don't answer, search as usual.",
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
/// without a prompt-submit context channel (OpenCode's plugin calls it).
#[test]
fn the_brief_command_should_print_the_brief_or_stay_silent() {
    let root = indexed("brief-cmd");
    let output = pixel_command()
        .current_dir(&*root)
        .env("PIXEL_BRIEF_PACK", "0")
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
        .env("PIXEL_BRIEF_PACK", "0")
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

// A hook that may start a daemon: the three switches (brief, auto-start, an
// index) and the two moments (a session starting, a prompt).

fn session_start(root: &Path, configure: impl FnOnce(&mut Command)) -> Value {
    let payload = json!({
        "session_id": "brief-session",
        "cwd": root,
        "hook_event_name": "SessionStart",
        "source": "startup",
    });
    run_hook(root, "claude", "session-start", &payload, configure)
}

/// The environment of a hook allowed to start a daemon, in a shard cache of
/// its own so nothing is written under the developer's cache.
fn auto_start(command: &mut Command, cache: &Path) {
    command
        .env("PIXEL_DAEMON_AUTO_START", "1")
        .env_remove("PIXEL_BRIEF")
        .env("XDG_CACHE_HOME", cache);
}

fn daemon_running(root: &Path) -> bool {
    let output = pixel_command()
        .args(["daemon", "status"])
        .arg(root)
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).starts_with("daemon running")
}

fn wait_for_daemon(root: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if daemon_running(root) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Stop the daemon and wait for the start process that launched it to be
/// gone too, so the fixture can drop without a leak.
fn stop_daemon(root: &Path) {
    let _ = pixel_command()
        .args(["daemon", "stop"])
        .arg(root)
        .output()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && !daemons_serving(root).is_empty() {
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_session_start_should_start_the_daemon_of_an_indexed_repository() {
    let root = indexed("warm");
    let cache = Scratch::for_test("prompt-brief-cache", "warm");
    assert!(!daemon_running(&root), "no daemon before the session");
    session_start(&root, |command| auto_start(command, &cache));
    let up = wait_for_daemon(&root);
    // Stop before asserting: a failed assertion never leaves a daemon.
    stop_daemon(&root);
    assert!(up, "the session start started no daemon");
}

/// A session start with `configure` applied, which must start no daemon.
fn assert_session_start_starts_no_daemon(
    name: &str,
    root: &Path,
    configure: impl FnOnce(&mut Command),
) {
    session_start(root, configure);
    std::thread::sleep(Duration::from_millis(500));
    let started = daemon_running(root) || !daemons_serving(root).is_empty();
    if started {
        stop_daemon(root);
    }
    assert!(!started, "{name}: a daemon was started");
}

#[test]
fn a_session_start_should_start_no_daemon_when_any_switch_is_off_or_there_is_no_index() {
    let indexed_root = indexed("warm-off");
    let bare = fixture("warm-unindexed");
    let cache = Scratch::for_test("prompt-brief-cache", "warm-off");
    assert_session_start_starts_no_daemon(
        "auto-start off by the environment",
        &indexed_root,
        |command| {
            auto_start(command, &cache);
            command.env("PIXEL_DAEMON_AUTO_START", "0");
        },
    );
    assert_session_start_starts_no_daemon("brief off by the environment", &indexed_root, |c| {
        auto_start(c, &cache);
        c.env("PIXEL_BRIEF", "0");
    });
    assert_session_start_starts_no_daemon("no index", &bare, |c| auto_start(c, &cache));
    // Last: the setting stays in the repository.
    std::fs::write(
        indexed_root.join(".pixel/config.yaml"),
        "daemon_auto_start: false\n",
    )
    .unwrap();
    assert_session_start_starts_no_daemon(
        "auto-start off by the repository",
        &indexed_root,
        |command| {
            auto_start(command, &cache);
            command.env_remove("PIXEL_DAEMON_AUTO_START");
        },
    );
}

#[test]
fn a_prompt_should_brief_and_start_a_daemon_without_assuming_startup_order() {
    let root = indexed("prompt-start");
    let cache = Scratch::for_test("prompt-brief-cache", "prompt-start");
    let payload = |prompt: &str| {
        json!({
            "session_id": "brief-session",
            "prompt": prompt,
            "cwd": &*root,
            "hook_event_name": "UserPromptSubmit",
        })
    };
    let first = run_hook(
        &root,
        "claude",
        "prompt-submit",
        &payload(RENAME),
        |command| {
            auto_start(command, &cache);
        },
    );
    let up = wait_for_daemon(&root);
    let second = up.then(|| {
        run_hook(
            &root,
            "claude",
            "prompt-submit",
            &payload(RENAME),
            |command| {
                auto_start(command, &cache);
            },
        )
    });
    stop_daemon(&root);
    assert!(
        context(&first).starts_with("[PIXEL:BRIEF]"),
        "the first prompt is briefed while the daemon starts: {first}"
    );
    assert!(up, "the prompt started no daemon");
    assert!(
        context(&second.unwrap()).starts_with("[PIXEL:BRIEF]"),
        "the second prompt is briefed by the daemon"
    );
    let records = logged(&root);
    assert_eq!(records.len(), 2, "{records:?}");
    // Autostart and the evidence worker run concurrently. A fast daemon may
    // answer the first worker's probe; neither route may be assumed from the
    // launch outcome. The unit test with a blocked starter separately pins
    // the contract that starting a daemon cannot hold up the brief.
    assert_eq!(records[0]["daemon"], "launched", "{}", records[0]);
    assert!(
        matches!(records[0]["route"].as_str(), Some("local" | "daemon")),
        "{}",
        records[0]
    );
    assert_eq!(
        (records[1]["route"].as_str(), records[1]["daemon"].as_str()),
        (Some("daemon"), Some("running")),
        "{}",
        records[1]
    );
}

#[test]
fn a_prompt_should_start_no_daemon_when_auto_start_is_off() {
    let root = indexed("prompt-no-start");
    let output = hook(&root, "claude", RENAME, &[("PIXEL_DAEMON_AUTO_START", "0")]);
    assert!(context(&output).starts_with("[PIXEL:BRIEF]"), "{output}");
    std::thread::sleep(Duration::from_millis(500));
    let started = daemon_running(&root) || !daemons_serving(&root).is_empty();
    if started {
        stop_daemon(&root);
    }
    assert!(!started, "a daemon was started");
    let records = logged(&root);
    assert_eq!(records[0]["route"], "local");
    assert_eq!(records[0]["daemon"], "skipped: auto-start off");
}

// A host caps what one hook injects (10,000 characters for Claude Code, 2,500
// estimated tokens for Codex), so a larger brief is delivered by several hooks:
// `--part K/N`. `PIXEL_BRIEF_TEST_PAD_PARTS` (debug builds) forces the brief
// oversized, each part ending in a marker line.

#[cfg(debug_assertions)]
const PADDED: (&str, &str) = ("PIXEL_BRIEF_TEST_PAD_PARTS", "3");

fn part_payload(root: &Path) -> Value {
    json!({
        "session_id": "brief-session",
        "prompt": RENAME,
        "cwd": root,
        "hook_event_name": "UserPromptSubmit",
    })
}

/// Spawn `run-hook task-event --event prompt-submit --part <part>` and return
/// the handle of its output, started now.
fn spawn_part(
    root: &Path,
    provider: &str,
    part: &str,
    payload: &Value,
    env: &[(&str, &str)],
) -> std::thread::JoinHandle<std::process::Output> {
    let mut command = pixel_command();
    command
        .current_dir(root)
        .env("PIXEL_METRICS", "0")
        .env_remove("PIXEL_BRIEF")
        .args(["run-hook", "task-event", "--provider", provider])
        .args(["--event", "prompt-submit", "--part", part])
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
    std::thread::spawn(move || child.wait_with_output().unwrap())
}

fn printed(output: std::process::Output) -> Option<Value> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    (!text.trim().is_empty()).then(|| serde_json::from_str(&text).unwrap())
}

#[test]
#[cfg(debug_assertions)]
fn the_three_parts_of_an_oversized_brief_concatenate_to_the_whole_one() {
    // The packet, not the pack: the pack's size follows the part count, and
    // this test is about cutting one brief.
    const PADDED_PACKET: [(&str, &str); 2] = [PADDED, ("PIXEL_BRIEF_PACK", "0")];
    let root = indexed("parts");
    let payload = part_payload(&root);
    // The whole brief, from the hook with no `--part`: oversized.
    let whole = hook(&root, "claude", RENAME, &PADDED_PACKET);
    let whole = context(&whole).to_owned();
    assert!(whole.starts_with("[PIXEL:BRIEF]"), "{}", &whole[..200]);
    assert!(
        whole.chars().count() > 3 * 9_000,
        "{}",
        whole.chars().count()
    );

    // Parts 3 and 2 start first and wait for part 1.
    let three = spawn_part(&root, "claude", "3/3", &payload, &PADDED_PACKET);
    let two = spawn_part(&root, "claude", "2/3", &payload, &PADDED_PACKET);
    std::thread::sleep(Duration::from_millis(200));
    let one = spawn_part(&root, "claude", "1/3", &payload, &PADDED_PACKET);
    let parts: Vec<Value> = [one, two, three]
        .into_iter()
        .map(|handle| printed(handle.join().unwrap()).expect("every part prints"))
        .collect();

    let slices: Vec<&str> = parts.iter().map(context).collect();
    for (k, slice) in (1..=3).zip(&slices) {
        assert!(
            slice.chars().count() <= 9_800,
            "part {k}: {}",
            slice.chars().count()
        );
        assert!(
            slice.ends_with(&format!("PIXEL-PART-MARKER-{k}-OF-3\n")),
            "part {k} ends with its marker: {:?}",
            &slice[slice.len().saturating_sub(60)..]
        );
        assert_eq!(
            parts[usize::try_from(k - 1).unwrap()]["hookSpecificOutput"]["hookEventName"],
            "UserPromptSubmit"
        );
    }
    assert_eq!(slices.concat(), whole, "the parts together are the brief");
    assert!(
        slices[0].starts_with("[PIXEL:BRIEF]") && !slices[0].contains("--part"),
        "part 1 is today's brief and says nothing about parts"
    );
    assert!(!slices[1].contains("[PIXEL:BRIEF]") && !slices[2].contains("[PIXEL:BRIEF]"));

    // The stored brief: owner-only, under .pixel, and the whole brief; part 1
    // left its marker beside it for the others.
    let dir = root.join(".pixel/brief-parts");
    let mut stored: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
    stored.sort_by_key(std::fs::DirEntry::file_name);
    let names: Vec<String> = stored
        .iter()
        .map(|entry| {
            entry
                .path()
                .extension()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(names, vec!["done", "txt"], "{stored:?}");
    {
        use std::os::unix::fs::PermissionsExt;
        for entry in &stored {
            assert_eq!(
                entry.metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    assert_eq!(std::fs::read_to_string(stored[1].path()).unwrap(), whole);
}

#[test]
#[cfg(debug_assertions)]
fn codex_gets_its_brief_in_parts_of_at_most_9800_bytes() {
    let root = indexed("parts-codex");
    let payload = part_payload(&root);
    let env = [PADDED];
    let handles: Vec<_> = ["3/3", "2/3", "1/3"]
        .into_iter()
        .map(|part| spawn_part(&root, "codex", part, &payload, &env))
        .collect();
    let mut slices: Vec<String> = handles
        .into_iter()
        .map(|handle| {
            let output = printed(handle.join().unwrap()).expect("every part prints");
            context(&output).to_owned()
        })
        .collect();
    slices.reverse();
    for (k, slice) in (1..=3).zip(&slices) {
        assert!(slice.len() <= 9_800, "part {k}: {} bytes", slice.len());
        assert!(slice.ends_with(&format!("PIXEL-PART-MARKER-{k}-OF-3\n")));
    }
    assert!(slices[0].starts_with("[PIXEL:BRIEF]"));
}

#[test]
fn a_part_that_never_gets_the_first_one_prints_nothing_and_exits_cleanly() {
    let root = indexed("parts-timeout");
    let started = Instant::now();
    let output = spawn_part(&root, "claude", "2/3", &part_payload(&root), &[])
        .join()
        .unwrap();
    let waited = started.elapsed();
    assert!(output.status.success());
    assert!(
        output.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        waited >= Duration::from_millis(1_500),
        "it waited for part 1: {waited:?}"
    );
    // The later-part bound plus stdin wait stays below the host's 10s hook deadline.
    assert!(waited < Duration::from_secs(10), "and gave up: {waited:?}");
    // It ran no ledger and stored nothing.
    assert!(!root.join(".pixel/brief-parts").exists());
    assert!(!root.join(".pixel/tasks").exists());
}

#[test]
fn parts_of_a_prompt_with_no_brief_print_nothing_much_and_do_not_wait() {
    let root = indexed("parts-none");
    let payload = part_payload(&root);
    // PIXEL_BRIEF=0: part 1 has no brief; it prints `{}` and stores an empty
    // one, so part 2 answers at once instead of waiting out its bound.
    let off = [("PIXEL_BRIEF", "0")];
    let started = Instant::now();
    let one = spawn_part(&root, "claude", "1/3", &payload, &off)
        .join()
        .unwrap();
    let one = printed(one).expect("part 1 prints the ledger's answer");
    assert_eq!(one, json!({}));
    let two = spawn_part(&root, "claude", "2/3", &payload, &off)
        .join()
        .unwrap();
    assert_eq!(printed(two), Some(json!({})));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[test]
#[cfg(debug_assertions)]
fn only_the_first_part_runs_the_task_ledger() {
    let root = indexed("parts-ledger");
    let payload = part_payload(&root);
    let env = [PADDED];
    let handles: Vec<_> = ["2/3", "3/3", "1/3"]
        .into_iter()
        .map(|part| spawn_part(&root, "claude", part, &payload, &env))
        .collect();
    for handle in handles {
        assert!(handle.join().unwrap().status.success());
    }
    // The ledger saw one prompt: the whole-brief hook on the same payload
    // leaves exactly as many task events behind as one part-1 run did.
    let after_parts = task_event_files(&root);
    assert!(after_parts > 0, "the first part reached the ledger");
    let other = indexed("parts-ledger-whole");
    let _ = hook(&other, "claude", RENAME, &[PADDED]);
    assert_eq!(
        after_parts,
        task_event_files(&other),
        "one prompt, one ledger entry"
    );
}

/// How many files the task ledger keeps under `.pixel/tasks`, by relative
/// shape: a prompt that reaches the ledger twice leaves more.
fn task_event_files(root: &Path) -> usize {
    fn count(dir: &Path) -> usize {
        std::fs::read_dir(dir).map_or(0, |entries| {
            entries
                .flatten()
                .map(|entry| {
                    let path = entry.path();
                    if path.is_dir() { count(&path) } else { 1 }
                })
                .sum()
        })
    }
    count(&root.join(".pixel/tasks"))
}

#[test]
fn a_hook_with_arguments_it_cannot_parse_says_so_and_exits_zero() {
    // Claude Code blocks the prompt when a UserPromptSubmit hook exits 2, the
    // code clap exits with: a hook fails open whatever it was given.
    let root = fixture("parts-bad");
    for bad in ["0/3", "4/3", "1/9", "2", "x/y"] {
        let output = spawn_part(&root, "claude", bad, &part_payload(&root), &[])
            .join()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{bad}");
        assert!(output.stdout.is_empty(), "{bad}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("K/N"),
            "{bad}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    for args in [
        &[
            "run-hook",
            "task-event",
            "--provider",
            "claude",
            "--event",
            "prompt-submit",
            "--future-flag",
        ][..],
        &[
            "run-hook",
            "task-event",
            "--provider",
            "claude",
            "--event",
            "prompt-submit",
            "--part",
        ][..],
        &[
            "run-hook",
            "task-event",
            "--provider",
            "nobody",
            "--event",
            "prompt-submit",
        ][..],
        &["run-hook", "task-event"][..],
    ] {
        let output = pixel_command()
            .current_dir(&*root)
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(
            !output.stderr.is_empty(),
            "{args:?}: the reason goes to stderr"
        );
    }
    // Only hooks fail open: another command still exits with clap's code.
    let output = pixel_command()
        .args(["status", "--no-such-flag"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    // And asking a hook for help is not an error.
    let help = pixel_command()
        .args(["run-hook", "--help"])
        .output()
        .unwrap();
    assert_eq!(help.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&help.stdout).contains("task-event"));
}

#[test]
fn a_part_on_another_event_is_ignored() {
    let root = indexed("parts-other-event");
    let payload = json!({"session_id": "brief-session", "cwd": &*root, "hook_event_name": "Stop"});
    let plain = run_hook(&root, "claude", "stop", &payload, |_| {});
    let with_part = run_hook(&root, "claude", "stop", &payload, |command| {
        command.args(["--part", "2/3"]);
    });
    assert_eq!(plain, with_part);
}
