// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

use serde_json::Value;

use crate::support::{action_log, project, stderr, stdout};

fn json(output: &std::process::Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("json on stdout")
}

#[test]
fn find_symbol_prints_the_place_and_the_signature() {
    let fixture = project();
    let output = fixture.run(&["find-symbol", "run"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "src/a.rs:1 function run\n    pub fn run(x: u32) -> u32 {\n"
    );
}

#[test]
fn who_calls_lists_the_proved_caller_and_callees_work_the_other_way() {
    let fixture = project();
    let callers = stdout(&fixture.run(&["who-calls", "run"]));
    assert!(
        callers.starts_with("run (src/a.rs:1) callers: 1\n"),
        "{callers}"
    );
    assert!(callers.contains("  src/b.rs:4 go\n"), "{callers}");
    let callees = stdout(&fixture.run(&["who-calls", "go", "--callees"]));
    assert!(
        callees.contains("  src/a.rs:1 run (called at line 4)\n"),
        "{callees}"
    );
}

#[test]
fn impact_names_the_depths_and_the_covering_test() {
    let fixture = project();
    let text = stdout(&fixture.run(&["impact", "run"]));
    assert!(
        text.starts_with("run (src/a.rs:1) upstream, risk LOW\n"),
        "{text}"
    );
    assert!(
        text.contains("depth 1 (will break): 1\n  src/b.rs:3 go\n"),
        "{text}"
    );
    assert!(text.contains("tests:\n  tests/it.rs\n"), "{text}");
}

#[test]
fn json_prints_the_result_with_the_envelope_fields_folded_in() {
    let fixture = project();
    let output = fixture.run(&["find-symbol", "run", "--json"]);
    let value = json(&output);
    assert_eq!(value["symbols"][0]["file"], "src/a.rs");
    assert_eq!(value["epistemics"]["closed_world"], false);
    assert_eq!(value["epistemics"]["basis"], "graph");
    assert!(value["snapshot"]["token"].is_string());
}

#[test]
fn a_name_that_is_nobody_fails_with_not_found_and_a_json_failure() {
    let fixture = project();
    let output = fixture.run(&["find-symbol", "nobody", "--json"]);
    assert!(!output.status.success());
    let value = json(&output);
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["message"], "no symbol named \"nobody\"");
    assert!(stderr(&output).contains("pixel: no symbol named \"nobody\""));
}

#[test]
fn search_content_prints_ripgrep_lines_and_filters_by_type() {
    let fixture = project();
    let hits = stdout(&fixture.run(&["search-content", "fn run", "-t", "rust"]));
    assert_eq!(hits, "src/a.rs:1:pub fn run(x: u32) -> u32 {\n");
    let none = fixture.run(&["search-content", "fn run", "-t", "md"]);
    assert_eq!(stdout(&none), "");
    let files = stdout(&fixture.run(&["search-content", "run", "-l"]));
    assert_eq!(files, "src/a.rs\nsrc/b.rs\n");
    let below = stdout(&fixture.run(&["search-content", "run", "src/b.rs"]));
    assert_eq!(
        below,
        "src/b.rs:1:use crate::a::run;\nsrc/b.rs:4:    run(1)\n"
    );
}

#[test]
fn what_changed_and_review_follow_an_edit_to_a_signature() {
    let fixture = project();
    fixture.write(
        "src/a.rs",
        "pub fn run(x: u32, y: u32) -> u32 {\n    x + y\n}\n",
    );
    let changed = stdout(&fixture.run(&["what-changed", "--tests"]));
    assert_eq!(
        changed,
        "src/a.rs:1 function run [signature]\n    test tests/it.rs\n"
    );
    let review = fixture.run(&["review", "--fail-on", "suggestion"]);
    let text = stdout(&review);
    assert!(text.contains("producer-reader-divergence src/a.rs:1: run changed; go at src/b.rs:4 reads it and was not changed"), "{text}");
    assert_eq!(
        review.status.code(),
        Some(1),
        "a suggestion fails --fail-on suggestion"
    );
    let lenient = fixture.run(&["review", "--fail-on", "blocker"]);
    assert!(lenient.status.success());
}

