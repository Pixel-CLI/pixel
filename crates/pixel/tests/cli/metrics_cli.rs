//! Real CLI metrics boundaries: invocation-local accounting, never stream decoration.
use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

const PIXEL: &str = env!("CARGO_BIN_EXE_pixel");
static NEXT: AtomicU64 = AtomicU64::new(0);

#[test]
fn execution_brief_is_visible_in_cli_help() {
    let fixture = Fixture::new();
    let help = fixture.run(&["--help"]);
    assert!(help.status.success(), "{help:?}");
    let stdout = String::from_utf8_lossy(&help.stdout);
    assert!(stdout.contains("execution-brief"), "{stdout}");
}

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "pixel-metrics-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/login.rs"),
            "pub fn login_user(name: &str) -> bool {\n    !name.is_empty()\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("src/caller.rs"),
            "use crate::login::login_user;\npub fn go() { login_user(\"someone\"); }\n",
        )
        .unwrap();
        fs::write(root.join(".gitignore"), ".pixel/\n").unwrap();
        // Native Git only prepares the isolated test repository; Pixel has no init operation.
        for args in [
            vec!["init", "-q"],
            vec!["add", "."],
            vec!["commit", "-qm", "fixture"],
        ] {
            let result = Command::new("git")
                .args([
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(&root)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            assert!(result.status.success(), "{result:?}");
        }
        Self(root.canonicalize().unwrap())
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(PIXEL);
        cmd.current_dir(&self.0)
            .env("HOME", crate::support::neutral_home())
            .env("PIXEL_DAEMON_AUTO_START", "0")
            .env("PIXEL_METRICS", "1")
            .env_remove("PIXEL_METRICS_ROUND_TRIP_MS")
            .env_remove("PIXEL_TARGETS_GUARD")
            .env_remove("PIXEL_TARGETS_MANIFEST");
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn events(&self, command: &str) -> Vec<Value> {
        fs::read_to_string(self.0.join(".pixel/actions.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("complete JSONL record"))
            .filter(|event| event["command"] == command)
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            crate::support::assert_no_daemon(&self.0);
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn metric_lines(output: &Output) -> Vec<String> {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lines: Vec<_> = stderr.lines().collect();
    let mut blocks = Vec::new();
    let mut index = 0;

    while index < lines.len() {
        if !lines[index].starts_with("🟩 pixel ") {
            index += 1;
            continue;
        }

        let start = index;
        index += 1;
        if lines.get(index) == Some(&"  │") {
            while index < lines.len() {
                let is_separator = lines[index].starts_with("  └");
                index += 1;
                if is_separator {
                    break;
                }
            }
        }
        blocks.push(lines[start..index].join("\n"));
    }

    blocks
}

fn short_invocation_id(event: &Value) -> String {
    let id = event["invocation_id"].as_str().unwrap();
    id.split('-').nth(1).map_or_else(
        || id.to_owned(),
        |s| {
            s.chars()
                .rev()
                .take(6)
                .collect::<String>()
                .chars()
                .rev()
                .collect()
        },
    )
}

fn assert_metric_identity(block: &str, event: &Value) {
    let header = block.lines().next().unwrap();
    assert!(
        header.starts_with(&format!(
            "🟩 pixel {} ❀ ",
            event["command"].as_str().unwrap()
        )),
        "unexpected metric header: {header}"
    );
    assert!(
        header.ends_with(&format!(" ❀ #{}", short_invocation_id(event))),
        "unexpected metric header: {header}"
    );
}

fn assert_success(output: &Output) {
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn dual_savings_use_recorded_round_trip_policy_without_changing_search_json() {
    let fixture = Fixture::new();
    let args = ["search-content", "login_user", ".", "--json", "--no-daemon"];
    let baseline = fixture.run(&args);
    assert_success(&baseline);
    let default_event = fixture.events("search-content").pop().unwrap();
    assert_eq!(
        default_event["metrics"]["time_estimate"]["round_trip_ms"],
        2000
    );

    for (input, expected_ms) in [
        ("3500", 3500),
        ("0", 0),
        ("-1", 2000),
        ("NaN", 2000),
        ("1.5", 2000),
        ("18446744073709551616", 2000),
    ] {
        let output = fixture
            .command()
            .args(args)
            .env("PIXEL_METRICS_ROUND_TRIP_MS", input)
            .output()
            .unwrap();
        assert_success(&output);
        assert_eq!(output.stdout, baseline.stdout);
        let lines = metric_lines(&output);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("estimated LLM context saved:"));
        if expected_ms > 0 {
            assert!(lines[0].contains("against ~"));
        }
        let event = fixture.events("search-content").pop().unwrap();
        let metrics = &event["metrics"];
        let time = &metrics["time_estimate"];
        assert_eq!(time["estimator_version"], "sequential-v1");
        assert_eq!(time["round_trip_ms"], expected_ms);
        assert_eq!(time["native_command_ms"], 0);
        let steps = metrics["evidence"]["native_commands"].as_u64().unwrap()
            + metrics["evidence"]["distinct_files"].as_u64().unwrap();
        assert_eq!(time["sequential_steps"], steps);
        let expected_saved_ms = steps.saturating_sub(1) as f64 * expected_ms as f64
            - metrics["duration_us"].as_u64().unwrap() as f64 / 1000.0;
        assert!((time["saved_ms"].as_f64().unwrap() - expected_saved_ms).abs() < 0.000_001);
        if expected_ms == 0 {
            assert!(time["saved_ms"].as_f64().unwrap() < 0.0);
        }
        assert_eq!(metrics["reporting_bytes"], lines[0].len() + 2);
        assert_metric_identity(&lines[0], &event);
    }
    let disabled = fixture
        .command()
        .args(args)
        .arg("--metrics=off")
        .env("PIXEL_METRICS_ROUND_TRIP_MS", "7000")
        .output()
        .unwrap();
    assert_success(&disabled);
    assert_eq!(disabled.stdout, baseline.stdout);
    assert!(metric_lines(&disabled).is_empty());
    let event = fixture.events("search-content").pop().unwrap();
    assert_eq!(event["metrics"]["time_estimate"]["round_trip_ms"], 7000);
    assert_eq!(event["metrics"]["reporting_bytes"], 0);
}

#[test]
fn concurrent_time_policies_stay_with_their_own_invocations() {
    let fixture = Fixture::new();
    let children: Vec<_> = [0_u64, 1000, 3500, 9000]
        .into_iter()
        .map(|policy| {
            let child = fixture
                .command()
                .args(["repo-state", ".", "--json"])
                .env("PIXEL_METRICS_ROUND_TRIP_MS", policy.to_string())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            (policy, child)
        })
        .collect();
    let outputs: Vec<_> = children
        .into_iter()
        .map(|(policy, child)| (policy, child.wait_with_output().unwrap()))
        .collect();
    let events = fixture.events("repo-state");
    assert_eq!(events.len(), 4);
    let mut ids = HashSet::new();
    for (policy, output) in outputs {
        assert_success(&output);
        serde_json::from_slice::<Value>(&output.stdout).unwrap();
        let lines = metric_lines(&output);
        assert_eq!(lines.len(), 1);
        let header = lines[0].lines().next().unwrap();
        assert!(
            events
                .iter()
                .any(|event| { header.ends_with(&format!("#{}", short_invocation_id(event))) })
        );
        let event = events
            .iter()
            .find(|event| event["metrics"]["time_estimate"]["round_trip_ms"] == policy)
            .unwrap();
        assert!(ids.insert(event["invocation_id"].as_str().unwrap()));
        assert_eq!(event["metrics"]["time_estimate"]["sequential_steps"], 3);
        assert!(
            events
                .iter()
                .any(|event| event["metrics"]["reporting_bytes"] == lines[0].len() + 2),
            "each complete emitted block must be fully accounted for"
        );
    }
}

#[test]
fn time_history_preserves_assumptions_legacy_unavailability_and_exact_lines() {
    use std::io::Write;
    let fixture = Fixture::new();
    let mut historical_lines = Vec::new();
    for ms in [1000, 3500] {
        let output = fixture
            .command()
            .args(["repo-state", ".", "--json"])
            .env("PIXEL_METRICS_ROUND_TRIP_MS", ms.to_string())
            .output()
            .unwrap();
        assert_success(&output);
        historical_lines.extend(metric_lines(&output));
    }
    let originals = fixture.events("repo-state");
    let mut old_metrics = originals[0].clone();
    old_metrics["invocation_id"] = json!("old-token-only-record");
    old_metrics["metrics"]
        .as_object_mut()
        .unwrap()
        .remove("time_estimate");
    let legacy = json!({"ts_ms":1,"pid":1,"command":"legacy","args":"fixture",
        "cwd":fixture.0,"outcome":"ok","duration_ms":3});
    let mut log = fs::OpenOptions::new()
        .append(true)
        .open(fixture.0.join(".pixel/actions.jsonl"))
        .unwrap();
    for event in [&originals[0], &old_metrics, &legacy] {
        writeln!(log, "{event}").unwrap();
    }
    drop(log);
    let report = fixture.run(&["token-savings", ".", "--json", "--metrics=off"]);
    assert_success(&report);
    let data: Value = serde_json::from_slice(&report.stdout).unwrap();
    let summary = &data["workflow_metrics"];
    assert_eq!(summary["duplicate_records"], 1);
    assert_eq!(summary["legacy_records"], 1);
    assert_eq!(summary["time_unavailable_records"], 1);
    let groups = summary["time_estimates"].as_array().unwrap();
    assert_eq!(groups.len(), 2);
    for event in &originals {
        let time = &event["metrics"]["time_estimate"];
        let group = groups
            .iter()
            .find(|g| g["round_trip_ms"] == time["round_trip_ms"])
            .unwrap();
        assert_eq!(group["operations"], 1);
        assert_eq!(group["estimated"]["saved_ms"], time["saved_ms"]);
        assert_eq!(
            group["measured"]["duration_us"],
            event["metrics"]["duration_us"]
        );
    }
    // A later invocation's assumption cannot reinterpret saved records.
    let replay = fixture
        .command()
        .args(["action-log", ".", "--metrics=off"])
        .env("PIXEL_METRICS_ROUND_TRIP_MS", "99999")
        .output()
        .unwrap();
    assert_success(&replay);
    let replay = String::from_utf8(replay.stdout).unwrap();
    for line in historical_lines {
        let header = line.lines().next().unwrap();
        assert!(
            replay
                .lines()
                .any(|saved| saved.strip_prefix("  ") == Some(header)),
            "{replay}"
        );
    }
}

#[test]
fn search_json_is_identical_with_reporting_on_off_and_env_off() {
    let fixture = Fixture::new();
    let args = ["search-content", "login_user", ".", "--json", "--no-daemon"];
    let on = fixture.run(&args);
    assert_success(&on);
    let lines = metric_lines(&on);
    assert_eq!(lines.len(), 1, "{on:?}");
    assert!(String::from_utf8_lossy(&on.stdout).contains("login_user"));
    for line in String::from_utf8_lossy(&on.stdout).lines() {
        serde_json::from_str::<Value>(line).unwrap();
    }
    let events = fixture.events("search-content");
    assert_eq!(events.len(), 1, "search must finalize only one record");
    let event = &events[0];
    assert_eq!(event["outcome"], "ok");
    assert_eq!(
        event["metrics"]["output_bytes"],
        on.stdout.len() + on.stderr.len() - lines[0].len() - 2
    );
    assert_eq!(event["metrics"]["reporting_bytes"], lines[0].len() + 2);
    assert_metric_identity(&lines[0], event);
    assert!(
        event["metrics"]["evidence"]["distinct_files"]
            .as_u64()
            .unwrap()
            >= 2
    );

    let mut disabled_bytes = Vec::new();
    for before in [true, false] {
        let mut cmd = fixture.command();
        if before {
            cmd.arg("--metrics=off");
        }
        cmd.args(args);
        if !before {
            cmd.arg("--metrics=off");
        }
        let off = cmd.output().unwrap();
        assert_success(&off);
        assert_eq!(off.stdout, on.stdout);
        assert!(metric_lines(&off).is_empty());
        disabled_bytes.push(off.stdout.len() + off.stderr.len());
    }
    let env_off = fixture
        .command()
        .args(args)
        .env("PIXEL_METRICS", "0")
        .output()
        .unwrap();
    assert_success(&env_off);
    assert_eq!(env_off.stdout, on.stdout);
    assert!(metric_lines(&env_off).is_empty());
    disabled_bytes.push(env_off.stdout.len() + env_off.stderr.len());
    let events = fixture.events("search-content");
    assert_eq!(events.len(), 4, "disabled reporting retains accounting");
    for (event, bytes) in events[1..].iter().zip(disabled_bytes) {
        assert_eq!(event["metrics"]["reporting_bytes"], 0);
        assert_eq!(event["metrics"]["output_bytes"], bytes);
    }
}

/// A search from a subdirectory that finds nothing prints nothing on stdout;
/// stderr says where it ran and where the repo starts, and the box no longer
/// claims a context saving for an empty answer.
#[test]
fn an_empty_search_from_a_subdirectory_names_the_root_and_claims_no_saving() {
    let fixture = Fixture::new();
    let sub = fixture.0.join("src");
    let output = fixture
        .command()
        .current_dir(&sub)
        .args(["search-content", "-F", "login_user", "--no-daemon"])
        .output()
        .unwrap();
    // `src/` holds the match, so search a name that is only in the root.
    assert!(
        !output.stdout.is_empty(),
        "control: the match exists in src"
    );
    let control_stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !control_stderr.contains("matches under"),
        "a search that matched prints no empty-answer note: {control_stderr}"
    );
    let output = fixture
        .command()
        .current_dir(&sub)
        .args([
            "search-content",
            "-F",
            "no_such_name_anywhere",
            "--no-daemon",
        ])
        .output()
        .unwrap();
    assert_success(&output);
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let sub = sub.canonicalize().unwrap();
    assert!(
        stderr.contains(&format!(
            "0 matches under {}; repo root {}",
            sub.display(),
            fixture.0.display()
        )),
        "{stderr}"
    );
    let block = &metric_lines(&output)[0];
    assert!(
        block.contains("no estimated context saving (empty output)"),
        "{block}"
    );
    assert!(!block.contains("estimated LLM context saved"), "{block}");
    // `--json` keeps its envelope on stdout and adds no note; a page past the
    // first (`--offset 1`) is not "nothing found" and adds none either.
    for extra in [&["--json"][..], &["--offset", "1"][..]] {
        let mut args = vec![
            "search-content",
            "-F",
            "no_such_name_anywhere",
            "--no-daemon",
        ];
        args.extend_from_slice(extra);
        let output = fixture
            .command()
            .current_dir(&sub)
            .args(&args)
            .output()
            .unwrap();
        assert_success(&output);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("matches under"), "{extra:?}: {stderr}");
    }
}

#[test]
fn an_empty_exact_search_should_run_one_task_aware_find_code_fallback() {
    let fixture = Fixture::new();
    assert_success(&fixture.run(&["build-index"]));

    let output = fixture
        .command()
        .args([
            "search-content",
            "-F",
            "missing_exact_pixel_literal",
            "--fallback-query",
            "Trace callers of login_user",
            "--no-daemon",
        ])
        .output()
        .unwrap();
    assert_success(&output);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("login_user"), "{stdout}");
    assert!(stdout.contains("Confidence:"), "{stdout}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr
            .matches("ran one task-aware find-code fallback")
            .count(),
        1,
        "{stderr}"
    );
    assert!(!stderr.contains("0 matches under"), "{stderr}");
    assert_eq!(metric_lines(&output).len(), 1, "{stderr}");
    assert_eq!(fixture.events("search-content").len(), 1);
}

/// When the task-aware fallback also finds nothing, the empty-answer note
/// still lands on stderr — replacing the exact search with one fallback
/// query must not silence the report that no match was found.
#[test]
fn an_exhausted_fallback_still_reports_the_empty_answer_note() {
    let fixture = Fixture::new();
    assert_success(&fixture.run(&["build-index"]));

    let output = fixture
        .command()
        .args([
            "search-content",
            "-F",
            "missing_exact_pixel_literal",
            "--fallback-query",
            "mzzqxwv xorffle gazonk 4f9",
            "--no-daemon",
        ])
        .output()
        .unwrap();
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "No matches found.", "{stdout}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ran one task-aware find-code fallback"),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "0 matches under {}; repo root {}",
            fixture.0.display(),
            fixture.0.display()
        )),
        "{stderr}"
    );
}

