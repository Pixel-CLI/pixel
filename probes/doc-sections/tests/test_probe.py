#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Which documentation sections doc-sections quotes, Markdown and reStructuredText (issue #1200)."""

import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

PROBE = Path(__file__).resolve().parents[1] / "probe"
GIT_ENV = {"GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t", "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@t",
           "GIT_CONFIG_GLOBAL": os.devnull, "PATH": os.environ["PATH"]}

RST = """Configuration Handling
======================

Applications need some kind of configuration.

Prose before a transition.

----

Configuring from Environment Variables
--------------------------------------

Point an environment variable at a file and load it with from_envvar.

Unrelated Topic
---------------

Nothing about the question here.
"""


def headers(prompt, files):
    root = Path(tempfile.mkdtemp())
    for rel, text in files.items():
        (root / rel).parent.mkdir(parents=True, exist_ok=True)
        (root / rel).write_text(text)
    for argv in (["init", "-q"], ["add", "."], ["commit", "-q", "-m", "fixture"]):
        subprocess.run(["git", *argv], cwd=root, env=GIT_ENV, check=True, capture_output=True)
    request = {"prompt": prompt, "repo_root": str(root), "brief": "", "kind": "config", "session_files": []}
    out = subprocess.run([str(PROBE)], input=json.dumps(request), capture_output=True, text=True, check=True).stdout
    return [line for line in out.splitlines() if line.startswith("### ")]


class Sections(unittest.TestCase):
    def test_an_underlined_rst_title_is_a_section_and_the_matching_one_is_quoted(self):
        self.assertEqual(headers("how do I load configuration from a file named by an environment variable",
                                 {"docs/config.rst": RST}),
                         ["### docs/config.rst:10-14 — Configuring from Environment Variables (documentation section)"])

    def test_a_transition_rule_after_a_blank_line_is_not_a_title(self):
        found = headers("prose transition", {"docs/config.rst": RST})
        self.assertNotIn("----", " ".join(found))

    def test_markdown_headings_still_make_sections(self):
        self.assertEqual(headers("how do environment variables configure loading",
                                 {"README.md": "# Tool\n\nA tool.\nIt sorts.\nIt prints.\nIt stops.\n\n"
                                               "## Environment variables\n\nLoading reads TOOL_CONFIG.\n"}),
                         ["### README.md:8-10 — Environment variables (documentation section)"])


if __name__ == "__main__":
    unittest.main()
