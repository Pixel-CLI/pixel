// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `run-recipe --kind locate`: one call that resolves a phrase, shows the
//! context of what it singles out, lists the test files among the callers,
//! and says how far the answer gets, the same through the daemon and in
//! process.

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::support::{Scratch, assert_no_daemon, daemons_serving, git, pixel_command};

fn fixture(tag: &str) -> Scratch {
    let dir = Scratch::for_test("pixel-locate-cli", tag);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    let files = [
        (
            "src/flow.ts",
            "export function flowDir(): string {\n  return process.env.FLOW_DIR ?? \"\"\n}\n",
        ),
        (
            "src/render.ts",
            "export function render(x: number): number {\n  return x + 1\n}\n",
        ),
        (
            "src/other.ts",
            "export function render(x: number): number {\n  return x + 2\n}\n",
        ),
        (
            "src/box.ts",
            "export class Box {\n  open(): number {\n    return 1\n  }\n}\n",
        ),
        (
            "src/crate.ts",
            "export class Crate {\n  open(): number {\n    return 2\n  }\n}\n",
        ),
        (
            "tests/flow.test.ts",
            "import { flowDir } from '../src/flow'\nexport function checkFlow(): string {\n  return flowDir()\n}\n",
        ),
    ];
    for (path, content) in files {
        std::fs::write(dir.join(path), content).unwrap();
    }
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "init"]);
    dir
}

fn locate(root: &Path, phrase: &str, budget: &str, extra: &[&str]) -> Value {
    let out = pixel_command()
        .args([
            "run-recipe",
            phrase,
            "--kind",
            "locate",
            "--json",
            "--budget",
            budget,
        ])
        .arg("--path")
        .arg(root)
        .args(extra)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "run-recipe: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn locate_should_show_one_exact_symbol_with_its_test_files_in_one_call() {
    let root = fixture("located");
    let answer = locate(&root, "where is `flowDir`", "1500", &["--no-daemon"]);
    let locate = &answer["locate"];
    assert_eq!(locate["status"], "located", "{answer}");
    assert_eq!(locate["targets"][0]["uid"], "src/flow.ts#flowDir#function");
    assert!(
        locate["targets"][0]["text"]
            .as_str()
            .unwrap()
            .contains("return process.env.FLOW_DIR"),
        "{answer}"
    );
    assert_eq!(
        locate["tests_found"],
        serde_json::json!(["tests/flow.test.ts"])
    );
    assert!(locate["next_action"].is_null(), "{answer}");
    assert_eq!(locate["limits"], serde_json::json!([]), "{answer}");
    assert_eq!(answer["result"]["plan"][0]["recipe"], "locate.v2");
    // resolve, context, callers: one round trip each, no subprocess.
    assert_eq!(answer["metrics"]["operations"], 3);
    let rendered = answer["metrics"]["rendered_tokens_estimate"]
        .as_u64()
        .unwrap();
    assert!(rendered <= 1500, "{rendered} over budget");
}

#[test]
fn locate_should_give_the_same_answer_through_the_daemon_and_in_process() {
    let root = fixture("parity");
    let in_process = locate(&root, "where is `flowDir`", "1500", &["--no-daemon"]);
    let start = pixel_command()
        .args(["daemon", "start"])
        .arg(&*root)
        .output()
        .unwrap();
    assert!(start.status.success(), "{start:?}");
    let daemon = StopDaemonOnDrop(&root);
    let through_daemon = locate(&root, "where is `flowDir`", "1500", &[]);
    drop(daemon);
    assert_eq!(through_daemon["locate"], in_process["locate"]);
}

/// Stops the daemon serving a fixture even when the test panics before its
/// last line, so a failed assertion never leaves a daemon behind.
struct StopDaemonOnDrop<'a>(&'a Path);

impl Drop for StopDaemonOnDrop<'_> {
    fn drop(&mut self) {
        let stop = pixel_command()
            .args(["daemon", "stop"])
            .arg(self.0)
            .output();
        // A test that already failed keeps its own panic message; a second
        // panic inside drop would abort the whole test binary.
        if !std::thread::panicking() {
            let stop = stop.unwrap();
            assert!(stop.status.success(), "{stop:?}");
            // Shutdown acknowledges the request before the process finishes
            // releasing its watcher and acceptor. Keep the fixture root alive
            // until that exit, so Scratch's immediate leak scan cannot race it.
            let deadline = Instant::now() + Duration::from_secs(5);
            while !daemons_serving(self.0).is_empty() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            // A real leak still fails and is killed by the shared leak guard.
            assert_no_daemon(self.0);
        }
    }
}

