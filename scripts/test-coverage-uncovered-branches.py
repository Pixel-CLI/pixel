#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Contract of coverage-uncovered-branches.py.

The script is how a test unit for task #757 picks its targets, so a miscount
sends the work to the wrong file. Both lcov spellings of an uncovered branch
(`-`: the line never ran; `0`: it ran, the branch was never taken) must
count, a taken branch must not, a CI runner's absolute path must match a
repository prefix, and records outside the prefix must not leak into the
totals.
"""
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("coverage-uncovered-branches.py")

LCOV = """\
TN:
SF:/home/runner/work/pixel/pixel/crates/pixel-daemon/src/api.rs
DA:10,1
BRDA:10,0,0,1
BRDA:10,0,1,0
BRDA:20,0,0,-
BRDA:20,0,1,-
BRDA:30,0,0,5
BRDA:30,0,1,2
end_of_record
SF:/home/runner/work/pixel/pixel/crates/pixel-graph/src/extract.rs
BRDA:5,0,0,0
BRDA:5,0,1,1
end_of_record
SF:/home/runner/work/pixel/pixel/crates/pixel-index/src/plan.rs
DA:1,1
end_of_record
"""


class UncoveredBranches(unittest.TestCase):
    def run_script(self, *flags):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        report = Path(scratch.name) / "coverage-branch.lcov"
        report.write_text(LCOV)
        proc = subprocess.run(
            [sys.executable, str(SCRIPT), str(report), *flags],
            capture_output=True,
            text=True,
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        return proc.stdout.splitlines()

    def test_both_uncovered_spellings_count_and_taken_branches_do_not(self):
        out = self.run_script()
        # api.rs: line 10 has one 0, line 20 two "-", line 30 none -> 3 of 6.
        self.assertEqual(out[0].split(), ["3", "/", "6", "50.00%", "crates/pixel-daemon/src/api.rs"])
        self.assertEqual(out[1].split(), ["1", "/", "2", "50.00%", "crates/pixel-graph/src/extract.rs"])

    def test_files_without_branches_are_not_listed(self):
        out = self.run_script()
        self.assertFalse(any("plan.rs" in line for line in out))

    def test_prefix_keeps_only_matching_files_and_their_totals(self):
        out = self.run_script("--prefix", "crates/pixel-graph/")
        self.assertEqual(len(out), 2)
        self.assertIn("crates/pixel-graph/src/extract.rs", out[0])
        self.assertEqual(out[1].split()[:3], ["1", "/", "2"])

    def test_lines_names_each_line_holding_an_uncovered_branch(self):
        out = self.run_script("--prefix", "crates/pixel-daemon/", "--lines")
        detail = [line.split() for line in out if ":" in line.split()[0]]
        self.assertEqual(
            detail,
            [
                ["crates/pixel-daemon/src/api.rs:10", "1", "uncovered"],
                ["crates/pixel-daemon/src/api.rs:20", "2", "uncovered"],
            ],
        )

    def test_top_orders_by_uncovered_branches(self):
        out = self.run_script("--top", "1")
        self.assertIn("crates/pixel-daemon/src/api.rs", out[0])
        self.assertEqual(out[1].split()[:3], ["3", "/", "6"])


if __name__ == "__main__":
    unittest.main()
