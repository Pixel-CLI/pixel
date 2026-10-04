#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Contract of check-advisory-ignores.py.

An advisory accepted by cargo-deny but absent from osv-scanner.toml stays a
Scorecard finding; one only osv-scanner.toml hides was never reviewed by the
cargo-deny policy. Both must fail the deny job, as must an entry in either
file with no reason (deny.toml's bare-string form included) or an id listed
twice, while this repository's two files pass.
"""
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("check-advisory-ignores.py")

DENY = """[advisories]
version = 2
ignore = [
    { id = "RUSTSEC-2024-0436", reason = "proc-macro" },
    { id = "RUSTSEC-2025-0119", reason = "formatting helper" },
]
"""

BOTH = """[[IgnoredVulns]]
id = "RUSTSEC-2024-0436"
reason = "proc-macro"

[[IgnoredVulns]]
id = "RUSTSEC-2025-0119"
reason = "formatting helper"
"""


class AdvisoryIgnores(unittest.TestCase):
    def check(self, deny, osv):
        with tempfile.TemporaryDirectory() as scratch:
            deny_path, osv_path = Path(scratch, "deny.toml"), Path(scratch, "osv-scanner.toml")
            deny_path.write_text(deny)
            osv_path.write_text(osv)
            return subprocess.run([sys.executable, str(SCRIPT), str(deny_path), str(osv_path)],
                                  capture_output=True, text=True, timeout=30)

    def test_this_repository_lists_the_same_advisories_in_both_files(self):
        result = subprocess.run([sys.executable, str(SCRIPT)], capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_identical_lists_with_reasons_pass(self):
        result = self.check(DENY, BOTH)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("2 advisory ignore(s) agree", result.stdout)

    def test_an_acceptance_missing_from_osv_scanner_fails_naming_it(self):
        result = self.check(DENY, BOTH.split("\n\n")[0] + "\n")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stderr.strip(),
                         "RUSTSEC-2025-0119: ignored in deny.toml, missing from osv-scanner.toml")

    def test_an_ignore_only_osv_scanner_holds_fails_naming_it(self):
        extra = BOTH + '\n[[IgnoredVulns]]\nid = "RUSTSEC-2099-0001"\nreason = "unreviewed"\n'
        result = self.check(DENY, extra)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stderr.strip(),
                         "RUSTSEC-2099-0001: ignored in osv-scanner.toml, not accepted in deny.toml")

    def test_an_osv_scanner_entry_without_a_reason_fails(self):
        result = self.check(DENY, BOTH.replace('reason = "proc-macro"', 'reason = "  "'))
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stderr.strip(), "RUSTSEC-2024-0436: osv-scanner.toml entry has no reason")


    def test_a_bare_string_deny_entry_fails_for_want_of_a_reason(self):
        bare = DENY.replace('{ id = "RUSTSEC-2025-0119", reason = "formatting helper" }', '"RUSTSEC-2025-0119"')
        result = self.check(bare, BOTH)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stderr.strip(), "RUSTSEC-2025-0119: deny.toml entry has no reason")

    def test_a_duplicate_id_cannot_hide_a_sibling_without_a_reason(self):
        duplicate = '[[IgnoredVulns]]\nid = "RUSTSEC-2024-0436"\n\n' + BOTH
        result = self.check(DENY, duplicate)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stderr.strip().splitlines(), [
            "RUSTSEC-2024-0436: osv-scanner.toml entry has no reason",
            "RUSTSEC-2024-0436: listed twice in osv-scanner.toml",
        ])


if __name__ == "__main__":
    unittest.main()