#[test]
fn an_exact_hit_should_not_run_fallback_when_call_count_warning_is_present() {
    let fixture = Fixture::new();
    assert_success(&fixture.run(&["build-index"]));
    let session = format!("metrics-cli-fallback-{}", std::process::id());

    let mut final_output = None;
    for pattern in ["l", "lo", "log", "login", "login_", "login_user"] {
        let output = fixture
            .command()
            .env("PIXEL_SESSION_ID", &session)
            .args([
                "search-content",
                "-F",
                pattern,
                "--fallback-query",
                "Trace callers of login_user",
                "--no-daemon",
            ])
            .output()
            .unwrap();
        assert_success(&output);
        final_output = Some(output);
    }

    let output = final_output.unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("login_user"), "{stdout}");
    assert!(!stdout.contains("Confidence:"), "{stdout}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("prior calls in 10 minutes"), "{stderr}");
    assert!(
        !stderr.contains("ran one task-aware find-code fallback"),
        "{stderr}"
    );
}

/// The task-aware fallback is a first-page recovery: on a later page
/// (`--offset > 0`) the skip guard must hold even when that page holds zero
/// matches, so paging an empty exact search cannot silently re-run the
/// fallback query.
#[test]
fn the_task_aware_fallback_skips_nonzero_pages() {
    let fixture = Fixture::new();
    assert_success(&fixture.run(&["build-index"]));

    let output = fixture
        .command()
        .args([
            "search-content",
            "-F",
            "missing_exact_pixel_literal",
            "--offset",
            "1",
            "--fallback-query",
            "mzzqxwv xorffle gazonk 4f9",
            "--no-daemon",
        ])
        .output()
        .unwrap();
    assert_success(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("ran one task-aware find-code fallback"),
        "paging an empty exact search must not run the task-aware fallback: {stderr}"
    );
}

