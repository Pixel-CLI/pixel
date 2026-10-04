#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Contract of harness-grid.yml's dispatch input guard.

`pr` names the pull request the shoot script comments on and pushes media
for, so the step refuses anything but a number before the script runs. The
test runs the step's own `run:` block, taken from the workflow file, with the
script replaced by a stub that records its arguments: a number reaches the
script unchanged, every other value fails the step and never reaches it.
Removing or loosening the guard fails this test.
"""
from __future__ import annotations

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WORKFLOW = ROOT / ".github" / "workflows" / "harness-grid.yml"
STEP = "Shoot the four harnesses and publish the grid"


def step_script() -> str:
    """The `run: |` block of the step named STEP, dedented."""
    lines = WORKFLOW.read_text().splitlines()
    start = next(i for i, line in enumerate(lines) if line.strip() == f"- name: {STEP}")
    step_indent = len(lines[start]) - len(lines[start].lstrip())
    run_at = None
    for i in range(start + 1, len(lines)):
        line = lines[i]
        indent = len(line) - len(line.lstrip())
        if line.strip() and indent <= step_indent:
            break
        if line.strip() == "run: |":
            run_at = i
            break
    if run_at is None:
        raise AssertionError(f"no `run: |` block in step {STEP!r}")
    key_indent = len(lines[run_at]) - len(lines[run_at].lstrip())
    body = []
    for line in lines[run_at + 1:]:
        if line.strip() and len(line) - len(line.lstrip()) <= key_indent:
            break
        body.append(line)
    block_indent = min(len(l) - len(l.lstrip()) for l in body if l.strip())
    return "\n".join(l[block_indent:] for l in body) + "\n"


class HarnessGridPrInput(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        scripts = self.root / "scripts"
        scripts.mkdir()
        self.calls = self.root / "calls.txt"
        stub = scripts / "harness-sandbox-shoot.sh"
        stub.write_text('#!/bin/sh\nprintf "%s|%s\\n" "$1" "$2" >> "$CALLS"\n')
        stub.chmod(0o755)
        self.script = step_script()

    def tearDown(self):
        self.tmp.cleanup()

    def run_step(self, pr: str, prompt: str = "Create the story feature") -> subprocess.CompletedProcess:
        env = dict(os.environ, PR=pr, PROMPT=prompt, CALLS=str(self.calls))
        # GitHub runs `run:` blocks with `bash -e`.
        return subprocess.run(["bash", "-e", "-c", self.script], cwd=self.root, env=env,
                              capture_output=True, text=True)

    def calls_made(self) -> list[str]:
        return self.calls.read_text().splitlines() if self.calls.exists() else []

    def test_a_number_reaches_the_script_with_the_prompt(self):
        out = self.run_step("641")
        self.assertEqual(out.returncode, 0, out.stdout + out.stderr)
        self.assertEqual(self.calls_made(), ["641|Create the story feature"])

    def test_anything_else_fails_before_the_script(self):
        for bad in ["", "12a", "1;id", "$(id)", " 4", "4 ", "-1", "+3"]:
            with self.subTest(pr=bad):
                out = self.run_step(bad)
                self.assertNotEqual(out.returncode, 0)
                self.assertIn("pr must be a pull request number", out.stdout + out.stderr)
                self.assertEqual(self.calls_made(), [])


if __name__ == "__main__":
    unittest.main()