#[test]
fn call_path_and_list_signatures_read_the_graph_in_process() {
    let fixture = project();
    let path = stdout(&fixture.run(&["call-path", "t", "run"]));
    assert!(
        path.contains("→ go (src/b.rs:3) called at line 4"),
        "{path}"
    );
    assert!(
        path.contains("→ run (src/a.rs:1) called at line 4"),
        "{path}"
    );
    let none = stdout(&fixture.run(&["call-path", "run", "t"]));
    assert!(
        none.starts_with("no chain of resolved calls from run to t"),
        "{none}"
    );
    let signatures = stdout(&fixture.run(&["list-signatures", "src/b.rs"]));
    assert_eq!(
        signatures,
        "src/b.rs\n    3 function   pub fn go() -> u32 {\n"
    );
}

#[test]
fn rename_is_a_dry_run_until_apply_and_then_the_files_change() {
    let fixture = project();
    let dry = fixture.run(&["rename", "run", "execute"]);
    let text = stdout(&dry);
    assert!(
        text.contains("src/a.rs:1 definition: run → execute"),
        "{text}"
    );
    assert!(text.contains("src/b.rs:1 import: run → execute"), "{text}");
    assert!(text.contains("src/b.rs:4 call: run → execute"), "{text}");
    assert!(stderr(&dry).contains("dry run: pass --apply to write"));
    assert!(
        std::fs::read_to_string(fixture.root().join("src/a.rs"))
            .unwrap()
            .contains("fn run")
    );
    let applied = fixture.run(&["rename", "run", "execute", "--apply"]);
    assert!(applied.status.success(), "{}", stderr(&applied));
    let a = std::fs::read_to_string(fixture.root().join("src/a.rs")).unwrap();
    assert!(a.contains("pub fn execute(x: u32)"), "{a}");
    let after = stdout(&fixture.run(&["who-calls", "execute"]));
    assert!(after.contains("src/b.rs:4 go"), "{after}");
}

#[test]
fn repo_map_lists_the_files_and_status_reports_the_stores() {
    let fixture = project();
    let map = stdout(&fixture.run(&["repo-map"]));
    assert!(map.contains("src/a.rs\n  function run\n"), "{map}");
    let status = stdout(&fixture.run(&["status"]));
    assert!(status.contains("graph:   5 files"), "{status}");
    assert!(status.contains("history: not built"), "{status}");
}

#[test]
fn history_is_built_by_the_first_query_and_a_read_only_one_says_so_before() {
    let fixture = project();
    let before = stdout(&fixture.run(&["search-history", "one", "--read-only"]));
    assert!(before.starts_with("history unavailable"), "{before}");
    assert!(!fixture.root().join(".pixel/history.db").exists());
    let hits = stdout(&fixture.run(&["search-history", "one"]));
    assert!(hits.contains(" one [message]"), "{hits}");
}

