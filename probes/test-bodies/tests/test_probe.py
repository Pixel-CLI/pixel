#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""What the test-bodies probe quotes for a question about tests (issue #1196)."""

import json
import importlib.util
import os
import runpy
import subprocess
import tempfile
import unittest
from pathlib import Path

PROBE = Path(__file__).resolve().parents[1] / "probe"
WORKTREE = PROBE.parents[2]
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


def run(root, prompt, brief="", kind="tests"):
    request = {"prompt": prompt, "repo_root": str(root), "brief": brief, "kind": kind, "session_files": []}
    proc = subprocess.run([str(PROBE)], input=json.dumps(request), capture_output=True, text=True, check=True)
    return proc.stdout


def headers(out):
    return [line for line in out.splitlines() if line.startswith("### ")]


PY_TESTS = """import pytest


def test_follows_a_redirect_and_handles_the_new_location():
    response = get("/old", follow=True)
    assert response.redirect_target == "/new"


def test_parses_the_header_case_insensitively():
    assert parse("X-A: 1") == {"x-a": "1"}
"""


class TestBodies(unittest.TestCase):
    def test_subject_matching_uses_words_and_inflections_not_arbitrary_prefixes(self):
        path = WORKTREE / "probes/test-bodies/probe"
        probe = runpy.run_path(str(path))

        self.assertTrue(probe["carries"](probe["tokens"]("redirects handles balanced"), "redirect"))
        self.assertTrue(probe["carries"](probe["tokens"]("redirects handles balanced"), "handl"))
        self.assertTrue(probe["carries"](probe["tokens"]("redirects handles balanced"), "balance"))
        self.assertFalse(probe["carries"](probe["tokens"]("candidate production"), "can"))
        self.assertFalse(probe["carries"](probe["tokens"]("candidate production"), "product"))

    def test_the_tests_probe_can_run_for_a_matching_closed_gate_prompt(self):
        path = WORKTREE / "scripts/bench/probe-test.py"
        spec = importlib.util.spec_from_file_location("probe_test", path)
        probe_test = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(probe_test)
        probe = probe_test.Probe(PROBE.parent)
        root = repo({"tests/test_client.py": PY_TESTS})
        prompt = "where are the tests for redirect handling and what do they cover"

        self.assertEqual(probe.problems, [])
        self.assertTrue(probe_test.gate_runs("closed", probe) and probe.applies("tests", prompt))
        self.assertFalse(probe_test.gate_runs("denied", probe))
        request = {"prompt": prompt, "repo_root": str(root), "brief": "[PIXEL:BRIEF]\n",
                   "kind": "tests", "session_files": []}
        output, _elapsed, error = probe_test.run_probe(probe, request, Path("/bin/true"), root)
        self.assertIsNone(error)
        merged = probe_test.merge(request["brief"], [(probe.name, probe.mode, output)])
        self.assertIn("### tests/test_client.py:4-6 — test test_follows_a_redirect_and_handles_the_new_location", merged)
        self.assertIn('6|     assert response.redirect_target == "/new"', merged)

    def test_the_case_naming_the_subject_is_quoted_whole_and_the_unrelated_case_is_not(self):
        root = repo({"tests/test_client.py": PY_TESTS, "src/client.py": "def get(path, follow=False):\n    pass\n"})
        out = run(root, "where are the tests for redirect handling and what do they cover")
        self.assertEqual(headers(out), ["### tests/test_client.py:4-6 — test test_follows_a_redirect_and_handles_the_new_location"])
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

    def test_an_exact_option_name_does_not_select_a_sibling_option_case(self):
        root = repo({"tests/options.rs": '''
#[test]
fn test_exec() {
    run(&["--exec", "echo", "{}"]).assert_stdout("one.txt");
    run(&["--exec", "echo", "{/.}"]).assert_stdout("one");
}

#[test]
fn test_exec_batch() {
    run(&["--exec-batch", "echo", "{}"]).assert_stdout("one.txt");
}
'''})
        exact = run(root, "where are the integration tests for the exec option and what behaviours do they cover")
        self.assertEqual(headers(exact), ["### tests/options.rs:2-6 — test test_exec"])
        self.assertIn('--exec", "echo"', exact)
        self.assertNotIn("test_exec_batch", exact)

        batch = run(root, "where are the integration tests for the --exec-batch option")
        self.assertEqual(headers(batch), ["### tests/options.rs:8-11 — test test_exec_batch"])
        self.assertIn('--exec-batch', batch)
        self.assertNotIn("test_exec()", batch)

    def test_exact_option_selector_generalizes_to_other_option_names(self):
        root = repo({"tests/options.rs": '''
#[test]
fn test_search_depth() {
    run(&["--search-depth", "4"]).assert_depth(4);
}

#[test]
fn test_search_depth_default() {
    run(&["--search-depth"]).assert_depth(8);
}
'''})
        out = run(root, "where are the integration tests for the --search-depth option")
        self.assertEqual(headers(out), ["### tests/options.rs:2-5 — test test_search_depth"])

    def test_exact_option_requires_title_or_exact_flag_evidence(self):
        root = repo({"tests/options.rs": '''
#[test]
fn test_exec_batch() {
    run(&["--exec-batch", "echo", "{}"]).assert_stdout("one.txt");
}
'''})
        self.assertEqual("", run(root, "where are the tests for the exec option"))

        root = repo({"tests/options.rs": '''
#[test]
fn test_uses_command_override() {
    run(&["--exec", "echo", "{}"]).assert_stdout("one.txt");
}
'''})
        out = run(root, "where are the tests for the exec option")
        self.assertEqual(headers(out), ["### tests/options.rs:2-5 — test test_uses_command_override"])

        root = repo({"tests/options.rs": '''
#[test]
fn test_exec() {
    run(&["--exec", "echo", "{}"]).assert_stdout("one.txt");
}

#[test]
fn test_exec_batch() {
    run(&["--exec-batch", "echo", "{}"]).assert_stdout("one.txt");
}
'''})
        brief = "\n".join([
            "### tests matching packet (2 found)",
            "tests/options.rs:2 test_exec",
            "tests/options.rs:7 test_exec_batch",
        ])
        out = run(root, "where are the tests for the exec option", brief)
        self.assertEqual(headers(out), ["### tests/options.rs:2-5 — test test_exec"])

    def test_listed_tests_survive_ranking_while_other_matches_keep_the_relative_cutoff(self):
        root = repo({"tests/options.rs": '''
#[test]
fn internal_variant() {
    run_variant("alpha").assert_code(1);
}

#[test]
fn edge_branch() {
    run_variant("beta").assert_code(2);
}

#[test]
fn test_cache_expiration_behaviors_and_assertions() {
    run_cache_expiration().assert_expired_after_limit();
}

#[test]
fn test_cache_is_not_visible_after_reload() {
    reload_cache().assert_expired();
}
'''})
        brief = "\n".join([
            "### tests of cache state",
            "tests/options.rs:2 internal_variant",
            "tests/options.rs:7 edge_branch",
        ])
        out = run(root, "where are the tests for cache expiration behaviors and assertions", brief)
        self.assertIn("internal_variant", out)
        self.assertIn("edge_branch", out)
        self.assertIn("test_cache_expiration_behaviors_and_assertions", out)
        self.assertNotIn("test_cache_is_not_visible_after_reload", out)

    def test_a_high_scoring_listed_case_does_not_lower_the_unlisted_cutoff(self):
        source = """def test_cache_expiration_behaviors_and_assertions():
    run_cache_expiration().assert_expired_after_limit()

def test_cache_reload_expiration():
    reload_cache().assert_expired()
"""
        root = repo({"tests/test_cache.py": source})
        brief = "tests/test_cache.py:1 test_cache_expiration_behaviors_and_assertions — assertion\n"
        out = run(root, "where are the tests for cache expiration behaviors and assertions", brief)
        self.assertIn("test_cache_expiration_behaviors_and_assertions", out)
        self.assertNotIn("test_cache_reload_expiration", out)

    def test_multiword_javascript_title_named_by_brief_survives_lexical_ranking(self):
        root = repo({"tests/ui.test.js": "it('keeps query values', () => {\n  expect(renderQuery()).toEqual('x=1');\n});\n"})
        brief = "tests/ui.test.js:1 keeps query values — expect(renderQuery()).toEqual('x=1')"
        out = run(root, "where are the tests for orphan handling", brief)
        self.assertEqual(headers(out), ["### tests/ui.test.js:1-3 — test keeps query values"])

    def test_inventory_line_selects_only_one_duplicate_javascript_title(self):
        source = """it('keeps query values', () => {
    expect(renderQuery()).toEqual('x=1');
});

it('keeps query values', () => {
    expect(renderQuery()).toEqual('unrelated');
});
"""
        root = repo({"tests/ui.test.js": source})
        brief = "tests/ui.test.js:1 keeps query values — expect(renderQuery()).toEqual('x=1')\n"
        out = run(root, "where are the tests for orphan handling", brief)
        self.assertIn("### tests/ui.test.js:1-3", out)
        self.assertNotIn("unrelated", out)

    def test_a_javascript_title_with_an_em_dash_matches_the_full_inventory_title(self):
        root = repo({"tests/ui.test.js": "it('keeps query — values', () => {\n  expect(renderQuery()).toEqual('x=1');\n});\n"})
        brief = "tests/ui.test.js:1 keeps query — values — expect(renderQuery()).toEqual('x=1')\n"
        out = run(root, "where are the tests for orphan handling", brief)
        self.assertEqual(headers(out), ["### tests/ui.test.js:1-3 — test keeps query — values"])

    def test_a_js_case_ends_at_its_closing_line_and_the_spec_file_is_found_by_its_path(self):
        spec = ("describe('undo', () => {\n  it('restores the deleted shape on undo', () => {\n    del(shape);\n"
                "    undo();\n    expect(shapes()).toContain(shape);\n    // The observable behaviours stay stable.\n  });\n\n  it('keeps the colour', () => {\n"
                "    expect(colour()).toBe('red');\n  });\n});\n")
        root = repo({"src/history.test.ts": spec})
        out = run(root, "where are the tests for undo and what behaviours do they cover")
        self.assertEqual(headers(out), ["### src/history.test.ts:2-7 — test restores the deleted shape on undo"])

    def test_weak_closed_gate_test_word_does_not_admit_or_emit_unrelated_cases(self):
        path = WORKTREE / "scripts/bench/probe-test.py"
        spec = importlib.util.spec_from_file_location("probe_test_negative", path)
        probe_test = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(probe_test)
        probe = probe_test.Probe(PROBE.parent)
        prompts = [
            "How do I test whether a dragon can balance a teacup?",
            "Can you test whether our release mascot can drink tea?",
            "Please test a concept for our product workshop.",
        ]
        root = repo({"tests/test_candidate_policy.py": (
            "def test_candidate_policy_stays_explicit():\n    assert balance_is_stable()\n\n"
            "def test_general_case():\n    assert can_run()\n"
        )})

        for prompt in prompts:
            with self.subTest(prompt=prompt):
                self.assertFalse(probe.applies(None, prompt))
                self.assertEqual("", run(root, prompt, kind=None))
                self.assertEqual("", run(root, prompt, kind="tests"))


