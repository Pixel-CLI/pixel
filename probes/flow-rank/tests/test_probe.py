#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""The entry flow-rank starts from, and the chain it follows from an entry the question names (issue #1198).
`pixel` is a fake here: canned `find-symbol` and `who-calls` answers over a throwaway tree."""

import json
import os
import re
import runpy
import subprocess
import tempfile
import tomllib
import unittest
from pathlib import Path

PROBE = Path(__file__).resolve().parents[1] / "probe"
MANIFEST = Path(__file__).resolve().parents[1] / "pixel-plugin.toml"
_PROBE_HELPERS = runpy.run_path(str(PROBE))
SOURCE_PATH_ORDER = _PROBE_HELPERS["prioritized_source_paths"]
RUST_CODE_MASK = _PROBE_HELPERS["rust_code_mask"]

FAKE = r'''#!/usr/bin/env python3
import json, sys
graph = json.load(open(__file__ + ".json"))
a = [x for x in sys.argv[1:] if x not in ("--metrics", "off", "--json", ".")]
if a[0] == "find-symbol":
    print(json.dumps({"symbols": [s for s in graph["symbols"] if s["name"].split(".")[-1] == a[1]]}))
elif a[0] == "who-calls":
    offset = int(a[a.index("--offset") + 1]) if "--offset" in a else 0
    edges = graph["callees"].get(a[-1], []) if offset == 0 else []
    print(json.dumps({"edges": [{"symbol": s, "tier": "exact"} for s in edges]}))
'''


def sym(uid, kind, path, lo, hi):
    return {"uid": uid, "name": uid.split("#")[1].split(".")[-1], "kind": kind, "path": path, "start_line": lo, "end_line": hi}


GET = sym("pkg/api.py#get#function", "function", "pkg/api.py", 1, 2)
REQUEST = sym("pkg/api.py#request#function", "function", "pkg/api.py", 4, 5)
SEND = sym("pkg/sessions.py#Session.send#method", "method", "pkg/sessions.py", 1, 2)
COOKIE_GET = sym("pkg/cookies.py#Jar.get#method", "method", "pkg/cookies.py", 1, 2)
TEST_GET = sym("tests/test_api.py#get#function", "function", "tests/test_api.py", 1, 1)
GUESS = sym("pkg/auth.py#__call__#method", "method", "pkg/auth.py", 1, 2)


def run(prompt, brief="", files=None):
    root = Path(tempfile.mkdtemp())
    source_files = {"pkg/api.py": "def get(url):\n    return request('get', url)\n\ndef request(method, url):\n    return Session().send()\n",
             "pkg/sessions.py": "def send(self):\n    return 'response'\n", "pkg/cookies.py": "def get(self):\n    pass\n",
             "pkg/auth.py": "def __call__(self, r):\n    return r\n", "tests/test_api.py": "def get(): pass\n"}
    source_files.update(files or {})
    for rel, text in source_files.items():
        (root / rel).parent.mkdir(parents=True, exist_ok=True)
        (root / rel).write_text(text)
    subprocess.run(["git", "init", "-q"], cwd=root, check=True)
    subprocess.run(["git", "add", "."], cwd=root, check=True)
    fake = root / "fakepixel"
    fake.write_text(FAKE)
    fake.chmod(0o755)
    graph = {"symbols": [GET, REQUEST, SEND, COOKIE_GET, TEST_GET, GUESS],
             "callees": {GET["uid"]: [REQUEST], REQUEST["uid"]: [SEND]}}
    Path(str(fake) + ".json").write_text(json.dumps(graph))
    request = {"prompt": prompt, "repo_root": str(root), "brief": brief, "kind": "flow", "session_files": []}
    return subprocess.run([str(PROBE)], input=json.dumps(request), capture_output=True, text=True, check=True,
                          env={**os.environ, "PIXEL_BIN": str(fake)}).stdout


GUESSED = "### call trace, in call order\nentry __call__ — pkg/auth.py:1-2\n"


class NamedEntry(unittest.TestCase):
    def test_the_entry_the_question_names_replaces_the_brief_guess_and_its_narrow_chain_is_followed(self):
        out = run("walk me through what happens when I call pkg.get until the answer comes back", GUESSED)
        trace = out.split("### pkg/")[0]
        self.assertEqual(trace.splitlines(), [
            "drop: .", "### call trace, ranked by the question", "entry get — pkg/api.py:1-2",
            "  calls request — pkg/api.py:4-5", "    calls send — pkg/sessions.py:1-2"])
        self.assertIn("### pkg/sessions.py:1-2 — send definition (method)", out)

    def test_a_function_whose_path_holds_the_qualifier_wins_over_a_method_and_a_test(self):
        out = run("walk me through pkg.get", GUESSED)
        self.assertIn("entry get — pkg/api.py:1-2", out)
        self.assertNotIn("cookies.py", out)
        self.assertNotIn("tests/", out)

    def test_without_a_named_entry_the_brief_entry_is_used_and_without_either_nothing_is_said(self):
        self.assertEqual(run("walk me through what happens when a request arrives"), "")

    def test_key_chord_walkthrough_follows_action_and_dispatch_source(self):
        files = {
            "src/history.ts": "export class History {\n  public undo() { return this.restorePrevious(); }\n}\n",
            "src/action.ts": "export const createUndoAction = (history) => ({\n"
                "  name: \"undo\",\n"
                "  perform: () => history.undo(),\n"
                "  keyTest: (event) => event.ctrlKey && event.key === KEYS.Z,\n"
                "});\n",
            "src/manager.ts": "handleKeyDown(event) {\n"
                "  const action = this.actions.find((action) => action.keyTest(event));\n"
                "  return action && this.executeAction(action);\n"
                "}\n",
        }
        out = run("walk me through what happens when a user presses Ctrl+Z", GUESSED, files)
        self.assertIn("Ctrl-Z action createUndoAction", out)
        self.assertIn("history.undo()", out)
        self.assertIn("action.keyTest(event)", out)
        self.assertIn("public undo()", out)

    def test_shortcut_fallback_requires_a_walkthrough_and_a_real_source_binding(self):
        files = {"src/no_shortcuts.ts": "export function save() { return true; }\n"}
        self.assertEqual(run("Ctrl+Z", files=files), "")
        self.assertEqual(run("walk me through what happens when a user presses Ctrl+Z", files=files), "")

    def test_named_undo_shortcut_without_literal_chord_resolves_action_and_dispatcher(self):
        files = {
            "src/history.ts": "export class History {\n  public undo() { return this.restorePrevious(); }\n}\n",
            "src/action.ts": "export const createUndoAction = (history) => ({\n"
                "  name: \"undo\",\n  perform: () => history.undo(),\n"
                "  keyTest: (event) => event[KEYS.CTRL_OR_CMD] && matchKey(event, KEYS.Z),\n});\n",
            "src/manager.ts": "handleKeyDown(event) {\n"
                "  const data = Object.values(this.actions).filter((action) => action.keyTest(event));\n"
                "  this.updater(data[0].perform(elements, appState));\n}\n",
        }
        prompt = "Trace the undo keyboard shortcut from keydown handling to the restored scene."
        out = run(prompt, files=files)
        self.assertIn("undo keyboard shortcut (key from source)", out)
        self.assertIn("KEYS.CTRL_OR_CMD", out)
        self.assertIn("action.keyTest(event)", out)
        self.assertIn("this.updater(data[0].perform", out)
        self.assertIn("public undo()", out)

    def test_undo_without_shortcut_intent_does_not_emit_shortcut_evidence(self):
        files = {"src/history.ts": "export function undo() { return restore(); }\n"}
        self.assertEqual(run("Trace the undo operation to the restored scene.", files=files), "")

    def test_request_flow_follows_real_router_handoff_and_abstains_on_comment(self):
        prompt = "walk me through what happens when a request arrives until the router sends the response"
        files = {
            "lib/app.js": "var app = function(req, res, next) {\n  app.handle(req, res, next);\n};\n"
                "app.handle = function handle(req, res, callback) {\n"
                "  var done = callback || finalhandler(req, res);\n"
                "  this.router.handle(req, res, done);\n}\n",
        }
        out = run(prompt, files=files)
        self.assertIn("this.router.handle(req, res, done)", out)
        self.assertIn("app.handle(req, res, next)", out)
        comment_only = {"lib/app.js": "// this.router.handle(req, res, done)\n"}
        self.assertEqual(run(prompt, files=comment_only), "")

    def test_application_completion_paraphrase_is_source_bound_and_near_miss_abstains(self):
        prompt = "How does the application callback hand an incoming HTTP request to routing and its completion callback?"
        files = {
            "lib/app.js": "var app = function(req, res, next) {\n  app.handle(req, res, next);\n};\n"
                "app.handle = function handle(req, res, callback) {\n"
                "  var done = callback || finalhandler(req, res);\n"
                "  this.router.handle(req, res, done);\n}\n",
        }
        out = run(prompt, files=files)
        self.assertIn("this.router.handle(req, res, done)", out)
        self.assertIn("app.handle(req, res, next)", out)
        self.assertEqual(run("How does the application callback hand a request to its completion callback?", files=files), "")
        self.assertEqual(run("How does the application route a request to the router?", files=files), "")

    def test_event_flow_uses_query_literal_and_requires_read_brief_and_output_chain(self):
        prompt = "walk me through what happens from the prompt-submit hook until the brief is printed"
        files = {
            "src/task_hook.rs": "enum HookEvent { PromptSubmit }\n"
                "impl HookEvent { fn as_str(self) -> &'static str { match self {\n"
                "  Self::PromptSubmit => \"prompt-submit\",\n} } }\n"
                "pub fn run(event: HookEvent) {\n  let raw = read_payload();\n"
                "  let output = if event == HookEvent::PromptSubmit { process(event, raw) } else { None };\n"
                "  let brief = start_brief(raw);\n  let text = finish_brief(brief);\n"
                "  let output = envelope(text);\n  writeln!(stdout, \"{output}\");\n}\n",
            "src/main.rs": "fn main() { task_hook::run(event); }\n",
        }
        out = run(prompt, files=files)
        self.assertIn("prompt-submit -> PromptSubmit", out)
        self.assertIn("read_payload()", out)
        self.assertIn("process(event, raw)", out)
        self.assertIn("finish_brief(brief)", out)
        self.assertIn("writeln!(stdout", out)
        self.assertIn("task_hook::run(event)", out)
        no_output = {"src/task_hook.rs": files["src/task_hook.rs"].replace('writeln!(stdout, "{output}");', "")}
        self.assertEqual(run(prompt, files=no_output), "")
        unbound_runner = {"src/task_hook.rs": files["src/task_hook.rs"].replace("run(event: HookEvent)", "run()"),
                          "src/main.rs": "fn main() { task_hook::run(); }\n"}
        self.assertEqual(run(prompt, files=unbound_runner), "")
        wrong_event_type = {"src/task_hook.rs": files["src/task_hook.rs"].replace("event: HookEvent", "event: OtherEvent")}
        self.assertEqual(run(prompt, files=wrong_event_type), "")

    def test_submitted_prompt_paraphrase_resolves_mapped_event_and_abstains_without_delivery_context(self):
        prompt = "Trace a submitted user prompt through the hook handler to the text sent back as agent context."
        files = {
            "src/task_hook.rs": "enum HookEvent { PromptSubmit }\n"
                "impl HookEvent { fn as_str(self) -> &'static str { match self {\n"
                "  Self::PromptSubmit => \"prompt-submit\",\n} } }\n"
                "pub fn run(event: HookEvent) {\n  let raw = read_payload();\n"
                "  let output = if event == HookEvent::PromptSubmit { process(event, raw) } else { None };\n"
                "  let brief = start_brief(raw);\n  let text = finish_brief(brief);\n"
                "  let output = envelope(text);\n  writeln!(stdout, \"{output}\");\n}\n",
            "src/main.rs": "fn main() { task_hook::run(event); }\n",
        }
        out = run(prompt, files=files)
        self.assertIn("prompt-submit -> PromptSubmit", out)
        self.assertIn("process(event, raw)", out)
        self.assertIn("writeln!(stdout", out)
        self.assertEqual(run("Trace a submitted user prompt through the hook handler.", files=files), "")

    def test_event_mapping_inside_raw_string_does_not_prove_active_name_mapping(self):
        prompt = "walk me through what happens from the prompt-submit hook until the brief is printed"
        source = "enum HookEvent { PromptSubmit }\n"
        source += 'const DOC: &str = r#"Self::PromptSubmit => "prompt-submit""#;\n'
        source += "pub fn run(event: HookEvent) {\n"
        source += "  let output = process(event, read_payload());\n"
        source += '  writeln!(stdout, "{output}");\n}\n'
        files = {"src/task_hook.rs": source, "src/main.rs": "fn main() { task_hook::run(event); }\n"}
        self.assertEqual(run(prompt, files=files), "")

    def test_same_line_active_non_mapping_arm_does_not_bind_later_raw_string(self):
        prompt = "walk me through what happens from the prompt-submit hook until the brief is printed"
        source = "enum HookEvent { PromptSubmit }\n"
        source += 'fn route(event: HookEvent) { match event { Self::PromptSubmit => { let _doc = r#"Self::PromptSubmit => "prompt-submit""#; process(event, read_payload()); } } }\n'
        source += "pub fn run(event: HookEvent) {\n"
        source += "  let output = process(event, read_payload());\n"
        source += '  writeln!(stdout, "{output}");\n}\n'
        files = {"src/task_hook.rs": source, "src/main.rs": "fn main() { task_hook::run(event); }\n"}
        self.assertEqual(run(prompt, files=files), "")


class RustSourceMask(unittest.TestCase):
    def test_late_raw_string_masks_without_recomputing_absolute_match_offset(self):
        source = "// filler\n" * 100 + 'const DOC: &str = r#"fake event"#;\nfn after() {}\n'
        masked = RUST_CODE_MASK(source)
        self.assertEqual(len(masked), len(source))
        self.assertNotIn("fake event", masked)
        self.assertIn("fn after() {}", masked)


class ManifestAdmission(unittest.TestCase):
    def test_when_prompt_avoids_lookaround_unsupported_by_native_regex(self):
        with MANIFEST.open("rb") as manifest_file:
            pattern = tomllib.load(manifest_file)["brief-probe"]["when_prompt"]
        for unsupported in ("(?=", "(?!", "(?<=", "(?<!"):
            with self.subTest(unsupported=unsupported):
                self.assertNotIn(unsupported, pattern)

    def test_paraphrased_flow_forms_admit_but_near_misses_do_not(self):
        with MANIFEST.open("rb") as manifest_file:
            pattern = re.compile(tomllib.load(manifest_file)["brief-probe"]["when_prompt"])
        positives = [
            "Trace a submitted user prompt through the hook handler to the text sent back as agent context.",
            "Trace the undo keyboard shortcut from keydown handling to the restored scene.",
            "How does the application callback hand an incoming HTTP request to routing and its completion callback?",
        ]
        negatives = [
            "What is a submitted prompt?",
            "How does the keyboard work?",
            "How does the application callback hand a request to its completion callback?",
            "Where is routing configured?",
        ]
        for prompt in positives:
            with self.subTest(prompt=prompt):
                self.assertRegex(prompt, pattern)
        for prompt in negatives:
            with self.subTest(prompt=prompt):
                self.assertNotRegex(prompt, pattern)

    def test_trace_scan_prioritizes_prompt_terms_and_top_level_caller(self):
        listing = b"lib/unrelated.js\0src/main.rs\0src/task_hook.rs\0"
        ordered = SOURCE_PATH_ORDER(
            listing,
            "Trace a submitted user prompt through the hook handler to text sent back as agent context.",
            {".rs", ".js"},
        )
        self.assertEqual(set(ordered[:2]), {b"src/main.rs", b"src/task_hook.rs"})


if __name__ == "__main__":
    unittest.main()
