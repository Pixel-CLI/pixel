#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""The entry flow-rank starts from, and the chain it follows from an entry the question names (issue #1198).
`pixel` is a fake here: canned `find-symbol` and `who-calls` answers over a throwaway tree."""

import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

PROBE = Path(__file__).resolve().parents[1] / "probe"

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


if __name__ == "__main__":
    unittest.main()