class Symlinks(unittest.TestCase):
    def ask(self, link, target):
        root = Path(tempfile.mkdtemp())
        (root / link).parent.mkdir(parents=True, exist_ok=True)
        (root / link).symlink_to(target)
        for argv in (["init", "-q"], ["add", "."], ["commit", "-q", "-m", "fixture"]):
            subprocess.run(["git", *argv], cwd=root, env=GIT_ENV, check=True, capture_output=True)
        request = {"prompt": "which tests cover the redirect location", "repo_root": str(root), "brief": "",
                   "kind": "tests", "session_files": []}
        return subprocess.run([str(PROBE)], input=json.dumps(request), capture_output=True, text=True, check=True,
                              timeout=20).stdout

    def test_a_tracked_symlink_to_a_file_outside_the_repo_is_not_read(self):
        outside = Path(tempfile.mkdtemp()) / "test_outside.py"
        outside.write_text("def test_redirect_location():\n    assert SECRET_TOKEN\n")
        out = self.ask("tests/test_redirect.py", outside)
        self.assertNotIn("SECRET_TOKEN", out)
        self.assertEqual(headers(out), [])

    def test_a_tracked_symlink_to_dev_zero_neither_hangs_nor_quotes(self):
        self.assertEqual(headers(self.ask("tests/test_redirect.py", "/dev/zero")), [])


if __name__ == "__main__":
    unittest.main()