/// `find-code --json` on an overview prompt answers with an empty match list
/// and the README pointer as the note.
#[test]
fn overview_find_code_json_carries_an_empty_match_list_and_the_readme_note() {
    let fixture = Fixture::new();
    fs::write(fixture.0.join("README.md"), "# demo\n").unwrap();
    let output = fixture.run(&["find-code", "what does this repo do", "--json"]);
    assert_success(&output);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        value,
        serde_json::json!({"matches": [], "note": "no concept match; read README.md"}),
        "{output:?}"
    );
}

/// "What does this repo do" names no code: `find-code` and `scope-task` point
/// at the project description instead of matching the word "repo", and
/// `scope-task` writes no manifest that would scope later edits to noise.
#[test]
fn overview_queries_point_at_the_readme_instead_of_matching_the_word_repo() {
    let fixture = Fixture::new();
    fs::write(fixture.0.join("README.md"), "# demo\n").unwrap();
    for args in [
        ["find-code", "what does this repo do"],
        ["scope-task", "tell me what this repo does"],
    ] {
        let output = fixture.run(&args);
        assert_success(&output);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            stdout.trim_end(),
            "no concept match; read README.md",
            "{args:?}"
        );
    }
    assert!(!fixture.0.join(".pixel/targets.json").exists());
}

#[test]
fn capped_search_marks_only_returned_evidence_partial() {
    let fixture = Fixture::new();
    let output = fixture.run(&[
        "search-content",
        "login_user",
        ".",
        "--limit",
        "1",
        "--json",
        "--no-daemon",
    ]);
    assert_success(&output);
    let lines = metric_lines(&output);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("partial"), "{}", lines[0]);
    let events = fixture.events("search-content");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["metrics"]["evidence"]["partial"], true);
    assert_eq!(events[0]["metrics"]["evidence"]["distinct_files"], 1);
    assert_eq!(
        events[0]["metrics"]["output_bytes"],
        output.stdout.len() + output.stderr.len() - lines[0].len() - 2
    );
}

