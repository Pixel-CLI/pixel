#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Exercise the actual caller-probe prompt admission regex from its manifest."""
import re
import tomllib
import unittest
from pathlib import Path


MANIFEST = Path(__file__).resolve().parents[1] / "pixel-plugin.toml"


class CallerManifestAdmission(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        manifest = tomllib.loads(MANIFEST.read_text())
        cls.pattern = re.compile(manifest["brief-probe"]["when_prompt"])

    def test_admits_public_and_generic_caller_phrasings(self):
        prompts = (
            "if I change how req.get reads headers, which parts of lib call it",
            "Which library methods invoke req.get or its header-reading alias?",
            "Which parts of the request module call req.get?",
            "Which modules call randomId?",
            "Which code calls the request parser?",
            "Show callers of the function that creates fresh drawing element IDs.",
            "Locate RequestContext.push and the internal entry points that invoke it.",
        )
        for prompt in prompts:
            with self.subTest(prompt=prompt):
                self.assertIsNotNone(self.pattern.search(prompt))

    def test_rejects_non_caller_scope_questions(self):
        prompts = (
            "Which parts of my vacation did I change?",
            "Which parts of my vacation should call the driver?",
            "Which parts of lib explain request parsing?",
            "Which parts of the repository are impacted?",
        )
        for prompt in prompts:
            with self.subTest(prompt=prompt):
                self.assertIsNone(self.pattern.search(prompt))


if __name__ == "__main__":
    unittest.main()
