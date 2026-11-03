#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Both shipped history readers must choose the same source-backed commit."""
import json
from pathlib import Path
import subprocess
import tempfile
import unittest


PROBES = Path(__file__).resolve().parents[2]


class SubjectHistoryTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.git("init", "-q")
        self.git("config", "user.name", "Probe Test")
        self.git("config", "user.email", "probe@example.invalid")

    def git(self, *args):
        return subprocess.run(["git", "-C", str(self.root), *args], check=True,
                              capture_output=True, text=True).stdout.strip()

    def commit(self, subject, body=""):
        source = self.root / "source.py"
        source.write_text(source.read_text() + "feature = True\n" if source.exists() else "feature = True\n")
        self.git("add", "source.py")
        self.git("commit", "-q", "-m", subject, "-m", body)
        return self.git("rev-parse", "HEAD")

    def assert_commit(self, prompt, expected):
        request = json.dumps({"repo_root": str(self.root), "prompt": prompt, "brief": ""})
        for name in ("history-fallback", "cand-k-commit-patch"):
            with self.subTest(probe=name):
                result = subprocess.run(["python3", str(PROBES / name / "probe")], input=request,
                                        text=True, capture_output=True, timeout=3, check=True)
                commit = next(line.split()[2] for line in result.stdout.splitlines()
                              if line.startswith("### commit "))
                self.assertTrue(expected.startswith(commit), result.stdout)
                self.assertIn("+feature = True", result.stdout)
                repeat = subprocess.run(["python3", str(PROBES / name / "probe")], input=request,
                                        text=True, capture_output=True, timeout=3, check=True)
                self.assertEqual(result.stdout, repeat.stdout)

    def test_subject_evidence_beats_incidental_body_and_keeps_short_words(self):
        expected = self.commit("feat: compact data pack")
        self.commit("docs: refresh output", "introduced compact data in a later manual")
        self.assert_commit("which commit introduced the compact data pack and what did it add", expected)

    def test_two_word_subject_phrase_is_enough(self):
        expected = self.commit("feat: delta compression")
        self.commit("docs: refresh manual", "introduced delta behavior and compression examples")
        self.assert_commit("which commit introduced delta compression", expected)

    def test_exact_quoted_title_keeps_priority(self):
        expected = self.commit("fix the archive reader")
        self.commit("feat: compact data pack")
        self.assert_commit('what did "fix the archive reader" change for the compact data pack', expected)

    def test_exact_release_tag_keeps_priority(self):
        expected = self.commit("release 7.2.0")
        self.git("tag", "v7.2.0")
        self.commit("feat: compact data pack")
        self.assert_commit("what changed in v7.2.0 for the compact data pack", expected)


if __name__ == "__main__":
    unittest.main()