#[test]
fn operation_error_precedes_metrics_and_preserves_failure() {
    let fixture = Fixture::new();
    let output = fixture.run(&["search-content", "(", ".", "--json", "--no-daemon"]);
    assert!(!output.status.success());
    // The failure is machine-readable on stdout (the envelope) and explicit on
    // stderr (the diagnostic, the metrics line, then the diagnostic again so
    // the last stderr line still names the failure under `| tail`).
    let doc: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(doc["ok"], false, "{output:?}");
    assert_eq!(doc["error"]["code"], "INVALID_INPUT", "{doc}");
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    let lines = metric_lines(&output);
    assert_eq!(lines.len(), 1);
    assert!(stderr.starts_with("pixel:"), "{stderr}");
    let (diagnostics, trailer) = stderr
        .split_once(&format!("\n{}\n", lines[0]))
        .expect("the metrics block follows the diagnostic");
    assert!(diagnostics.contains("regex") || diagnostics.contains("pattern"));
    assert_eq!(
        trailer, diagnostics,
        "the error is repeated after the block"
    );
    let events = fixture.events("search-content");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["outcome"], "error");
    // Rendered output covers both streams: the failure envelope on stdout and
    // both copies of the diagnostic on stderr (the metrics line itself is
    // reporting, not output).
    assert_eq!(
        events[0]["metrics"]["output_bytes"],
        (output.stdout.len() + diagnostics.len() + trailer.len()) as u64
    );
    assert!(events[0]["metrics"]["native_workflow_bytes"].is_null());
    assert_metric_identity(&lines[0], &events[0]);

    let disabled = fixture.run(&[
        "--metrics=off",
        "search-content",
        "(",
        ".",
        "--json",
        "--no-daemon",
    ]);
    assert_eq!(output.status.code(), disabled.status.code());
    assert_eq!(disabled.stdout, output.stdout);
    assert_eq!(disabled.stderr, diagnostics.as_bytes());
}