fn journal(root: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(root.join(".pixel/journal.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

#[test]
fn brief_prints_the_block_for_a_prompt_about_code_and_journals_the_decision() {
    let fixture = project();
    let output = fixture.run(&["brief", "who calls `run` and what breaks if I rename it"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.starts_with("[PIXEL:BRIEF]\n"), "{text}");
    assert!(
        text.contains("### src/a.rs:1-3 — run definition (function)"),
        "{text}"
    );
    assert!(text.ends_with('\n'));
    let lines = journal(&fixture.root());
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["t"], "brief");
    assert_eq!(lines[0]["gate"], "unjudged");
    assert!(
        lines[0]["steps"].as_array().is_some_and(|steps| steps
            .iter()
            .any(|step| step["name"] == "cand-j-callers" && step["ms"].is_u64())),
        "the steps that ran are timed: {}",
        lines[0]["steps"]
    );
    assert_eq!(lines[0]["provenance"]["source"], "brief");
    assert_eq!(
        lines[0]["typed"],
        "who calls `run` and what breaks if I rename it"
    );
}

#[test]
fn brief_prints_nothing_for_a_prompt_that_asks_nothing_about_code_and_says_why() {
    let fixture = project();
    let output = fixture.run(&["brief", "thanks, that works"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
    assert!(
        stderr(&output).contains("no brief: not about code"),
        "{}",
        stderr(&output)
    );
    let lines = journal(&fixture.root());
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["gate"], "declined");
    assert_eq!(lines[0]["reason"], "not_about_code");
}

#[test]
fn brief_refuses_a_step_that_does_not_exist_before_any_work() {
    let fixture = project();
    let output = fixture.run(&["brief", "who calls `run`", "--without", "no-such-step"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
    assert!(
        stderr(&output).contains(
            "unknown brief step \"no-such-step\"; known: workspace-overview, \
             overview-answer-evidence, cand-k-member-manifest, cand-j-callers, cand-k-code-sites, cand-j-chain, cand-j-stop-cause, flow-rank, history-fallback, cand-k-commit-patch, error-origin, config-registry, test-bodies, doc-sections, name-files, rank-blocks, prune-noise"
        ),
        "{}",
        stderr(&output)
    );
    assert!(journal(&fixture.root()).is_empty(), "no brief was computed");

    let named = fixture.run(&[
        "brief",
        "who calls `run`",
        "--without",
        "workspace-overview",
    ]);
    assert!(named.status.success(), "{}", stderr(&named));
    assert!(stdout(&named).starts_with("[PIXEL:BRIEF]\n"));
}

#[test]
fn a_garbled_steps_variable_never_costs_a_brief() {
    let fixture = project();
    let output = fixture
        .pixel(&["brief", "who calls `run` and what breaks if I rename it"])
        .env("PIXEL_BRIEF_STEPS", "garbled, -no-such-step")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).starts_with("[PIXEL:BRIEF]\n"));
}

#[test]
fn brief_is_switched_off_by_the_environment_and_leaves_no_journal_line() {
    let fixture = project();
    let output = fixture
        .pixel(&["brief", "who calls `run`"])
        .env("PIXEL_BRIEF", "0")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
    assert!(stderr(&output).contains("no brief: switched off"));
    assert!(journal(&fixture.root()).is_empty());
}

#[test]
fn a_user_probe_adds_its_block_to_the_brief_and_the_journal_names_its_run() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = project();
    let plugin = fixture.home().join(".pixel/plugins/evidence");
    std::fs::create_dir_all(&plugin).unwrap();
    std::fs::write(
        plugin.join("pixel-plugin.toml"),
        "name = \"evidence\"\nversion = \"0.1.0\"\napi = 1\ndescription = \"evidence\"\nrun = \"probe\"\ncapabilities = [\"brief-probe\"]\n\n[brief-probe]\nowner = \"test\"\nbudget_ms = 1500\n",
    )
    .unwrap();
    let program = plugin.join("probe");
    std::fs::write(
        &program,
        "#!/bin/sh\ncat >/dev/null\nprintf '### probe.rs:1-1 — from a probe\\n1| hello\\n'\n",
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let prompt = "who calls `run` and what breaks if I rename it";
    let output = fixture.run(&["brief", prompt]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("### probe.rs:1-1 — from a probe\n1| hello\n"),
        "{text}"
    );
    assert!(
        text.contains("### src/a.rs:1-3 — run definition (function)"),
        "the probe adds to the brief, it does not replace it: {text}"
    );
    let lines = journal(&fixture.root());
    assert_eq!(lines[0]["probes"][0]["name"], "evidence");
    assert_eq!(lines[0]["probes"][0]["outcome"], "ok");
    assert!(lines[0]["stages"]["probe_ms"].is_u64(), "{}", lines[0]);

    let off = fixture
        .pixel(&["brief", prompt])
        .env("PIXEL_BRIEF_PROBES", "off")
        .output()
        .unwrap();
    assert!(off.status.success(), "{}", stderr(&off));
    assert!(!stdout(&off).contains("from a probe"), "{}", stdout(&off));
    let lines = journal(&fixture.root());
    assert_eq!(lines[1]["probes"], serde_json::json!([]));
    assert_eq!(lines[1]["stages"]["probe_ms"], Value::Null);
}

#[test]
fn every_command_leaves_an_action_line_and_a_block_unless_metrics_are_off() {
    let fixture = project();
    let loud = fixture.run(&["find-symbol", "run"]);
    let block = stderr(&loud);
    assert!(block.contains("🟩 pixel find-symbol ❀ "), "{block}");
    let quiet = fixture.run(&["--metrics", "off", "find-symbol", "run"]);
    assert!(!stderr(&quiet).contains("🟩"), "{}", stderr(&quiet));
    let env_off = fixture
        .pixel(&["find-symbol", "run"])
        .env("PIXEL_METRICS", "0")
        .output()
        .unwrap();
    assert!(!stderr(&env_off).contains("🟩"));
    let lines = action_log(&fixture.root());
    let finds: Vec<&Value> = lines
        .iter()
        .filter(|l| l["command"] == "find-symbol")
        .collect();
    assert_eq!(finds.len(), 3, "the accounting is never skipped");
    assert_eq!(finds[0]["outcome"], "ok");
    assert_eq!(finds[0]["provenance"]["source"], "cli");
    assert!(finds[0]["metrics"]["output_bytes"].as_u64().unwrap() > 0);
    assert_eq!(finds[0]["serve"][0]["route"], "in_process");
}

#[test]
fn a_directory_that_is_no_repository_fails_without_creating_state() {
    let fixture = project();
    let outside = tempfile::tempdir().unwrap();
    let output = fixture
        .pixel(&["find-symbol", "run"])
        .current_dir(outside.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(stderr(&output).contains("pixel:"), "{}", stderr(&output));
    assert!(!outside.path().join(".pixel").exists());
}

#[test]
fn search_meaning_without_the_semantic_build_says_why_and_exits_nonzero() {
    if cfg!(feature = "semantic") {
        return;
    }
    let fixture = project();
    let output = fixture.run(&["--no-daemon", "search-meaning", "where is run called"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.starts_with("semantic search unavailable: failed (this build has no semantic search"),
        "{text}"
    );
    let value = json(&fixture.run(&["--no-daemon", "search-meaning", "run", "--json"]));
    assert_eq!(value["status"], "unavailable");
    assert_eq!(value["reason"], "failed");
}

#[test]
fn search_meaning_download_needs_the_semantic_build() {
    if cfg!(feature = "semantic") {
        return;
    }
    let fixture = project();
    let output = fixture.run(&["search-meaning", "q", "--download"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("no semantic search"),
        "{}",
        stderr(&output)
    );
    assert_eq!(stdout(&output), "");
}

#[test]
fn pack_context_prints_the_symbols_own_lines_and_cuts_to_the_budget() {
    let fixture = project();
    let output = fixture.run(&["--no-daemon", "pack-context", "run"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "src/a.rs:1-3 run (function)\n1| pub fn run(x: u32) -> u32 {\n2|     x\n3| }\n"
    );
    let steps: String = (1..=30)
        .map(|n| format!("    println!(\"step number {n}\");\n"))
        .collect();
    fixture.write("src/big.rs", &format!("pub fn big() {{\n{steps}}}\n"));
    let value = json(&fixture.run(&[
        "--no-daemon",
        "pack-context",
        "big",
        "--json",
        "--budget",
        "1",
    ]));
    assert_eq!(value["symbol"]["file"], "src/big.rs");
    assert_eq!(value["budget_tokens"], 1);
    assert!(value["cut_lines"].as_u64().unwrap() > 0, "{value}");
    assert!(
        value["text"]
            .as_str()
            .unwrap()
            .contains("more line(s) not shown")
    );
    let missing = fixture.run(&["--no-daemon", "pack-context", "nobody"]);
    assert!(!missing.status.success());
    assert!(
        stderr(&missing).contains("no symbol named"),
        "{}",
        stderr(&missing)
    );
}

#[test]
fn find_code_ranks_the_string_before_the_function_and_names_the_owner() {
    let fixture = project();
    fixture.write(
        "src/cart.rs",
        "pub fn add_to_cart(item: u32) -> u32 {\n    item\n}\n\npub fn banner() -> &'static str {\n    \"Add to cart failed\"\n}\n",
    );
    let output = fixture.run(&["--no-daemon", "find-code", "add to cart"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    let first = text.lines().next().unwrap();
    assert_eq!(
        first, "src/cart.rs:6 string in banner — \"Add to cart failed\"",
        "{text}"
    );
    assert!(
        text.contains("src/cart.rs:1 code in add_to_cart — pub fn add_to_cart"),
        "{text}"
    );
    let value = json(&fixture.run(&[
        "--no-daemon",
        "find-code",
        "Add-To-Cart",
        "--json",
        "--limit",
        "1",
    ]));
    assert_eq!(value["leads"].as_array().unwrap().len(), 1);
    assert_eq!(value["leads"][0]["kind"], "string");
    assert_eq!(value["leads"][0]["owner"], "banner");
    let none = fixture.run(&["--no-daemon", "find-code", "nothing like this anywhere"]);
    assert!(none.status.success());
    assert_eq!(stdout(&none), "no line carries those words\n");
    let empty = fixture.run(&["--no-daemon", "find-code", "? !"]);
    assert!(!empty.status.success());
    assert!(
        stderr(&empty).contains("at least one word"),
        "{}",
        stderr(&empty)
    );
}
