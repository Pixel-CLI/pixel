#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Contract of scripts/ci/verdict-gate.py, the `Record the green verdict` judge.

The cases that matter are the ones that used to pass: a failed or cancelled
lane, and a lane skipped although the change filter said it applies. The
last case feeds the real ci.yml `needs` list, so a lane added to the job
without a row in the script's table cannot ship unnoticed.
"""

import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import unittest

REPO = Path(__file__).resolve().parent.parent
SCRIPT = REPO / "scripts/ci/verdict-gate.py"
SPEC = importlib.util.spec_from_file_location("verdict_gate", SCRIPT)
gate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(gate)

FLAGS = {"installer": "false", "rust-source": "true", "deps": "false"}


def needs(**results: str) -> dict:
    """A `needs` object: every lane green, flags as FLAGS, then overrides."""
    lanes = {lane: {"result": "success", "outputs": {}} for lane in gate.LANES}
    lanes["changes"]["outputs"] = dict(FLAGS)
    lanes["linux-install"]["result"] = "skipped"
    lanes["deny"]["result"] = "skipped"
    for lane, result in results.items():
        lanes[lane.replace("_", "-")]["result"] = result
    return lanes


class Judge(unittest.TestCase):
    def test_green_with_inapplicable_lanes_skipped_passes(self):
        self.assertEqual(gate.judge(needs()), [])

    def test_failed_lane_fails(self):
        self.assertEqual(gate.judge(needs(lint="failure")), ["lint: failure"])

    def test_cancelled_lane_fails(self):
        self.assertEqual(gate.judge(needs(test="cancelled")), ["test: cancelled"])

    def test_skipped_lane_whose_flag_is_true_fails(self):
        problems = gate.judge(needs(msrv="skipped"))
        self.assertEqual(len(problems), 1)
        self.assertIn("msrv: skipped although", problems[0])

    def test_skipped_lane_without_a_flag_fails(self):
        self.assertEqual(gate.judge(needs(test="skipped")), ["test: skipped"])

    def test_unknown_lane_fails_closed(self):
        lanes = needs()
        lanes["fresh"] = {"result": "skipped", "outputs": {}}
        self.assertIn("fresh: skipped", gate.judge(lanes)[0])

    def test_every_failing_lane_is_reported(self):
        self.assertEqual(len(gate.judge(needs(lint="failure", ranking="cancelled"))), 2)


class Cli(unittest.TestCase):
    def run_gate(self, lanes: dict) -> subprocess.CompletedProcess:
        return subprocess.run(
            [sys.executable, str(SCRIPT)],
            env=dict(os.environ, NEEDS_JSON=json.dumps(lanes)),
            capture_output=True, text=True,
        )

    def test_exit_status_follows_the_judgement(self):
        self.assertEqual(self.run_gate(needs()).returncode, 0)
        red = self.run_gate(needs(lint="failure"))
        self.assertEqual(red.returncode, 1)
        self.assertIn("::error::lint: failure", red.stdout)


class Workflow(unittest.TestCase):
    def test_every_needs_entry_of_the_verdict_job_is_mapped(self):
        text = (REPO / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        job = text.split("\n  verdict:\n", 1)[1]
        listed = re.search(r"needs: \[([^\]]*)\]", job).group(1).split(",")
        self.assertEqual({lane.strip() for lane in listed}, set(gate.LANES))


if __name__ == "__main__":
    unittest.main()
