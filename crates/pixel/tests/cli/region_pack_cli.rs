// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The region pack through the real binary: a prompt that names a symbol gets
//! the definition, its usage sites in their functions and its tests as
//! line-numbered source under one directive, the text search stands in for a
//! graph that went stale, and `PIXEL_BRIEF_PACK=0` brings the old packet back.

use std::path::Path;

use std::io::Write;
use std::process::Stdio;

use serde_json::{Value, json};

use crate::support::{Scratch, git, pixel_command};

const PROMPT: &str = "what value is MAX_RETRIES set to and who uses it?";
const DIRECTIVE: &str =
    "Answer from this context. Use a tool only if something you need is not shown above.";
const SKILL_LINE: &str = "Any search beyond these lines: use the `pixel` skill (`pixel find-code`, `pixel who-calls`, `pixel search-content`) before grep, rg, find or ls.";

fn fixture(tag: &str) -> Scratch {
    fixture_with(tag, 0)
}

/// The fixture, and `callers` more files that each use `MAX_RETRIES` in a
/// thirty-line function.
fn fixture_with(tag: &str, callers: usize) -> Scratch {
    let root = Scratch::for_test("region-pack-cli", tag);
    std::fs::create_dir_all(root.join("src")).unwrap();
    for number in 0..callers {
        let mut body = vec![format!("pub fn caller_{number}() -> u32 {{")];
        body.extend((1..=30).map(|step| {
            if step == 15 {
                "    let limit = crate::limits::MAX_RETRIES;".to_string()
            } else {
                format!("    let step_{step} = {step} * {number}; // padding for the window")
            }
        }));
        body.push("}".to_string());
        body.push(String::new());
        std::fs::write(
            root.join(format!("src/caller_{number}.rs")),
            body.join("\n"),
        )
        .unwrap();
    }
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::write(
        root.join("src/limits.rs"),
        [
            "/// How many times a failed request is sent again.",
            "pub const MAX_RETRIES: u32 = 42;",
            "",
            "pub fn backoff(attempt: u32) -> u32 {",
            "    attempt.min(MAX_RETRIES) * 2",
            "}",
            "",
        ]
        .join("\n"),
    )
    .unwrap();
    std::fs::write(
        root.join("src/client.rs"),
        [
            "use crate::limits::{MAX_RETRIES, backoff};",
            "",
            "pub fn send_with_retries() -> u32 {",
            "    let mut waited = 0;",
            "    for attempt in 0..MAX_RETRIES {",
            "        waited += backoff(attempt);",
            "    }",
            "    waited",
            "}",
            "",
        ]
        .join("\n"),
    )
    .unwrap();
    std::fs::write(
        root.join("tests/limits_test.rs"),
        [
            "#[test]",
            "fn retries_stop_at_the_limit() {",
            "    assert_eq!(MAX_RETRIES, 42);",
            "}",
            "",
        ]
        .join("\n"),
    )
    .unwrap();
    // A tree of its own: two fixtures with the same tree committed in the same
    // second share a commit, and the base-shard cache would answer both.
    std::fs::write(root.join("fixture.txt"), format!("{tag}\n")).unwrap();
    std::fs::write(root.join(".gitignore"), ".pixel/\n").unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "fixture"]);
    let search = pixel_command()
        .args(["search-content", "-F", "MAX_RETRIES", "-l", "--no-daemon"])
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

fn brief(root: &Path, env: &[(&str, &str)]) -> Value {
    let mut command = pixel_command();
    command
        .current_dir(root)
        .env_remove("PIXEL_BRIEF")
        .env_remove("PIXEL_BRIEF_PACK")
        .env_remove("PIXEL_BRIEF_PARTS")
        // These tests are about the pack, not about what the probes make of it.
        .env("PIXEL_BRIEF_PROBES", "0")
        .args(["brief", PROMPT, "--json", "--metrics", "off"]);
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout).unwrap()
}

fn text(decision: &Value) -> &str {
    decision["brief"]
        .as_str()
        .unwrap_or_else(|| panic!("no brief in {decision}"))
}

#[test]
fn a_prompt_naming_a_symbol_should_get_its_definition_usage_and_tests_as_source() {
    let root = fixture("symbol");
    let decision = brief(&root, &[]);
    let text = text(&decision);
    assert!(
        text.starts_with(
            "[PIXEL:BRIEF]\nregion pack — searched MAX_RETRIES in the symbol graph and text index"
        ),
        "{text}"
    );
    // The definition keeps its doc comment and value; its usage remains a
    // separate block instead of widening the definition to neighbouring lines.
    assert!(
        text.contains("### src/limits.rs:1-2 — MAX_RETRIES definition (const)\n1| /// How many times a failed request is sent again.\n2| pub const MAX_RETRIES: u32 = 42;"),
        "{text}"
    );
    assert!(
        text.contains("### src/limits.rs:4-6 — MAX_RETRIES usage in backoff\n4| pub fn backoff(attempt: u32) -> u32 {\n5|     attempt.min(MAX_RETRIES) * 2\n6| }"),
        "{text}"
    );
    assert_eq!(
        text.matches("attempt.min(MAX_RETRIES)").count(),
        1,
        "{text}"
    );
    // A usage site is shown inside the function that holds it.
    assert!(text.contains("usage in send_with_retries"), "{text}");
    assert!(
        text.contains("5|     for attempt in 0..MAX_RETRIES {"),
        "{text}"
    );
    // The test is named, with its first assertion.
    assert!(
        text.contains(
            "tests/limits_test.rs:2 retries_stop_at_the_limit — assert_eq!(MAX_RETRIES, 42);"
        ),
        "{text}"
    );
    // Every brief ends on the skill line, whatever its own last line says.
    assert!(
        text.ends_with(&format!("{DIRECTIVE}\n{SKILL_LINE}")),
        "{text}"
    );
    for old in ["\nfiles:", "\nread:", "\ncoverage:", "\nkind:"] {
        assert!(!text.contains(old), "{old}: {text}");
    }
    assert_eq!(decision["pack_bytes"], text.len() - SKILL_LINE.len() - 1);
    assert_eq!(decision["pack_parts"], 1);
    assert_eq!(decision["pack_targets"], 1);
    assert_eq!(decision["fallback"], false);
}

