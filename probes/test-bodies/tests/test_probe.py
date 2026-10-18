#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""What the test-bodies probe quotes for a question about tests (issue #1196)."""

import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

PROBE = Path(__file__).resolve().parents[1] / "probe"
GIT_ENV = {"GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t", "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@t",
           "GIT_CONFIG_GLOBAL": os.devnull, "PATH": os.environ["PATH"]}


def repo(files):
    root = Path(tempfile.mkdtemp())
    for rel, text in files.items():
        (root / rel).parent.mkdir(parents=True, exist_ok=True)
        (root / rel).write_text(text)
    for argv in (["init", "-q"], ["add", "."], ["commit", "-q", "-m", "fixture"]):
        subprocess.run(["git", *argv], cwd=root, env=GIT_ENV, check=True, capture_output=True)
    return root


def run(root, prompt, brief=""):
    request = {"prompt": prompt, "repo_root": str(root), "brief": brief, "kind": "tests", "session_files": []}
    proc = subprocess.run([str(PROBE)], input=json.dumps(request), capture_output=True, text=True, check=True)
    return proc.stdout


def headers(out):
    return [line for line in out.splitlines() if line.startswith("### ")]


PY_TESTS = """import pytest


def test_follows_a_redirect_to_the_new_location():
    response = get("/old", follow=True)
    assert response.redirect_target == "/new"


def test_parses_the_header_case_insensitively():
    assert parse("X-A: 1") == {"x-a": "1"}
"""


class TestBodies(unittest.TestCase):
    def test_the_case_naming_the_subject_is_quoted_whole_and_the_unrelated_case_is_not(self):
        root = repo({"tests/test_client.py": PY_TESTS, "src/client.py": "def get(path, follow=False):\n    pass\n"})
        out = run(root, "where are the tests for redirect handling and what do they cover")
        self.assertEqual(headers(out), ["### tests/test_client.py:4-6 — test test_follows_a_redirect_to_the_new_location"])
        self.assertIn('6|     assert response.redirect_target == "/new"', out)
        self.assertNotIn("parses_the_header", out)

    def test_a_rust_case_runs_to_its_closing_brace_inside_an_inline_test_module(self):
        source = ("pub fn gate(score: u32) -> bool {\n    score > 3\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n"
                  "    #[test]\n    fn the_gate_opens_above_three() {\n        assert!(gate(4));\n        assert!(!gate(3));\n"
                  "    }\n}\n")
        root = repo({"src/gate.rs": source})
        out = run(root, "which tests cover the gate", brief="### src/gate.rs:1-3 — gate\n1| pub fn gate(score: u32) -> bool {\n")
        self.assertEqual(headers(out), ["### src/gate.rs:9-13 — test the_gate_opens_above_three"])
        self.assertIn("13|     }", out)

    def test_a_word_every_case_carries_does_not_bring_them_all_in(self):
        # "check" is in every case: only the case that also names the parser is the question's.
        cases = "".join(f"def test_check_{n}():\n    check({n})\n\n\n" for n in range(4))
        cases += "def test_check_parser():\n    check(parser())\n"
        root = repo({"tests/test_a.py": cases})
        self.assertEqual(headers(run(root, "which tests check the parser")),
                         ["### tests/test_a.py:17-18 — test test_check_parser"])

    def test_a_range_the_brief_already_quotes_is_not_quoted_again(self):
        root = repo({"tests/test_client.py": PY_TESTS})
        brief = "### tests/test_client.py:1-12 — tests\n1| import pytest\n"
        self.assertEqual(run(root, "where are the tests for redirect handling", brief), "")

    def test_a_js_case_ends_at_its_closing_line_and_the_spec_file_is_found_by_its_path(self):
        spec = ("describe('undo', () => {\n  it('restores the deleted shape on undo', () => {\n    del(shape);\n"
                "    undo();\n    expect(shapes()).toContain(shape);\n  });\n\n  it('keeps the colour', () => {\n"
                "    expect(colour()).toBe('red');\n  });\n});\n")
        root = repo({"src/history.test.ts": spec})
        out = run(root, "where are the tests for undo and what behaviours do they cover")
        self.assertEqual(headers(out), ["### src/history.test.ts:2-6 — test restores the deleted shape on undo"])


if __name__ == "__main__":
    unittest.main()
