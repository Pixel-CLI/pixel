#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Module-name matching stays anchored to tracked source and its users."""

import json
import importlib.util
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

PROBE = Path(__file__).resolve().parents[1] / "probe"
GIT_ENV = {"GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t", "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@t",
           "GIT_CONFIG_GLOBAL": os.devnull, "PATH": os.environ["PATH"]}


def repo(files):
    root = Path(tempfile.mkdtemp())
    for rel, text in files.items():
        path = root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    for argv in (["init", "-q"], ["add", "."], ["commit", "-q", "-m", "fixture"]):
        subprocess.run(["git", *argv], cwd=root, env=GIT_ENV, check=True, capture_output=True)
    return root


def run(root, prompt, kind="impact"):
    request = {"prompt": prompt, "repo_root": str(root), "brief": "", "kind": kind, "session_files": []}
    proc = subprocess.run([str(PROBE)], input=json.dumps(request), capture_output=True, text=True, check=True)
    return proc.stdout


def probe_test_module():
    path = Path(__file__).resolve().parents[3] / "scripts/bench/probe-test.py"
    spec = importlib.util.spec_from_file_location("probe_test", path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


class TestModuleReferences(unittest.TestCase):
    FILES = {
        "src/exit_codes.rs": "pub enum ExitCode { Success, Failure }\n",
        "src/main.rs": "mod exit_codes;\nuse crate::exit_codes::ExitCode;\nfn main() { let _ = ExitCode::Success; }\n",
        "src/worker.rs": "use crate::exit_codes::ExitCode;\npub fn run() -> ExitCode { ExitCode::Success }\n",
        "tests/test_exit_codes.rs": "use crate::exit_codes::ExitCode;\n#[test]\nfn checks_exit_code() { let _ = ExitCode::Success; }\n",
    }

    def test_plural_prompt_matches_module_name_and_emits_source_uses_but_not_test_uses(self):
        root = repo(self.FILES)
        out = run(root, "if I change the exit codes, which parts of the code use the exit code module")

        self.assertIn("### src/exit_codes.rs:", out)
        self.assertIn("### src/main.rs:", out)
        self.assertIn("### src/worker.rs:", out)
        self.assertNotIn("tests/test_exit_codes.rs", out)

    def test_common_word_overlap_without_module_impact_intent_does_not_emit_module_references(self):
        root = repo(self.FILES)
        out = run(root, "what did the team say about exit code review comments", kind="lookup")

        self.assertNotIn("source references to named module", out)

    def test_one_filename_word_is_not_enough_to_name_a_module(self):
        root = repo(self.FILES)
        out = run(root, "what does exit do", kind="lookup")

        self.assertNotIn("### src/exit_codes.rs:", out)

    def test_closed_gate_prompt_runs_the_probe_and_returns_source_reference_evidence(self):
        root = repo(self.FILES)
        framework = probe_test_module()
        probe = framework.Probe(PROBE.parent)
        prompt = "if I change the exit codes, which parts of the code use the exit code module"
        self.assertEqual([], probe.problems)
        self.assertTrue(probe.closed_gate)
        self.assertTrue(probe.applies("impact", prompt))
        self.assertTrue(probe.applies("lookup", "an unrelated prompt used to be eligible by kind"))

        with tempfile.TemporaryDirectory() as home:
            request = {"prompt": prompt, "repo_root": str(root), "brief": "", "kind": "impact",
                       "session_files": []}
            output, _elapsed_ms, error = framework.run_probe(probe, request, Path("/usr/bin/true"), Path(home))

        self.assertIsNone(error)
        self.assertIn("### src/exit_codes.rs:", output)
        self.assertIn("### src/worker.rs:", output)


if __name__ == "__main__":
    unittest.main()
