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


def run(prompt, brief=""):
    root = Path(tempfile.mkdtemp())
    files = {"pkg/api.py": "def get(url):\n    return request('get', url)\n\ndef request(method, url):\n    return Session().send()\n",
             "pkg/sessions.py": "def send(self):\n    return 'response'\n", "pkg/cookies.py": "def get(self):\n    pass\n",
             "pkg/auth.py": "def __call__(self, r):\n    return r\n", "tests/test_api.py": "def get(): pass\n"}
    for rel, text in files.items():
        (root / rel).parent.mkdir(parents=True, exist_ok=True)
        (root / rel).write_text(text)
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


if __name__ == "__main__":
    unittest.main()
