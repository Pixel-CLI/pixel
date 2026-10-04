// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel evaluate` end to end: the exit-code contract and the JSON shape.
//!
//! The exit codes carry a distinction prose cannot: `0` means the predicate
//! was *evaluated*, whatever it concluded, so a script must not read a
//! non-zero exit as "the answer is no". `2` is a usage error and `3` a
//! technical failure — the two cases where there is no answer at all. These
//! tests pin that, and that `--json` still emits exactly one contract object
//! on the error paths, where a caller most needs to parse rather than scrape.

use std::path::Path;
use std::process::Output;

use crate::support::{Scratch, git, pixel_command};

/// A repository whose graph is already built, since `evaluate` never builds
/// one: `work` calls `helper`, `lonely` calls nothing.
fn fixture(tag: &str) -> Scratch {
    let dir = Scratch::for_test("evaluate-cli", tag);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/util.ts"),
        "export function helper(x: number): number { return x + 1 }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/worker.ts"),
        "import { helper } from \"./util\";\n\
         export function work(n: number): number {\n  return helper(n)\n}\n\
         export function lonely(): number {\n  return 0\n}\n",
    )
    .unwrap();
    std::fs::write(dir.join(".gitignore"), ".pixel/\n").unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "baseline"]);

    rebuild_graph(&dir);
    dir
}

/// Build or rebuild the graph the way any other command does. `evaluate`
/// never does it itself: a full build can take minutes, so it reports
/// `graph_unavailable` or `graph_stale` and leaves the decision to the
/// caller.
fn rebuild_graph(dir: &Path) {
    let built = pixel_command()
        .args(["rebuild-graph"])
        .arg(dir)
        .output()
        .unwrap();
    assert!(built.status.success(), "graph build failed: {built:?}");
}

