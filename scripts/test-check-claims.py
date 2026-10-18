#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Contract of scripts/ci/check-claims.py, which binds each public number to
the pixel version that produced it (docs/bench/claims.toml).

The point is that a number cannot silently age: a claim more than one minor
release behind the binary fails unless an issue tracks its re-measure, and a
claim whose sentence or stated version left its doc fails too, so the
registry and the docs move together. Each case builds a scratch root with
its own Cargo.toml version, doc and registry.
"""
from __future__ import annotations

import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "scripts" / "ci" / "check-claims.py"
DOC = "Reading `models.py`: **641 tokens**, measured with pixel 0.6.0.\n"


def claim(**over: str | bool) -> str:
    fields = {
        "id": "models-outline",
        "doc": "README.md",
        "anchor": "**641 tokens**",
        "version_text": "pixel 0.6.0",
        "protocol": "docs/bench/read-savings.md",
        "binary_version": "0.6.0",
        "commit": "abc1234",
        "measured": "2026-09-01",
        "command": "pixel list-signatures models.py",
        "live": False,
    }
    fields.update(over)
    body = "\n".join(
        f"{k} = {str(v).lower() if isinstance(v, bool) else repr(v)}"
        for k, v in fields.items()
        if v is not None
    )
    return f"[[claim]]\n{body}\n"


class CheckClaims(unittest.TestCase):
    def run_check(self, version: str, registry: str, doc: str = DOC) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "crates/pixel").mkdir(parents=True)
            (root / "crates/pixel/Cargo.toml").write_text(
                textwrap.dedent(f'[package]\nname = "pixel-cli"\nversion = "{version}"\n')
            )
            (root / "docs/bench").mkdir(parents=True)
            (root / "docs/bench/claims.toml").write_text(registry.replace("'", '"'))
            (root / "README.md").write_text(doc)
            return subprocess.run(
                ["python3", str(SCRIPT), "--root", str(root)], capture_output=True, text=True
            )

    def test_a_claim_one_minor_behind_should_pass_quietly(self) -> None:
        out = self.run_check("0.7.3", claim())
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertIn("1 claims: 0 error(s), 0 tracked stale", out.stdout)

    def test_a_claim_two_minors_behind_should_fail_naming_the_doc_and_the_command(self) -> None:
        out = self.run_check("0.8.0", claim())
        self.assertEqual(out.returncode, 1)
        self.assertIn(
            "models-outline: README.md quotes pixel 0.6.0, 2 minor releases behind 0.8; "
            "re-measure it (pixel list-signatures models.py) or name a tracking issue",
            out.stderr,
        )

    def test_a_tracked_stale_claim_should_pass_as_a_warning(self) -> None:
        out = self.run_check("0.8.0", claim(tracking="#1093"))
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertIn("warning: models-outline:", out.stdout)
        self.assertIn("re-measure tracked in #1093", out.stdout)
        self.assertIn("1 claims: 0 error(s), 1 tracked stale", out.stdout)

    def test_an_unknown_version_should_count_as_stale(self) -> None:
        out = self.run_check("0.8.0", claim(binary_version="unknown"))
        self.assertEqual(out.returncode, 1)
        self.assertIn("quotes pixel unknown, an unrecorded version behind 0.8", out.stderr)

    def test_a_previous_major_should_count_as_stale(self) -> None:
        out = self.run_check("1.0.0", claim(binary_version="0.9.0", version_text="pixel 0.6.0"))
        self.assertEqual(out.returncode, 1)
        self.assertIn("99 minor releases behind 1.0", out.stderr)

    def test_a_patch_range_should_read_as_its_minor(self) -> None:
        out = self.run_check("0.7.0", claim(binary_version="0.6.x"))
        self.assertEqual(out.returncode, 0, out.stderr)

    def test_a_doc_that_lost_the_number_or_its_version_should_fail(self) -> None:
        out = self.run_check("0.6.0", claim(), doc="Reading `models.py`: **700 tokens**, with pixel 0.6.0.\n")
        self.assertEqual(out.returncode, 1)
        self.assertIn("no longer contains its anchor '**641 tokens**'", out.stderr)
        out = self.run_check("0.6.0", claim(), doc="Reading `models.py`: **641 tokens**, pixel 0.8.0.\n")
        self.assertEqual(out.returncode, 1)
        self.assertIn("no longer contains its version_text 'pixel 0.6.0'", out.stderr)

    def test_malformed_and_duplicate_entries_should_fail(self) -> None:
        out = self.run_check("0.6.0", claim(command=None))
        self.assertEqual(out.returncode, 1)
        self.assertIn("models-outline: missing command", out.stderr)
        out = self.run_check("0.6.0", claim() + claim())
        self.assertIn("models-outline: duplicate id", out.stderr)
        out = self.run_check("0.6.0", claim(binary_version="six"))
        self.assertIn("binary_version 'six' is not x.y.z, x.y.x or unknown", out.stderr)
        out = self.run_check("0.6.0", "")
        self.assertIn("lists no [[claim]]", out.stderr)

    def test_the_repository_registry_should_pass(self) -> None:
        out = subprocess.run(["python3", str(SCRIPT)], capture_output=True, text=True)
        self.assertEqual(out.returncode, 0, out.stderr)


if __name__ == "__main__":
    unittest.main()
