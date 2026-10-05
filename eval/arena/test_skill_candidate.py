#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Tests for staging an explicitly distributed Codex skill in a pilot arm."""
import json
import tempfile
import unittest
from pathlib import Path

from skill_candidate import stage


class SkillCandidateTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.source = self.root / "candidate"
        (self.source / "agents").mkdir(parents=True)
        (self.source / "SKILL.md").write_text("---\nname: pixel-impact\n---\nUse only when explicitly asked.\n")
        (self.source / "agents" / "openai.yaml").write_text(
            "interface:\n  display_name: Pixel impact\npolicy:\n  allow_implicit_invocation: false\n"
        )
        self.repo = self.root / "repo"
        self.repo.mkdir()

    def tearDown(self):
        self.temp.cleanup()

    def test_source_policy_stays_explicit_and_candidate_copy_is_enabled(self):
        raw_receipt = self.root / "raw.json"
        candidate_receipt = self.root / "candidate.json"
        raw = stage(self.source, self.repo, "raw", raw_receipt)
        candidate = stage(self.source, self.repo, "pixel", candidate_receipt)

        staged_policy = (self.repo / ".agents/skills/pixel-impact/agents/openai.yaml").read_text()
        self.assertFalse(raw["staged"])
        self.assertTrue(candidate["staged"])
        self.assertEqual(raw["source_sha256"], candidate["source_sha256"])
        self.assertIn("allow_implicit_invocation: false", (self.source / "agents/openai.yaml").read_text())
        self.assertIn("allow_implicit_invocation: true", staged_policy)
        self.assertEqual(json.loads(candidate_receipt.read_text()), candidate)
        self.assertNotEqual(candidate["source_sha256"], candidate["staged_sha256"])

    def test_candidate_must_be_explicit_only_in_distribution(self):
        policy = self.source / "agents/openai.yaml"
        policy.write_text(policy.read_text().replace("false", "true"))

        with self.assertRaisesRegex(ValueError, "explicitly set allow_implicit_invocation: false"):
            stage(self.source, self.repo, "pixel", self.root / "receipt.json")


if __name__ == "__main__":
    unittest.main()