fn evaluate(dir: &Path, args: &[&str]) -> Output {
    pixel_command()
        .arg("evaluate")
        .arg("path")
        .args(args)
        .arg(dir)
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// A found path exits 0 and leads with the verdict and its scope.
#[test]
fn an_established_path_should_exit_zero_and_lead_with_the_scope() {
    let dir = fixture("established");
    let output = evaluate(&dir, &["--from", "work", "--to", "helper"]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    let first = text.lines().next().unwrap_or_default();
    assert!(
        first.starts_with("Path found in the indexed call graph"),
        "{first}"
    );
    assert!(first.contains("snapshot "), "{first}");
    assert!(
        first.contains("does not establish that the call happens at runtime")
            || text.contains("does not establish that the call happens at runtime"),
        "the runtime caveat must travel with the verdict: {text}"
    );
    // A witness nobody can read is not evidence: the hop must name the call
    // site, which is what a reader opens to check the claim themselves.
    assert!(
        text.contains("src/worker.ts:"),
        "the witness must print the call site: {text}"
    );
    assert!(
        text.contains("[exact]"),
        "each hop names the tier that resolved it: {text}"
    );
}

/// The candidates of an ambiguity are the way out of it, so they have to
/// reach the terminal, not just the JSON.
#[test]
fn an_ambiguous_name_should_print_candidate_uids() {
    let dir = fixture("ambiguous");
    std::fs::create_dir_all(dir.join("lib")).unwrap();
    std::fs::write(
        dir.join("lib/other.ts"),
        "export function helper(y: number): number { return y }\n",
    )
    .unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "second helper"]);
    // A new file in a three-file repository is drift past the incremental
    // threshold, which `evaluate` answers with `graph_stale` rather than
    // spending a full rebuild the caller did not ask for. Rebuild the way
    // the reason tells the caller to.
    rebuild_graph(&dir);

    let output = evaluate(&dir, &["--from", "work", "--to", "helper"]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    assert!(text.starts_with("Not evaluated:"), "{text}");
    assert!(
        text.contains("candidate: src/util.ts#helper#function"),
        "a candidate must be printed with the uid that resolves it: {text}"
    );
    assert!(
        text.contains("candidate: lib/other.ts#helper#function"),
        "{text}"
    );
}

/// The answer "no" is still an answer: exit 0, so a script cannot mistake a
/// negative for a failure to evaluate.
#[test]
fn an_absent_path_should_exit_zero_because_zero_means_evaluated() {
    let dir = fixture("absent");
    let output = evaluate(&dir, &["--from", "lonely", "--to", "helper"]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    assert!(
        text.starts_with("No path in the indexed call graph"),
        "{text}"
    );
    assert!(
        text.contains("traversal exhaustive"),
        "an absence must say the traversal was complete: {text}"
    );
}

/// An unknown is not an error either: the predicate was asked and refused,
/// which the caller handles by reading the reason, not the exit code.
#[test]
fn an_unknown_should_also_exit_zero_and_name_a_next_action() {
    let dir = fixture("unknown");
    let output = evaluate(&dir, &["--from", "no_such_symbol_here", "--to", "helper"]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    assert!(text.starts_with("Not evaluated:"), "{text}");
    assert!(text.contains("Next:"), "{text}");
    assert!(
        text.contains("--from was: no_such_symbol_here"),
        "the reader must see which value failed: {text}"
    );
}

/// A refused flag is a usage error: exit 2, and nothing that looks like a
/// verdict on stdout.
#[test]
fn an_unknown_tiers_value_should_exit_two_as_a_usage_error() {
    let dir = fixture("bad-tiers");
    let output = evaluate(
        &dir,
        &["--from", "work", "--to", "helper", "--tiers", "probable"],
    );

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--tiers"), "{stderr}");
    assert!(
        stdout(&output).trim().is_empty(),
        "a refused call prints no verdict: {}",
        stdout(&output)
    );
}

/// `--json` on the usage path still prints one contract object, so a caller
/// parses one shape whatever happened.
#[test]
fn a_usage_error_with_json_should_print_one_error_object_on_stdout() {
    let dir = fixture("json-usage");
    let output = evaluate(
        &dir,
        &[
            "--from", "work", "--to", "helper", "--tiers", "nonsense", "--json",
        ],
    );

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let value: serde_json::Value =
        serde_json::from_str(&stdout(&output)).expect("stdout must be one JSON object");
    assert_eq!(value["kind"], "error");
    assert_eq!(value["code"], "invalid_argument");
    assert_eq!(value["argument"], "--tiers");
}

/// The evaluation JSON is the wire contract: the honesty fields a reader
/// needs are present and named, not implied.
#[test]
fn the_json_evaluation_should_carry_the_contract_fields() {
    let dir = fixture("json-shape");
    let output = evaluate(&dir, &["--from", "work", "--to", "helper", "--json"]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let value: serde_json::Value =
        serde_json::from_str(&stdout(&output)).expect("stdout must be one JSON object");

    assert_eq!(value["kind"], "evaluation");
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["predicate"], "path");
    assert_eq!(value["status"], "established");
    assert_eq!(value["answer"], true);
    assert_eq!(value["domain"]["traversal"], "callees");
    assert_eq!(value["domain"]["tiers"][0], "exact");
    assert_eq!(
        value["snapshot"]["working_tree_check"],
        "full_before_and_after"
    );
    assert!(
        value["snapshot"]["signature"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "the answer names its generation: {value}"
    );
    // Not asserted true here: a successful traversal stops at the witness,
    // so it is *not* exhaustive. Exhaustion is the precondition of an
    // absence, and the absence test below is where it must hold.
    assert!(
        value["coverage"]["traversal_exhausted"].is_boolean(),
        "coverage must state exhaustion either way: {value}"
    );
    assert!(
        value["coverage"]["extraction_limits"]
            .as_array()
            .is_some_and(|limits| !limits.is_empty()),
        "the blind spots of extraction are stated on every answer: {value}"
    );
    assert_eq!(value["witness"]["kind"], "path");
    assert!(
        value["witness"]["edges"][0]["edge"]["site"]["line"].is_number(),
        "a witness edge names the call site that justifies it: {value}"
    );
    assert!(value["reason"].is_null());
}

/// A negative carries no witness and says what its absence is about.
#[test]
fn the_json_absence_should_carry_no_witness_and_a_bounded_claim() {
    let dir = fixture("json-absent");
    let output = evaluate(&dir, &["--from", "lonely", "--to", "helper", "--json"]);

    // The exit code first: an absence is an *evaluated* predicate, so this
    // must be 0. Parsing stdout without checking it would let the test pass
    // on a run that answered correctly and then reported a failure.
    assert_eq!(
        output.status.code(),
        Some(0),
        "an evaluated absence exits 0: {output:?}"
    );
    let value: serde_json::Value =
        serde_json::from_str(&stdout(&output)).expect("stdout must be one JSON object");
    assert_eq!(value["status"], "absent_in_snapshot");
    assert_eq!(value["answer"], false);
    assert_eq!(value["witness"]["kind"], "none");
    assert_eq!(
        value["coverage"]["traversal_exhausted"], true,
        "an absence is only an answer when the traversal exhausted: {value}"
    );
    assert!(
        value["summary"]
            .as_str()
            .is_some_and(|s| s.contains("says nothing about calls outside that relation")),
        "an absence must bound itself: {value}"
    );
}

/// `pixel evaluate` is in the action log, and its outcome there is the one
/// the exit code reports.
///
/// The command owns a three-way exit contract that `main`'s `Result`
/// cannot carry, so it hands its code back rather than calling
/// `std::process::exit` itself. Exiting inside the command would return
/// before the log is written and make `evaluate` the one command missing
/// from the journal; recording every run as a success would be just as
/// wrong, since the journal is what a later run reads to see what failed.
#[test]
fn the_action_log_should_record_an_evaluation_and_its_outcome() {
    let dir = fixture("action-log");

    let answered = evaluate(&dir, &["--from", "work", "--to", "helper"]);
    assert_eq!(answered.status.code(), Some(0), "{answered:?}");
    let refused = evaluate(
        &dir,
        &["--from", "work", "--to", "helper", "--tiers", "probable"],
    );
    assert_eq!(refused.status.code(), Some(2), "{refused:?}");

    let log = std::fs::read_to_string(dir.join(".pixel/actions.jsonl"))
        .expect("the command must reach the action log before exiting");
    let outcomes: Vec<String> = log
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("complete JSONL record"))
        .filter(|event| event["command"] == "evaluate")
        .map(|event| event["outcome"].as_str().unwrap_or_default().to_string())
        .collect();

    assert_eq!(
        outcomes,
        vec!["ok".to_string(), "error".to_string()],
        "both runs are journalled, and a usage error is not recorded as a success: {log}"
    );
}

/// `call-path` points to its successor without changing what it already
/// printed, and the command it names runs as is.
///
/// Its `found: false` cannot tell "no path" from "the depth cap cut the
/// search", so an agent that keeps calling it keeps misreading negatives;
/// the field is how one learns the replacement from the output it already
/// parses. It is additive because `call-path` stays compatible for two
/// minor versions, and it is a complete command because a suggestion that
/// no longer parses (a renamed flag), or that drops the repository it was
/// asked about, sends the agent into an error or into another repository.
/// The fixture path holds a space and an apostrophe, and the command runs
/// from another directory, so only its own quoted repository argument can
/// make it answer.
#[test]
fn call_path_should_name_a_runnable_evaluate_command_and_keep_its_fields() {
    let dir = fixture("call path's successor");
    let output = pixel_command()
        .args(["call-path", "work", "helper"])
        .arg(&*dir)
        .arg("--json")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(
        value["found"], true,
        "call-path keeps its own answer: {value}"
    );
    let command = value["successor"]["command"].as_str().unwrap_or_default();
    let argv = posix_words(command);
    let repo = dir.to_string_lossy();
    assert_eq!(
        argv,
        [
            "pixel",
            "evaluate",
            "path",
            "--from",
            "work",
            "--to",
            "helper",
            repo.as_ref(),
        ],
        "{command}"
    );

    let followed = pixel_command()
        .args(&argv[1..])
        .arg("--json")
        .current_dir(std::env::temp_dir())
        .output()
        .unwrap();
    assert_eq!(followed.status.code(), Some(0), "{followed:?}");
    let verdict: serde_json::Value = serde_json::from_slice(&followed.stdout).unwrap();
    assert_eq!(verdict["status"], "established", "{verdict}");
}

/// Split a command line the way a POSIX shell does for the forms a
/// single-quoting producer emits: blanks separate words, `'…'` is literal,
/// and a backslash outside quotes escapes the next character (the `'\''`
/// that closes, escapes and reopens around an apostrophe).
fn posix_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quoted = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                quoted = !quoted;
                in_word = true;
            }
            '\\' if !quoted => {
                word.extend(chars.next());
                in_word = true;
            }
            c if c.is_whitespace() && !quoted => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                word.push(c);
                in_word = true;
            }
        }
    }
    assert!(!quoted, "unbalanced quote in {line:?}");
    if in_word {
        words.push(word);
    }
    words
}

#[test]
fn posix_words_should_undo_single_quoting() {
    assert_eq!(
        posix_words("pixel a 'b c' 'it'\\''s' ''"),
        ["pixel", "a", "b c", "it's", ""]
    );
}
