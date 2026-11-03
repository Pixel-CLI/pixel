#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""The hunks stop at the budget: a wide commit names every file once, in the stat, not once more per stub."""

import json
import os
import re
import subprocess
import tempfile
import unittest
from pathlib import Path

PROBE = Path(__file__).resolve().parents[1] / "probe"
GIT_ENV = {"GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t", "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@t",
           "GIT_CONFIG_GLOBAL": os.devnull, "PATH": os.environ["PATH"]}
TITLE = "rework every widget module"


def git(root, *argv):
    subprocess.run(["git", *argv], cwd=root, env=GIT_ENV, check=True, capture_output=True)


def repo(files):
    """A base commit, then one commit titled TITLE that rewrites `files` source files of 40 lines each."""
    root = Path(tempfile.mkdtemp())
    (root / "README.md").write_text("widgets\n")
    git(root, "init", "-q")
    git(root, "add", ".")
    git(root, "commit", "-q", "-m", "base")
    for n in range(files):
        path = root / "src" / f"widget_{n:03}.rs"
        path.parent.mkdir(exist_ok=True)
        path.write_text("".join(f"pub fn widget_{n}_part_{i}() -> u32 {{ {i} }}\n" for i in range(40)))
    git(root, "add", ".")
    git(root, "commit", "-q", "-m", TITLE)
    return root


def run(root):
    request = {"prompt": f'what did the commit "{TITLE}" change?', "repo_root": str(root), "brief": "",
               "kind": "history", "session_files": []}
    return subprocess.run([str(PROBE)], input=json.dumps(request), capture_output=True, text=True,
                          check=True).stdout


class TestPatchBudget(unittest.TestCase):
    def test_a_wide_commit_stops_its_hunks_at_the_budget_and_counts_the_rest(self):
        out = run(repo(120))
        stat, _, hunks = out.partition("\n@@ ")

        self.assertEqual(120, len(re.findall(r"src/widget_\d{3}\.rs \|", stat)))
        self.assertLessEqual(len(out.encode()), len(stat.encode()) + 4_800 + 200)
        self.assertLessEqual(out.count("(hunks cut)"), 1)
        shown = len(re.findall(r"^@@ src/", "@@ " + hunks, re.M))
        self.assertIn(f"... hunks of {120 - shown} more files cut (named in the list above)", out)

    def test_a_small_commit_shows_every_hunk_and_no_cut_line(self):
        out = run(repo(2))

        self.assertIn("@@ src/widget_000.rs", out)
        self.assertIn("@@ src/widget_001.rs", out)
        self.assertNotIn("more files cut", out)
        self.assertNotIn("(hunks cut)", out)


if __name__ == "__main__":
    unittest.main()
