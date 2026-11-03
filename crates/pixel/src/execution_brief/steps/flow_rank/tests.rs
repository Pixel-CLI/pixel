// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The cases of `probes/flow-rank/tests/test_probe.py`, run on the step: the
//! entry flow-rank starts from, the chain it follows from an entry the
//! question names (issue #1198), and the four walkthroughs it answers from
//! the source. A fake daemon answers `symbol` and `uses`.

use serde_json::{Value, json};

use super::super::graph::testing::{FakeGraph, sym};
use super::super::testing::repo;
use super::*;

/// A symbol the way the script's fake printed it: the name is the last
/// dotted segment of the uid's second part.
fn symbol(uid: &str, kind: &str, path: &str, lo: u32, hi: u32) -> Value {
    let name = uid.split('#').nth(1).unwrap().rsplit('.').next().unwrap();
    sym(uid, name, kind, path, (lo, hi))
}

fn graph() -> (Vec<Value>, Vec<(&'static str, Vec<Value>)>) {
    let get = symbol("pkg/api.py#get#function", "function", "pkg/api.py", 1, 2);
    let request = symbol("pkg/api.py#request#function", "function", "pkg/api.py", 4, 5);
    let send = symbol("pkg/sessions.py#Session.send#method", "method", "pkg/sessions.py", 1, 2);
    let cookie = symbol("pkg/cookies.py#Jar.get#method", "method", "pkg/cookies.py", 1, 2);
    let test_get = symbol("tests/test_api.py#get#function", "function", "tests/test_api.py", 1, 1);
    let guess = symbol("pkg/auth.py#__call__#method", "method", "pkg/auth.py", 1, 2);
    (
        vec![get, request.clone(), send.clone(), cookie, test_get, guess],
        vec![("pkg/api.py#get#function", vec![request]), ("pkg/api.py#request#function", vec![send])],
    )
}

const GUESSED: &str = "### call trace, in call order\nentry __call__ — pkg/auth.py:1-2\n";

fn run_in(prompt: &str, brief: &str, files: &[(&str, &str)]) -> String {
    let mut all: Vec<(&str, &str)> = vec![
        (
            "pkg/api.py",
            "def get(url):\n    return request('get', url)\n\ndef request(method, url):\n    return Session().send()\n",
        ),
        ("pkg/sessions.py", "def send(self):\n    return 'response'\n"),
        ("pkg/cookies.py", "def get(self):\n    pass\n"),
        ("pkg/auth.py", "def __call__(self, r):\n    return r\n"),
        ("tests/test_api.py", "def get(): pass\n"),
    ];
    for (path, text) in files {
        all.retain(|(known, _)| known != path);
        all.push((path, text));
    }
    let dir = repo(&all);
    let (symbols, callees) = graph();
    let _daemon = FakeGraph::serve(dir.path(), move |request| match request["op"].as_str() {
        Some("symbol") => {
            let wanted = request["name"].as_str().unwrap_or_default();
            let found: Vec<&Value> = symbols
                .iter()
                .filter(|row| row["name"].as_str().unwrap_or_default().rsplit('.').next() == Some(wanted))
                .collect();
            json!({"symbols": found})
        }
        Some("uses") => {
            let uid = request["uid_or_name"].as_str().unwrap_or_default();
            let edges: Vec<Value> = callees
                .iter()
                .filter(|(from, _)| *from == uid && request["offset"] == 0)
                .flat_map(|(_, to)| to.iter().map(|row| json!({"symbol": row, "tier": "exact"})))
                .collect();
            json!({"edges": edges})
        }
        _ => json!({}),
    });
    super::super::testing::run(&STEP, dir.path(), prompt, brief)
}

fn ask(prompt: &str) -> String {
    run_in(prompt, "", &[])
}

#[test]
fn the_entry_the_question_names_replaces_the_brief_guess_and_its_narrow_chain_is_followed() {
    let out = run_in(
        "walk me through what happens when I call pkg.get until the answer comes back",
        GUESSED,
        &[],
    );
    let trace = out.split("### pkg/").next().unwrap();
    assert_eq!(
        trace.lines().collect::<Vec<_>>(),
        [
            "drop: .",
            "### call trace, ranked by the question",
            "entry get — pkg/api.py:1-2",
            "  calls request — pkg/api.py:4-5",
            "    calls send — pkg/sessions.py:1-2",
        ]
    );
    assert!(out.contains("### pkg/sessions.py:1-2 — send definition (method)\n"), "{out}");
    assert!(
        out.contains(
            "### pkg/api.py:1-2 — get definition (method)\n1| def get(url):\n2|     return request('get', url)\n### pkg/api.py:4-5 — request definition (function)\n4| def request(method, url):\n5|     return Session().send()\n"
        ),
        "{out}"
    );
}

#[test]
fn a_function_whose_path_holds_the_qualifier_wins_over_a_method_and_a_test() {
    let out = run_in("walk me through pkg.get", GUESSED, &[]);
    assert!(out.contains("entry get — pkg/api.py:1-2"), "{out}");
    assert!(!out.contains("cookies.py"), "{out}");
    assert!(!out.contains("tests/"), "{out}");
}

#[test]
fn without_a_named_entry_the_brief_entry_is_used_and_without_either_nothing_is_said() {
    assert_eq!(ask("walk me through what happens when a request arrives"), "");
    let out = run_in("walk me through what happens", GUESSED, &[]);
    assert_eq!(out, "", "the brief's entry has no callees in the graph");
    let brief = "### call trace, in call order\nentry get — pkg/api.py:1-2\n";
    let out = run_in("walk me through what the request does", brief, &[]);
    assert!(out.starts_with("drop: .\n### call trace, ranked by the question\nentry get — pkg/api.py:1-2\n"), "{out}");
    let no_trace = "entry get — pkg/api.py:1-2\n";
    assert_eq!(run_in("walk me through what happens", no_trace, &[]), "", "only a call trace's entry counts");
}

#[test]
fn key_chord_walkthrough_follows_action_and_dispatch_source() {
    let files = [
        (
            "src/history.ts",
            "export class History {\n  public undo() { return this.restorePrevious(); }\n}\n",
        ),
        (
            "src/action.ts",
            "export const createUndoAction = (history) => ({\n  name: \"undo\",\n  perform: () => history.undo(),\n  keyTest: (event) => event.ctrlKey && event.key === KEYS.Z,\n});\n",
        ),
        (
            "src/manager.ts",
            "handleKeyDown(event) {\n  const action = this.actions.find((action) => action.keyTest(event));\n  return action && this.executeAction(action);\n}\n",
        ),
    ];
    let out = run_in("walk me through what happens when a user presses Ctrl+Z", GUESSED, &files);
    assert!(out.starts_with("drop: .\n### shortcut flow: Ctrl-Z action createUndoAction\n"), "{out}");
    assert!(out.contains("### src/action.ts:1-5 — key action createUndoAction\n"), "{out}");
    assert!(out.contains("history.undo()"), "{out}");
    assert!(out.contains("action.keyTest(event)"), "{out}");
    assert!(out.contains("### src/history.ts:1-3 — undo handler\n"), "{out}");
    assert!(out.contains("public undo()"), "{out}");
}

#[test]
fn shortcut_fallback_requires_a_walkthrough_and_a_real_source_binding() {
    let files = [("src/no_shortcuts.ts", "export function save() { return true; }\n")];
    assert_eq!(run_in("Ctrl+Z", "", &files), "");
    assert_eq!(run_in("walk me through what happens when a user presses Ctrl+Z", "", &files), "");
}

#[test]
fn named_undo_shortcut_without_literal_chord_resolves_action_and_dispatcher() {
    let files = [
        (
            "src/history.ts",
            "export class History {\n  public undo() { return this.restorePrevious(); }\n}\n",
        ),
        (
            "src/action.ts",
            "export const createUndoAction = (history) => ({\n  name: \"undo\",\n  perform: () => history.undo(),\n  keyTest: (event) => event[KEYS.CTRL_OR_CMD] && matchKey(event, KEYS.Z),\n});\n",
        ),
        (
            "src/manager.ts",
            "handleKeyDown(event) {\n  const data = Object.values(this.actions).filter((action) => action.keyTest(event));\n  this.updater(data[0].perform(elements, appState));\n}\n",
        ),
    ];
    let prompt = "Trace the undo keyboard shortcut from keydown handling to the restored scene.";
    let out = run_in(prompt, "", &files);
    assert!(out.contains("### shortcut flow: undo keyboard shortcut (key from source) action createUndoAction\n"), "{out}");
    assert!(out.contains("KEYS.CTRL_OR_CMD"), "{out}");
    assert!(out.contains("### src/manager.ts:1-4 — action dispatcher selection and execution\n"), "{out}");
    assert!(out.contains("action.keyTest(event)"), "{out}");
    assert!(out.contains("this.updater(data[0].perform"), "{out}");
    assert!(out.contains("public undo()"), "{out}");
}

#[test]
fn undo_without_shortcut_intent_does_not_emit_shortcut_evidence() {
    let files = [("src/history.ts", "export function undo() { return restore(); }\n")];
    assert_eq!(run_in("Trace the undo operation to the restored scene.", "", &files), "");
}

const APP: &str = "var app = function(req, res, next) {\n  app.handle(req, res, next);\n};\napp.handle = function handle(req, res, callback) {\n  var done = callback || finalhandler(req, res);\n  this.router.handle(req, res, done);\n}\n";

#[test]
fn request_flow_follows_real_router_handoff_and_abstains_on_comment() {
    let prompt = "walk me through what happens when a request arrives until the router sends the response";
    let out = run_in(prompt, "", &[("lib/app.js", APP)]);
    assert!(out.starts_with("drop: .\n### request flow: app delegates request/response to router\n### lib/app.js:1-7 — app-to-router handoff\n"), "{out}");
    assert!(out.contains("this.router.handle(req, res, done)"), "{out}");
    assert!(out.contains("### lib/app.js:1-7 — application request entry\n1| var app = function(req, res, next) {\n"), "{out}");
    assert!(out.contains("app.handle(req, res, next)"), "{out}");
    let comment_only = [("lib/app.js", "// this.router.handle(req, res, done)\n")];
    assert_eq!(run_in(prompt, "", &comment_only), "");
}

#[test]
fn application_completion_paraphrase_is_source_bound_and_near_miss_abstains() {
    let prompt = "How does the application callback hand an incoming HTTP request to routing and its completion callback?";
    let files = [("lib/app.js", APP)];
    let out = run_in(prompt, "", &files);
    assert!(out.contains("this.router.handle(req, res, done)"), "{out}");
    assert!(out.contains("app.handle(req, res, next)"), "{out}");
    let one_callback = "How does the application callback hand a request to its completion callback?";
    assert_eq!(run_in(one_callback, "", &files), "");
    let no_callback = "How does the application route a request to the router?";
    assert_eq!(run_in(no_callback, "", &files), "");
}

const HOOK: &str = "enum HookEvent { PromptSubmit }
impl HookEvent { fn as_str(self) -> &'static str { match self {
  Self::PromptSubmit => \"prompt-submit\",
} } }
pub fn run(event: HookEvent) {
  let raw = read_payload();
  let output = if event == HookEvent::PromptSubmit { process(event, raw) } else { None };
  let brief = start_brief(raw);
  let text = finish_brief(brief);
  let output = envelope(text);
  writeln!(stdout, \"{output}\");
}
";
const MAIN: &str = "fn main() { task_hook::run(event); }\n";

#[test]
fn event_flow_uses_query_literal_and_requires_read_brief_and_output_chain() {
    let prompt = "walk me through what happens from the prompt-submit hook until the brief is printed";
    let out = run_in(prompt, "", &[("src/task_hook.rs", HOOK), ("src/main.rs", MAIN)]);
    assert!(out.starts_with("drop: .\n### event flow: prompt-submit -> PromptSubmit -> hook output\n"), "{out}");
    for header in [
        "### src/task_hook.rs:1-6 — typed event and host-name mapping",
        "### src/task_hook.rs:5-7 — runner receives typed event",
        "### src/task_hook.rs:5-10 — runner passes event to processing",
        "### src/task_hook.rs:5-12 — brief processing call chain",
        "### src/task_hook.rs:5-10 — hook reads the incoming event",
        "### src/task_hook.rs:6-12 — hook writes and flushes output",
        "### src/main.rs:1-1 — command passes event to hook runner",
    ] {
        assert!(out.contains(&format!("{header}\n")), "{header}\n{out}");
    }
    assert!(out.contains("read_payload()"), "{out}");
    assert!(out.contains("process(event, raw)"), "{out}");
    assert!(out.contains("finish_brief(brief)"), "{out}");
    assert!(out.contains("writeln!(stdout"), "{out}");
    assert!(out.contains("task_hook::run(event)"), "{out}");

    let no_output = HOOK.replace("writeln!(stdout, \"{output}\");", "");
    assert_eq!(run_in(prompt, "", &[("src/task_hook.rs", &no_output)]), "");
    let unbound = HOOK.replace("run(event: HookEvent)", "run()");
    let files = [("src/task_hook.rs", unbound.as_str()), ("src/main.rs", "fn main() { task_hook::run(); }\n")];
    assert_eq!(run_in(prompt, "", &files), "");
    let wrong_type = HOOK.replace("event: HookEvent", "event: OtherEvent");
    assert_eq!(run_in(prompt, "", &[("src/task_hook.rs", &wrong_type)]), "");
}

#[test]
fn the_runner_call_that_passes_no_event_is_not_the_command_that_passes_one() {
    let prompt = "walk me through what happens from the prompt-submit hook until the brief is printed";
    let main = "fn first() { task_hook::run(); }\n\n\n\n\n\nfn main() { task_hook::run(event); }\n";
    let out = run_in(prompt, "", &[("src/task_hook.rs", HOOK), ("src/main.rs", main)]);
    assert!(out.contains("### src/main.rs:4-7 — command passes event to hook runner\n"), "{out}");
    assert!(!out.contains("### src/main.rs:1-"), "{out}");
}

#[test]
fn submitted_prompt_paraphrase_resolves_mapped_event_and_abstains_without_delivery_context() {
    let prompt = "Trace a submitted user prompt through the hook handler to the text sent back as agent context.";
    let files = [("src/task_hook.rs", HOOK), ("src/main.rs", MAIN)];
    let out = run_in(prompt, "", &files);
    assert!(out.contains("prompt-submit -> PromptSubmit"), "{out}");
    assert!(out.contains("process(event, raw)"), "{out}");
    assert!(out.contains("writeln!(stdout"), "{out}");
    assert_eq!(run_in("Trace a submitted user prompt through the hook handler.", "", &files), "");
}

#[test]
fn event_mapping_inside_raw_string_does_not_prove_active_name_mapping() {
    let prompt = "walk me through what happens from the prompt-submit hook until the brief is printed";
    let source = "enum HookEvent { PromptSubmit }\nconst DOC: &str = r#\"Self::PromptSubmit => \"prompt-submit\"\"#;\npub fn run(event: HookEvent) {\n  let output = process(event, read_payload());\n  writeln!(stdout, \"{output}\");\n}\n";
    assert_eq!(run_in(prompt, "", &[("src/task_hook.rs", source), ("src/main.rs", MAIN)]), "");
}

#[test]
fn same_line_active_non_mapping_arm_does_not_bind_later_raw_string() {
    let prompt = "walk me through what happens from the prompt-submit hook until the brief is printed";
    let source = "enum HookEvent { PromptSubmit }\nfn route(event: HookEvent) { match event { Self::PromptSubmit => { let _doc = r#\"Self::PromptSubmit => \"prompt-submit\"\"#; process(event, read_payload()); } } }\npub fn run(event: HookEvent) {\n  let output = process(event, read_payload());\n  writeln!(stdout, \"{output}\");\n}\n";
    assert_eq!(run_in(prompt, "", &[("src/task_hook.rs", source), ("src/main.rs", MAIN)]), "");
}

#[test]
fn the_mask_blanks_comments_and_strings_and_keeps_every_offset() {
    let source = format!("{}const DOC: &str = r#\"fake event\"#;\nfn after() {{}}\n", "// filler\n".repeat(100));
    let masked = rust_code_mask(&source);
    assert_eq!(masked.chars().count(), source.chars().count());
    assert!(!masked.contains("fake event"));
    assert!(masked.contains("fn after() {}"));
    let mixed = "let a = \"x\\\"y\"; /* c /* nested */ d */ let b = 'z'; // tail\nbr#\"raw\"# end";
    assert_eq!(
        rust_code_mask(mixed),
        "let a =       ;                        let b = 'z';        \n          end"
    );
}

#[test]
fn the_step_applies_to_paraphrased_flow_forms_and_not_to_near_misses() {
    assert!(!STEP.when_prompt.unwrap().contains("(?="), "no lookaround in a native regex");
    for prompt in [
        "Trace a submitted user prompt through the hook handler to the text sent back as agent context.",
        "Trace the undo keyboard shortcut from keydown handling to the restored scene.",
        "How does the application callback hand an incoming HTTP request to routing and its completion callback?",
    ] {
        assert!(STEP.applies(None, prompt, true), "{prompt}");
    }
    for prompt in [
        "What is a submitted prompt?",
        "How does the keyboard work?",
        "How does the application callback hand a request to its completion callback?",
        "Where is routing configured?",
    ] {
        assert!(!STEP.applies(None, prompt, true), "{prompt}");
    }
    assert!(STEP.applies(Some("flow"), "anything", false), "a flow question is enough");
}

#[test]
fn trace_scan_prioritizes_prompt_terms_and_top_level_caller() {
    let files: Vec<String> = ["lib/unrelated.js", "src/main.rs", "src/task_hook.rs"]
        .map(ToString::to_string)
        .to_vec();
    let prompt = "Trace a submitted user prompt through the hook handler to text sent back as agent context.";
    let ordered = prioritized_source_paths(&files, prompt);
    let first_two: BTreeSet<&str> = ordered.iter().take(2).copied().collect();
    assert_eq!(first_two, BTreeSet::from(["src/main.rs", "src/task_hook.rs"]));
    assert_eq!(ordered, ["src/main.rs", "src/task_hook.rs", "lib/unrelated.js"]);
    let camel = vec!["src/actionHistory.ts".to_owned(), "src/zeta.ts".to_owned(), "src/a.md".to_owned()];
    assert_eq!(
        prioritized_source_paths(&camel, "what does the action history do"),
        ["src/actionHistory.ts", "src/zeta.ts"],
        "camel case is split, a non-source file is left out"
    );
}

#[test]
fn a_suffix_is_pythons_not_the_last_dot_of_the_path() {
    assert_eq!(suffix("src/a.tsx"), ".tsx");
    assert_eq!(suffix("a.d/readme"), "");
    assert_eq!(suffix(".ts"), "");
    assert_eq!(suffix("name."), "");
    assert_eq!(suffix("x.test.js"), ".js");
}