#[test]
fn locate_should_call_homonyms_ambiguous_and_name_how_to_pick_one() {
    let root = fixture("ambiguous");
    let answer = locate(&root, "where is `render`", "1500", &["--no-daemon"]);
    let locate = &answer["locate"];
    assert_eq!(locate["status"], "ambiguous", "{answer}");
    let mut uids: Vec<&str> = locate["targets"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["uid"].as_str())
        .collect();
    uids.sort_unstable();
    assert_eq!(
        uids,
        [
            "src/other.ts#render#function",
            "src/render.ts#render#function"
        ]
    );
    assert!(
        locate["next_action"]
            .as_str()
            .unwrap()
            .starts_with("pick the intended candidate by uid, e.g. pixel pack-context 'src/"),
        "{answer}"
    );
}

#[test]
fn locate_should_send_a_miss_to_an_exact_search() {
    let root = fixture("miss");
    let answer = locate(&root, "where is `zzqxMissing`", "1500", &["--no-daemon"]);
    let locate = &answer["locate"];
    assert_eq!(locate["status"], "needs_search", "{answer}");
    assert_eq!(
        locate["next_action"],
        "pixel search-content -F 'zzqxMissing'"
    );
    assert_eq!(locate["targets"], serde_json::json!([]));
    // resolve, then the file ranking a miss calls for.
    assert_eq!(answer["metrics"]["operations"], 2);
}

#[test]
fn locate_should_fall_back_to_the_name_when_a_method_uid_is_not_its_name() {
    let root = fixture("methods");
    let answer = locate(&root, "where is `open`", "1500", &["--no-daemon"]);
    let locate = &answer["locate"];
    assert_eq!(locate["status"], "ambiguous", "{answer}");
    let mut uids: Vec<&str> = locate["targets"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["uid"].as_str())
        .collect();
    uids.sort_unstable();
    assert_eq!(uids.len(), 2, "{answer}");
    assert!(
        uids[0].starts_with("src/box.ts#") && uids[1].starts_with("src/crate.ts#"),
        "{answer}"
    );
    // resolve, then per method: the guessed uid, the name (two candidates)
    // and the candidate's uid; then the best one's callers.
    assert_eq!(answer["metrics"]["operations"], 8, "{answer}");
}

#[test]
fn locate_should_ask_each_homonym_by_uid_when_their_list_does_not_fit() {
    let root = fixture("by-uid");
    // Shares of 240 and 60 tokens: the two-candidate list needs about 110,
    // one symbol's context fits.
    let answer = locate(&root, "where is `render`", "400", &["--no-daemon"]);
    let mut uids: Vec<&str> = answer["locate"]["targets"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["uid"].as_str())
        .collect();
    uids.sort_unstable();
    assert_eq!(
        uids,
        [
            "src/other.ts#render#function",
            "src/render.ts#render#function"
        ],
        "{answer}"
    );
}

#[test]
fn locate_should_say_what_a_small_budget_cost_it() {
    let root = fixture("budget");
    let answer = locate(&root, "where is `render`", "120", &["--no-daemon"]);
    let rendered = answer["metrics"]["rendered_tokens_estimate"]
        .as_u64()
        .unwrap();
    let limits = answer["locate"]["limits"].to_string();
    assert!(
        limits.contains("context of `render` unavailable"),
        "a 120-token answer cannot hold two contexts silently: {answer}"
    );
    // The target shown got no context text from its share, so no text was
    // dropped to make room; the answer used to say one was.
    assert_eq!(answer["locate"]["targets"][0]["text"], "", "{answer}");
    assert!(
        !limits.contains("dropped to fit the budget"),
        "no text was dropped: {answer}"
    );
    assert!(
        rendered <= 120 || limits.contains("exceeds the budget by about"),
        "an answer over its budget says so: {answer}"
    );
}

#[test]
fn locate_text_should_lead_with_the_status() {
    let root = fixture("text");
    let out = pixel_command()
        .args([
            "run-recipe",
            "where is `flowDir`",
            "--kind",
            "locate",
            "--no-daemon",
        ])
        .arg("--path")
        .arg(&*root)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.starts_with("locate: located — `flowDir`\n"), "{text}");
    assert!(text.contains("tests found: tests/flow.test.ts ("), "{text}");
}