#[test]
fn concurrent_invocations_keep_unique_complete_records_and_lines() {
    let fixture = Fixture::new();
    let children: Vec<_> = (0..4)
        .map(|_| {
            fixture
                .command()
                .args(["repo-state", ".", "--json"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let outputs: Vec<_> = children
        .into_iter()
        .map(|child| child.wait_with_output().unwrap())
        .collect();
    let events = fixture.events("repo-state");
    assert_eq!(events.len(), 4);
    let ids: HashSet<_> = events
        .iter()
        .map(|event| event["invocation_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 4);
    for output in outputs {
        assert_success(&output);
        let document: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(document["head"].as_str().unwrap().len() >= 40);
        let lines = metric_lines(&output);
        assert_eq!(lines.len(), 1);
        let event = events
            .iter()
            .find(|event| {
                lines[0].lines().next().is_some_and(|header| {
                    header.ends_with(&format!("#{}", short_invocation_id(event)))
                })
            })
            .expect("each live metrics block must identify its own action record");
        assert_metric_identity(&lines[0], event);
        assert_eq!(event["metrics"]["output_bytes"], output.stdout.len());
        assert_eq!(event["metrics"]["reporting_bytes"], lines[0].len() + 2);
    }
}

#[test]
fn action_log_failure_does_not_change_success_or_reporting() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.0.join(".pixel/actions.jsonl")).unwrap();
    let output = fixture.run(&["repo-state", ".", "--json"]);
    assert_success(&output);
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["dirty_count"], 0);
    assert_eq!(metric_lines(&output).len(), 1);
    assert!(fixture.0.join(".pixel/actions.jsonl").is_dir());
}

/// A scratch HOME so the machine's real `~/.pixel/config.json` can never
/// leak a global layer into these tests.
fn fake_home(fixture: &Fixture) -> PathBuf {
    let home = fixture.0.join("fake-home");
    fs::create_dir_all(&home).unwrap();
    home
}

#[test]
fn config_metrics_off_hides_the_footer_until_turned_back_on() {
    let fixture = Fixture::new();
    let home = fake_home(&fixture);
    let run = |args: &[&str]| {
        fixture
            .command()
            .args(args)
            .env("HOME", &home)
            .output()
            .unwrap()
    };
    let on = run(&["repo-state", ".", "--json"]);
    assert_success(&on);
    assert_eq!(metric_lines(&on).len(), 1);

    // The persistent repo-level opt-out.
    let set = run(&["config", "metrics", "off"]);
    assert_success(&set);
    let config: Value =
        serde_saphyr::from_str(&fs::read_to_string(fixture.0.join(".pixel/config.yaml")).unwrap())
            .unwrap();
    assert_eq!(config["metrics"], "off");

    let off = run(&["repo-state", ".", "--json"]);
    assert_success(&off);
    assert_eq!(off.stdout, on.stdout, "stdout is untouched by the opt-out");
    assert!(metric_lines(&off).is_empty(), "config off emits no block");
    // Accounting still lands in the journal — the opt-out is presentation.
    let events = fixture.events("repo-state");
    assert_eq!(events.len(), 2);
    assert_eq!(events[1]["metrics"]["reporting_bytes"], 0);
    assert!(events[1]["metrics"]["duration_us"].as_u64().unwrap() > 0);

    // Bare `config metrics` reports the effective setting and its layer.
    let status = run(&["config", "metrics"]);
    assert_success(&status);
    let text = String::from_utf8_lossy(&status.stdout);
    assert!(text.contains("metrics: off"), "{text}");
    assert!(text.contains("repo"), "{text}");

    // Per-invocation vetoes still veto when the config says on.
    let reenable = run(&["config", "metrics", "on"]);
    assert_success(&reenable);
    let vetoed = run(&["repo-state", ".", "--json", "--metrics=off"]);
    assert!(metric_lines(&vetoed).is_empty());
    let on_again = run(&["repo-state", ".", "--json"]);
    assert_eq!(metric_lines(&on_again).len(), 1);
}

#[test]
fn run_hook_metrics_replays_the_invocation_line_for_stderrless_hosts() {
    use std::io::Write;
    let fixture = Fixture::new();
    let home = fake_home(&fixture);
    let run = |args: &[&str]| {
        fixture
            .command()
            .args(args)
            .env("HOME", &home)
            .output()
            .unwrap()
    };
    let call = run(&["repo-state", ".", "--json"]);
    assert_success(&call);
    let emitted = metric_lines(&call);
    assert_eq!(emitted.len(), 1);

    // Codex-shaped PostToolUse payload: shell tool, command string, cwd.
    let payload = json!({
        "tool_name": "shell",
        "tool_input": {"command": "pixel repo-state . --json"},
        "cwd": fixture.0.display().to_string(),
    });
    let hook = |payload: &Value| {
        let mut child = fixture
            .command()
            .args(["run-hook", "metrics", "--provider", "codex"])
            .env("HOME", &home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    let output = hook(&payload);
    assert_success(&output);
    let doc: Value = serde_json::from_slice(&output.stdout).unwrap();
    let context = doc["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("the hook relays the finalized line as context");
    assert_eq!(context, emitted[0].as_str());

    // Opted out: the hook stays as silent as stderr would have been.
    assert_success(&run(&["config", "metrics", "off"]));
    let output = hook(&payload);
    assert_success(&output);
    assert!(output.stdout.is_empty(), "{output:?}");
}

#[test]
fn protected_native_hook_and_statusline_streams_have_no_metrics_line() {
    let fixture = Fixture::new();
    let native = Command::new("grep")
        .args(["-n", "login_user", "src/login.rs"])
        .current_dir(&fixture.0)
        .env_remove("GREP_OPTIONS")
        .output()
        .unwrap();
    let routed = fixture
        .command()
        .args([
            "search-like-rg",
            "grep",
            "--",
            "-n",
            "login_user",
            "src/login.rs",
        ])
        .env_remove("GREP_OPTIONS")
        .output()
        .unwrap();
    assert_eq!(routed.status.code(), native.status.code());
    assert_eq!(routed.stdout, native.stdout);
    assert_eq!(routed.stderr, native.stderr);
    assert!(String::from_utf8_lossy(&routed.stdout).contains("pub fn login_user"));

    for args in [
        vec!["status", ".", "--statusline"],
        vec!["run-hook", "session-start", "."],
    ] {
        let output = fixture.run(&args);
        assert_success(&output);
        assert!(
            !output.stdout.is_empty(),
            "protected operation must exercise real output"
        );
        assert!(metric_lines(&output).is_empty());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("🟩 pixel "));
        let events = fixture.events(args[0]);
        assert!(!events.is_empty());
        assert!(
            events.last().unwrap()["metrics"].is_null(),
            "unsupported volume capture is unavailable"
        );
    }
}

#[test]
fn explicit_repository_impact_logs_at_target_and_preserves_json() {
    let fixture = Fixture::new();
    // First graph construction intentionally adds timing/freshness metadata;
    // compare equivalent warm results rather than cold-vs-warm output.
    assert_success(&fixture.run(&["repo-map", ".", "--json", "--metrics=off"]));
    let output = fixture
        .command()
        .current_dir(std::env::temp_dir())
        .args(["impact", "login_user"])
        .arg(&fixture.0)
        .arg("--json")
        .output()
        .unwrap();
    assert_success(&output);
    let data: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(data["affected_files"].as_u64().unwrap() >= 1, "{data}");
    let lines = metric_lines(&output);
    assert_eq!(lines.len(), 1);
    let events = fixture.events("impact");
    assert_eq!(events.len(), 1);
    assert!(
        events[0]["metrics"]["evidence"]["relationships"]
            .as_u64()
            .unwrap()
            >= 1
    );
    let off = fixture.run(&["impact", "login_user", ".", "--json", "--metrics=off"]);
    assert_success(&off);
    assert_eq!(output.stdout, off.stdout);
}

#[test]
fn legacy_savings_and_error_details_survive_with_workflow_summary() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.0.join(".pixel")).unwrap();
    let legacy = json!({
        "ts_ms": 1, "pid": 1, "command": "search", "args": "legacy needle",
        "cwd": fixture.0, "outcome": "ok", "duration_ms": 3,
        "snippet_cap_chars": 20, "pool_chars": 80
    });
    fs::write(
        fixture.0.join(".pixel/actions.jsonl"),
        format!("{legacy}\n"),
    )
    .unwrap();
    assert_success(&fixture.run(&["repo-state", ".", "--json"]));
    let failed = fixture.run(&["search-content", "(", ".", "--json", "--no-daemon"]);
    assert!(!failed.status.success());
    let errors = fixture.run(&["action-log", ".", "--errors-only", "--metrics=off"]);
    assert_success(&errors);
    let text = String::from_utf8(errors.stdout).unwrap();
    assert!(text.contains("search-content ("), "arguments lost: {text}");
    assert!(
        text.contains("regex") || text.contains("pattern"),
        "error lost: {text}"
    );
    let report = fixture.run(&["token-savings", ".", "--json", "--metrics=off"]);
    assert_success(&report);
    let data: Value = serde_json::from_slice(&report.stdout).unwrap();
    assert_eq!(data["total_pool_chars"], 80);
    assert_eq!(data["total_snippet_chars"], 20);
    assert_eq!(data["overall_savings"], 0.75);
    assert_eq!(data["workflow_metrics"]["legacy_records"], 1);
    assert!(
        data["workflow_metrics"]["versions"]["workflow-v2"]["complete"]["operations"]
            .as_u64()
            .unwrap()
            >= 1
    );
    assert!(
        data["workflow_metrics"]["versions"]["workflow-v2"]["unavailable"]["operations"]
            .as_u64()
            .unwrap()
            >= 1
    );
}

#[test]
fn graph_text_and_json_account_for_same_returned_files() {
    let fixture = Fixture::new();
    for args in [vec!["find-code", "login_user", "."], vec!["repo-map", "."]] {
        let mut text_args = args.clone();
        if args[0] == "repo-map" {
            text_args.push("--markdown");
        }
        let text_output = fixture.run(&text_args);
        assert_success(&text_output);
        assert!(String::from_utf8_lossy(&text_output.stdout).contains("login_user"));
        let mut json_args = args.clone();
        json_args.push("--json");
        let json_output = fixture.run(&json_args);
        assert_success(&json_output);
        serde_json::from_slice::<Value>(&json_output.stdout).unwrap();
        let events = fixture.events(args[0]);
        assert_eq!(events.len(), 2);
        let text_files = events[0]["metrics"]["evidence"]["distinct_files"]
            .as_u64()
            .unwrap();
        assert!(text_files > 0);
        assert_eq!(
            text_files,
            events[1]["metrics"]["evidence"]["distinct_files"]
                .as_u64()
                .unwrap()
        );
        assert_ne!(
            events[0]["metrics"]["output_bytes"],
            events[1]["metrics"]["output_bytes"]
        );
        assert_eq!(metric_lines(&text_output).len(), 1);
        assert_eq!(metric_lines(&json_output).len(), 1);
    }
}

/// An agent reads a failed call through `2>&1 | tail -N`: when the metrics
/// block is the last thing on stderr, the error scrolls out of that window and
/// a failed operation (a commit, a push) reads as one that finished. The error
/// must be the last non-empty stderr line; with metrics off nothing is
/// repeated, so it appears exactly once.
#[test]
fn failure_ends_stderr_on_its_error_and_metrics_off_prints_it_once() {
    fn last_non_empty(stderr: &str) -> &str {
        stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_default()
    }
    let fixture = Fixture::new();
    let args = ["search-content", "fn main", "src/nope", "--no-daemon"];

    let live = fixture.run(&args);
    assert!(!live.status.success(), "{live:?}");
    let stderr = String::from_utf8(live.stderr.clone()).unwrap();
    let error = stderr
        .lines()
        .find(|line| line.starts_with("pixel: ") && line.contains("src/nope"))
        .unwrap_or_else(|| panic!("no diagnostic naming the bad path: {stderr}"));
    assert_eq!(metric_lines(&live).len(), 1, "{stderr}");
    assert_eq!(last_non_empty(&stderr), error, "{stderr}");
    assert_eq!(stderr.matches(error).count(), 2, "{stderr}");

    let flag_off = fixture
        .command()
        .arg("--metrics=off")
        .args(args)
        .output()
        .unwrap();
    let env_off = fixture
        .command()
        .env("PIXEL_METRICS", "0")
        .args(args)
        .output()
        .unwrap();
    let runs = [live, flag_off, env_off];
    for off in &runs[1..] {
        assert_eq!(off.status.code(), runs[0].status.code(), "{off:?}");
        let stderr = String::from_utf8_lossy(&off.stderr);
        assert!(!stderr.contains("🟩 pixel "), "{stderr}");
        assert_eq!(stderr.matches(error).count(), 1, "{stderr}");
        assert_eq!(last_non_empty(&stderr), error, "{stderr}");
    }

    // Every rendered byte is accounted once: the repeat is output on the live
    // run, and a run that prints no block records no repeat it never wrote.
    let events = fixture.events("search-content");
    assert_eq!(events.len(), runs.len());
    for (run, event) in runs.iter().zip(&events) {
        let metrics = &event["metrics"];
        assert_eq!(
            metrics["output_bytes"].as_u64().unwrap()
                + metrics["reporting_bytes"].as_u64().unwrap(),
            (run.stdout.len() + run.stderr.len()) as u64,
            "{event}"
        );
    }
}

#[test]
fn negative_workflow_savings_are_not_clamped() {
    let fixture = Fixture::new();
    for i in 0..40 {
        fs::write(
            fixture.0.join(format!(
                "src/{}_{}.rs",
                "long_fixture_filename".repeat(6),
                i
            )),
            "pub fn item() {}\n",
        )
        .unwrap();
    }
    let output = fixture.run(&["repo-state", ".", "--json"]);
    assert_success(&output);
    let data: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(data["dirty_count"], 40);
    let lines = metric_lines(&output);
    assert_eq!(lines.len(), 1);
    assert!(
        !lines[0].contains("estimated LLM context saved:"),
        "negative token savings must not be presented as a saving: {}",
        lines[0]
    );
    let event = &fixture.events("repo-state")[0];
    let metrics = &event["metrics"];
    assert!(
        metrics["native_workflow_bytes"].as_u64().unwrap()
            < metrics["output_bytes"].as_u64().unwrap()
                + metrics["reporting_bytes"].as_u64().unwrap()
    );
}

#[test]
fn metrics_preserve_safe_publication_replay_and_head_guard() {
    let fixture = Fixture::new();
    for (key, value) in [
        ("user.name", "Fixture"),
        ("user.email", "fixture@example.invalid"),
        ("commit.gpgsign", "false"),
    ] {
        assert!(
            Command::new("git")
                .args(["config", key, value])
                .current_dir(&fixture.0)
                .status()
                .unwrap()
                .success()
        );
    }
    let before: Value =
        serde_json::from_slice(&fixture.run(&["repo-state", "--json"]).stdout).unwrap();
    let head = before["head"].as_str().unwrap();
    fs::write(
        fixture.0.join("src/login.rs"),
        "pub fn login_user() -> bool { true }\n",
    )
    .unwrap();
    let args = [
        "commit",
        "--message",
        "test: greeting fixture",
        "--request-id",
        "metrics-publication",
        "--expected-head",
        head,
        "--files",
        "src/login.rs",
        "--json",
    ];
    let first = fixture.run(&args);
    assert_success(&first);
    let published: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(published["published"], true);
    assert_ne!(published["head"], before["head"]);
    let replay = fixture.run(&args);
    assert_success(&replay);
    assert_eq!(
        serde_json::from_slice::<Value>(&replay.stdout).unwrap()["head"],
        published["head"]
    );
    fs::write(
        fixture.0.join("src/login.rs"),
        "pub fn login_user() -> bool { false }\n",
    )
    .unwrap();
    let refused = fixture.run(&[
        "commit",
        "--message",
        "test: stale guard",
        "--request-id",
        "stale-metrics-publication",
        "--expected-head",
        head,
        "--files",
        "src/login.rs",
        "--json",
    ]);
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(metric_lines(&refused).len(), 1);
    let after: Value =
        serde_json::from_slice(&fixture.run(&["repo-state", "--json"]).stdout).unwrap();
    assert_eq!(after["head"], published["head"]);
    let events = fixture.events("commit");
    assert_eq!(events.len(), 3);
    assert_eq!(events[2]["outcome"], "error");
    assert_eq!(events[2]["metrics"]["output_scope"], "cli-rendered-streams");
}

#[test]
fn daemon_reindex_reports_actual_nested_index_counts() {
    let fixture = Fixture::new();
    assert_success(&fixture.run(&["build-index"]));
    assert_success(&fixture.run(&["daemon", "start"]));
    let reindexed = fixture.run(&["build-index"]);
    let status = fixture.run(&["status", "--json"]);
    // Stop before assertions so a failed count assertion never leaves a daemon.
    assert_success(&fixture.run(&["daemon", "stop"]));
    assert_success(&reindexed);
    assert_success(&status);
    let value: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert!(value["index"]["base_files"].as_u64().unwrap() > 0);
    let expected = format!(
        "indexed via daemon: base_files={} delta_files={} overlay_files={}",
        value["index"]["base_files"],
        value["index"]["delta_files"],
        value["index"]["overlay_files"]
    );
    let stderr = String::from_utf8_lossy(&reindexed.stderr);
    assert!(
        stderr.contains(&expected),
        "build-index must report the counts status --json reports: expected {expected:?} in {stderr:?}"
    );
    assert_eq!(metric_lines(&reindexed).len(), 1);
}

/// A renamed command's note is rendered output the caller reads, like any
/// other stderr line: `output_bytes` must count it, or every call through an
/// old name is priced against less output than it printed.
#[test]
fn rename_note_counts_as_rendered_output() {
    let fixture = Fixture::new();
    let out = fixture.run(&["inspect", "--json", "."]);
    assert_success(&out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.starts_with("note: 'inspect' is now 'repo-state'"),
        "{stderr}"
    );
    let lines = metric_lines(&out);
    assert_eq!(lines.len(), 1, "{out:?}");
    let events = fixture.events("repo-state");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(
        events[0]["metrics"]["output_bytes"],
        out.stdout.len() + out.stderr.len() - lines[0].len() - 2
    );
}

/// `prepare-repo --json` reports an already-running daemon on stderr
/// (`daemon_start` in quiet mode, through `eprint!`): that line is rendered
/// output too, and `output_bytes` must count it like the rename note.
#[test]
fn quiet_daemon_report_counts_as_rendered_output() {
    let fixture = Fixture::new();
    assert_success(&fixture.run(&["daemon", "start"]));
    let out = fixture.run(&["prepare-repo", "--json", "."]);
    // Stop before assertions so a failed one never leaves a daemon behind.
    assert_success(&fixture.run(&["daemon", "stop"]));
    assert_success(&out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("daemon already running"), "{stderr}");
    let lines = metric_lines(&out);
    assert_eq!(lines.len(), 1, "{out:?}");
    let events = fixture.events("prepare-repo");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(
        events[0]["metrics"]["output_bytes"],
        out.stdout.len() + out.stderr.len() - lines[0].len() - 2
    );
}

/// A slow `actions.jsonl` line is only diagnosable when it says how the
/// request was served: in process and why (with the open and the handling
/// timed apart), through a daemon it had to start, or through one already
/// running (with the probe and the request timed apart).
#[test]
fn action_log_records_how_each_request_was_served() {
    let fixture = Fixture::new();
    assert_success(&fixture.run(&["search-content", "login_user", "."]));
    assert_success(&fixture.run(&["search-content", "login_user", ".", "--no-daemon"]));
    let started = fixture
        .command()
        .env_remove("PIXEL_DAEMON_AUTO_START")
        .args(["search-content", "login_user", "."])
        .output()
        .unwrap();
    // A start that outlasted its 5 s window still brings the daemon up
    // later: wait for it (bounded), so the next call is served by it and the
    // stop below also reaches a late one.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while std::time::Instant::now() < deadline
        && !String::from_utf8_lossy(&fixture.run(&["daemon", "status"]).stdout)
            .starts_with("daemon running")
    {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let served = fixture.run(&["search-content", "login_user", "."]);
    // Stop before assertions so a failed one never leaves a daemon behind.
    assert_success(&fixture.run(&["daemon", "stop"]));
    assert_success(&started);
    assert_success(&served);

    let events = fixture.events("search-content");
    assert_eq!(events.len(), 4, "{events:?}");
    let steps: Vec<&Value> = events
        .iter()
        .map(|event| {
            let serve = event["serve"].as_array().expect("serve steps");
            assert_eq!(serve.len(), 1, "{event}");
            &serve[0]
        })
        .collect();

    let disabled = steps[0];
    assert_eq!(disabled["route"], "in_process");
    assert_eq!(disabled["reason"], "auto_start_disabled");
    for phase in ["probe_ms", "open_ms", "handle_ms"] {
        assert!(disabled[phase].is_u64(), "{phase}: {disabled}");
    }
    assert!(disabled.get("start_ms").is_none(), "{disabled}");

    let no_daemon = steps[1];
    assert_eq!(no_daemon["reason"], "no_daemon");
    assert!(no_daemon.get("probe_ms").is_none(), "{no_daemon}");
    assert!(no_daemon["open_ms"].is_u64(), "{no_daemon}");

    // A loaded runner may outlast the start window: either way the start
    // was attempted and its wait is on the line.
    let start = steps[2];
    assert!(
        start["route"] == "daemon_started" || start["reason"] == "start_timed_out",
        "{start}"
    );
    assert!(start["start_ms"].is_u64(), "{start}");

    let daemon = steps[3];
    assert_eq!(daemon["route"], "daemon");
    assert!(
        daemon["probe_ms"].is_u64() && daemon["request_ms"].is_u64(),
        "{daemon}"
    );
    assert!(daemon.get("open_ms").is_none(), "{daemon}");
}

/// `pixel action-log . --limit N | head -1`: the reader closes the pipe while
/// the process still has lines to write. That is a truncated read, not a
/// failure — the counted `print!` path must absorb EPIPE instead of panicking
/// with exit 101.
#[test]
fn closed_stdout_pipe_is_success_not_a_panic() {
    /// More lines than any pipe buffer holds, so the process cannot finish
    /// writing before the test closes the read end.
    const SEEDED_LINES: usize = 4000;

    let fixture = Fixture::new();
    let log = fixture.0.join(".pixel/actions.jsonl");
    fs::create_dir_all(log.parent().unwrap()).unwrap();
    let seeded: String = (0..SEEDED_LINES)
        .map(|n| {
            format!(
                "{}\n",
                json!({
                    "ts_ms": 1,
                    "pid": 1,
                    "command": "search-content",
                    "args": format!("seed-{n} --path . --budget 4000"),
                    "cwd": "/fixture",
                    "outcome": "ok",
                    "duration_ms": 1,
                })
            )
        })
        .collect();
    fs::write(&log, seeded).unwrap();

    let limit = SEEDED_LINES.to_string();
    let mut child = fixture
        .command()
        .args(["action-log", ".", "--limit", limit.as_str()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // `| head -1`: keep the first line, then drop the read end.
    let mut first_line = String::new();
    {
        let mut reader = BufReader::new(child.stdout.take().unwrap());
        reader.read_line(&mut first_line).unwrap();
    }
    let output = child.wait_with_output().unwrap();

    assert!(
        first_line.contains("search-content"),
        "the first rendered line arrives before the reader closes: {first_line:?}"
    );
    assert!(
        output.status.success(),
        "a closed stdout reader is a success, not exit 101: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("panicked"),
        "EPIPE must not panic the process: {stderr}"
    );
}

/// No-policy commands keep a compact identity line and a recorded gap.
/// Applicable comparisons still explain a one-step baseline or capped evidence.
#[test]
fn live_blocks_state_every_absent_comparison_and_keep_json_stdout_clean() {
    let fixture = Fixture::new();

    let status = fixture.run(&["status", ".", "--json"]);
    assert_success(&status);
    serde_json::from_slice::<Value>(&status.stdout).unwrap();
    assert!(!String::from_utf8_lossy(&status.stdout).contains("🟩 pixel "));
    let block = &metric_lines(&status)[0];
    assert_eq!(block.lines().count(), 1, "{block}");
    assert!(block.starts_with("🟩 pixel status ❀ "), "{block}");
    assert!(!block.contains("unavailable"), "{block}");
    let event = fixture.events("status").pop().unwrap();
    assert_eq!(event["metrics"]["comparison_gap"], "no_policy");
    assert!(event["metrics"]["native_workflow_bytes"].is_null());
    assert!(event["metrics"]["time_estimate"].is_null());

    let history = fixture.run(&["commit-history", ".", "--limit", "1", "--json"]);
    assert_success(&history);
    serde_json::from_slice::<Value>(&history.stdout).unwrap();
    let block = &metric_lines(&history)[0];
    assert!(
        block.contains("├─ ⏱ no estimated time saving (baseline has no saved round trip)"),
        "{block}"
    );
    assert!(
        block.contains("├─ § estimated LLM context saved:"),
        "{block}"
    );
    let event = fixture.events("commit-history").pop().unwrap();
    assert_eq!(event["metrics"]["time_estimate"]["sequential_steps"], 1);
    assert!(event["metrics"]["comparison_gap"].is_null());

    let capped = fixture
        .command()
        .args(["repo-map", ".", "--json"])
        .env("PIXEL_OUTPUT_CAP_BYTES", "1")
        .output()
        .unwrap();
    assert_success(&capped);
    let block = &metric_lines(&capped)[0];
    assert!(
        block.contains("├─ ⏱ unavailable: the rendered output cap hid the evidence"),
        "{block}"
    );
    assert!(
        block.contains("├─ § unavailable: the rendered output cap hid the evidence"),
        "{block}"
    );
    let event = fixture.events("repo-map").pop().unwrap();
    assert_eq!(event["metrics"]["comparison_gap"], "output_truncated");
    assert!(event["metrics"]["native_workflow_bytes"].is_null());
}

/// `push` now carries the one-command baseline it replaces (`git push`), so a
/// removed policy entry is a visible regression in both the live line and the
/// record, not a silently empty table row.
#[test]
fn push_baseline_replaces_git_push_and_states_the_single_step_time_gap() {
    let fixture = Fixture::new();
    let remote = fixture.0.join("remote.git");
    let initialized = Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(&remote)
        .status()
        .unwrap();
    assert!(initialized.success());
    let added = Command::new("git")
        .args(["remote", "add", "origin"])
        .arg(&remote)
        .current_dir(&fixture.0)
        .status()
        .unwrap();
    assert!(added.success());
    let output = fixture.run(&[
        "push",
        "origin",
        "HEAD",
        "--request-id",
        "metrics-push-golden",
        "--json",
    ]);
    assert_success(&output);
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["pushed"], true, "{document}");
    let block = &metric_lines(&output)[0];
    assert!(
        block.contains("├─ ⏱ no estimated time saving (baseline has no saved round trip)"),
        "{block}"
    );
    assert!(
        block.contains("├─ § estimated LLM context saved:"),
        "{block}"
    );
    let event = fixture.events("push").pop().unwrap();
    assert_eq!(event["metrics"]["evidence"]["native_commands"], 1);
    assert_eq!(event["metrics"]["native_workflow_bytes"], 1024);
    assert_eq!(event["metrics"]["time_estimate"]["sequential_steps"], 1);
    assert!(event["metrics"]["comparison_gap"].is_null());
}

/// The first result a new user sees: `pixel list-signatures` on a large file
/// states the whole-file read and its own answer, both as UTF-8 bytes / 4
/// floored (the rule of `scripts/bench-read-savings.sh`), so `wc -c` on the
/// file and on stdout re-derives the line. Under `workflow-v1` the baseline
/// was a 4 KiB guess plus 1 KiB for an assumed command, whatever the file's
/// size: a 20 KB file read as "47% saved" when the answer was 94% smaller.
#[test]
fn list_signatures_compares_the_measured_file_with_its_stdout_answer() {
    let fixture = Fixture::new();
    let body = (0..120)
        .map(|n| {
            format!(
                "pub fn step_{n}(input: &str) -> usize {{\n{}    input.trim().len() + {n}\n}}\n\n",
                "    let _ = input.split_whitespace().count();\n".repeat(8)
            )
        })
        .collect::<String>();
    fs::write(fixture.0.join("src/big.rs"), &body).unwrap();
    let output = fixture.run(&["list-signatures", "src/big.rs"]);
    assert_success(&output);
    let file_bytes = body.len() as u64;
    let answer_bytes = output.stdout.len() as u64;
    assert!(
        answer_bytes * 4 < file_bytes,
        "the fixture must be large enough to show a saving: {answer_bytes} of {file_bytes}"
    );
    let (full_tok, answer_tok) = (file_bytes / 4, answer_bytes / 4);
    let pct = (100.0 * (1.0 - answer_tok as f64 / full_tok as f64)).round() as i64;
    let block = &metric_lines(&output)[0];
    assert!(
        block.contains(&format!(
            "  ├─ § full read {full_tok} tok, pixel answer {answer_tok} tok (-{pct}%)\n"
        )),
        "{block}"
    );
    assert!(!block.contains("estimated LLM context saved"), "{block}");
    // One read against one call: no round trip is saved, and the line says so.
    assert!(
        block.contains("├─ ⏱ no estimated time saving (baseline has no saved round trip)"),
        "{block}"
    );
    let event = fixture.events("list-signatures").pop().unwrap();
    let metrics = &event["metrics"];
    assert_eq!(metrics["estimator_version"], "workflow-v2");
    assert_eq!(metrics["evidence"]["known_file_bytes"], file_bytes);
    assert_eq!(metrics["evidence"]["native_commands"], 0);
    assert_eq!(metrics["evidence"]["relationships"], 0);
    assert_eq!(metrics["native_workflow_bytes"], file_bytes);
    assert_eq!(metrics["answer_bytes"], answer_bytes);
}

/// An imported Claude metrics entry running beside Devin's own relay would
/// emit the Claude contract into a host that never asked for it and double
/// the native `--provider devin` relay's output, so the entry exits before
/// reading stdin when the process carries an importing-config marker. The
/// same entry in a real Claude session still relays.
#[test]
fn an_imported_claude_metrics_entry_is_silent_in_the_importing_host() {
    use std::io::Write;
    let fixture = Fixture::new();
    let home = fake_home(&fixture);
    // The relay correlates the payload to a recorded invocation by cwd +
    // argv: record one first.
    let warmup = fixture
        .command()
        .args(["repo-state", ".", "--json"])
        .env("HOME", &home)
        .output()
        .unwrap();
    assert!(warmup.status.success(), "{warmup:?}");

    let run = |marker: bool| {
        let payload = json!({
            "tool_name": "shell",
            "tool_input": {"command": "pixel repo-state . --json"},
            "cwd": fixture.0.display().to_string(),
        });
        let mut command = fixture.command();
        command
            .args(["run-hook", "metrics", "--provider", "claude"])
            .env("HOME", &home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if marker {
            command.env("DEVIN_PROJECT_DIR", fixture.0.as_os_str());
        } else {
            command.env_remove("DEVIN_PROJECT_DIR");
        }
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };

    // A real Claude session gets the advisory contract.
    let claude = run(false);
    assert!(
        claude.status.success(),
        "{}",
        String::from_utf8_lossy(&claude.stderr)
    );
    let doc: Value = serde_json::from_slice(&claude.stdout).unwrap();
    assert!(
        doc["hookSpecificOutput"]["additionalContext"].is_string(),
        "{doc}"
    );

    // The imported copy inside Devin: silent, stdin unread.
    let imported = run(true);
    assert!(imported.status.success(), "{imported:?}");
    assert!(imported.stdout.is_empty(), "{imported:?}");
    assert!(imported.stderr.is_empty(), "{imported:?}");
}