#[test]
fn a_stale_graph_should_not_take_the_definition_away() {
    let root = fixture("stale");
    // An edit after the graph was built: the graph can no longer be trusted.
    std::fs::write(
        root.join("src/client.rs"),
        "pub fn send_with_retries() -> u32 {\n    crate::limits::MAX_RETRIES\n}\n",
    )
    .unwrap();
    let decision = brief(&root, &[]);
    let text = text(&decision);
    assert!(text.contains("graph stale: text fallback"), "{text}");
    assert!(
        text.contains("2| pub const MAX_RETRIES: u32 = 42;"),
        "{text}"
    );
    assert!(text.contains("2|     crate::limits::MAX_RETRIES"), "{text}");
    assert!(!text.contains("find-symbol"), "{text}");
    assert_eq!(decision["fallback"], true);
}

#[test]
fn the_kill_switch_should_bring_the_old_packet_back() {
    let root = fixture("kill");
    for off in ["0", "false", "off"] {
        let decision = brief(&root, &[("PIXEL_BRIEF_PACK", off)]);
        let text = text(&decision);
        assert!(text.contains("\ndefined: "), "{off}: {text}");
        assert!(!text.contains("region pack"), "{off}: {text}");
        assert_eq!(decision["pack_bytes"], 0, "{off}");
        assert_eq!(decision["pack_parts"], 0, "{off}");
    }
}

#[test]
fn a_pack_should_grow_with_the_parts_the_host_delivers() {
    let root = fixture("parts");
    let one = brief(&root, &[("PIXEL_BRIEF_PARTS", "1")]);
    let three = brief(&root, &[("PIXEL_BRIEF_PARTS", "3")]);
    assert_eq!(one["pack_parts"], 1);
    assert!(three["pack_bytes"].as_u64().unwrap() >= one["pack_bytes"].as_u64().unwrap());
    assert!(three["pack_bytes"].as_u64().unwrap() <= 3 * 9_800);
}

#[test]
fn the_brief_command_should_keep_a_pack_under_what_pi_and_opencode_accept() {
    let root = fixture_with("command-cap", 4);
    // The `brief` command: the 4 000 bytes the extensions that run it accept.
    let command = brief(&root, &[]);
    let command_text = text(&command);
    assert!(command_text.len() <= 3_900, "{}", command_text.len());
    assert!(
        command_text.starts_with("[PIXEL:BRIEF]\nregion pack"),
        "{command_text}"
    );
    assert!(
        command_text.contains("MAX_RETRIES definition"),
        "{command_text}"
    );
    // A host that says how many parts it takes gets a pack for them.
    let one = brief(&root, &[("PIXEL_BRIEF_PARTS", "1")]);
    let one_len = text(&one).len();
    assert!(one_len > 3_900 && one_len <= 9_800, "{one_len}");
    assert_eq!(one["pack_parts"], 1);
}

/// Run `run-hook task-event` for a prompt-submit, with `--part` when given,
/// and return the context it prints (empty when it prints none).
fn hook_context(root: &Path, part: Option<&str>) -> String {
    let mut command = pixel_command();
    command
        .current_dir(root)
        .env("PIXEL_METRICS", "0")
        .env_remove("PIXEL_BRIEF")
        .env_remove("PIXEL_BRIEF_PACK")
        .env_remove("PIXEL_BRIEF_PARTS")
        // The same prompt, asked again in one session, would be withheld.
        .env("PIXEL_BRIEF_DEDUPE", "0")
        .env("PIXEL_BRIEF_PROBES", "0")
        .args(["run-hook", "task-event", "--provider", "claude"])
        .args(["--event", "prompt-submit"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    if let Some(part) = part {
        command.args(["--part", part]);
    }
    let mut child = command.spawn().unwrap();
    let payload = json!({
        "session_id": "pack-session",
        "prompt": PROMPT,
        "cwd": root,
        "hook_event_name": "UserPromptSubmit",
    });
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let printed: Value = serde_json::from_slice(&output.stdout).unwrap();
    printed["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[test]
fn the_pack_should_fill_as_many_parts_as_the_hooks_that_deliver_it() {
    let root = fixture_with("hook-parts", 8);
    // One hook (`brief_parts: 1`, or a host with a single hook): one part.
    let single = hook_context(&root, None);
    assert!(single.starts_with("[PIXEL:BRIEF]\nregion pack"), "{single}");
    assert!(
        single.chars().count() <= 9_800,
        "{}",
        single.chars().count()
    );
    // Two hooks: part 1 computes a pack for two, part 2 prints the rest.
    let (one, two) = std::thread::scope(|scope| {
        let two = scope.spawn(|| hook_context(&root, Some("2/2")));
        std::thread::sleep(std::time::Duration::from_millis(150));
        (hook_context(&root, Some("1/2")), two.join().unwrap())
    });
    assert!(one.starts_with("[PIXEL:BRIEF]\nregion pack"), "{one}");
    assert!(!two.is_empty(), "part 2 prints the rest of the pack");
    for (k, part) in [&one, &two].into_iter().enumerate() {
        assert!(part.chars().count() <= 9_800, "part {}", k + 1);
    }
    assert!(
        one.chars().count() + two.chars().count() > 9_800,
        "a pack for two parts is larger than one part"
    );
}
