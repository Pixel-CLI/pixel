#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Keep real PR security scans while moving Rust feedback off the merge path."""

import json
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parent.parent
WORKFLOW = (ROOT / ".github/workflows/codeql.yml").read_text()


def step(name):
    return WORKFLOW.split(f"      - name: {name}\n", 1)[1].split("      - name:", 1)[0]


class CodeQLPolicy(unittest.TestCase):
    def test_pr_scans_real_fast_languages_and_other_events_keep_rust(self):
        # Read the actual event expression, so changing its condition, either
        # branch or a language fails the contract, rather than testing a copy.
        matrix = re.search(
            r"language: \$\{\{ fromJSON\(github.event_name == 'pull_request' "
            r"&& '([^']+)' \|\| '([^']+)'\) \}\}", WORKFLOW)
        self.assertIsNotNone(matrix)
        self.assertCountEqual(json.loads(matrix[1]), ["actions", "python", "javascript-typescript"])
        self.assertCountEqual(json.loads(matrix[2]), ["rust", "actions", "python", "javascript-typescript"])

    def test_security_feedback_runs_after_every_merge_and_every_night(self):
        self.assertIn("  push:\n    branches: [main]\n", WORKFLOW)
        self.assertIn('    - cron: "41 5 * * *"', WORKFLOW)
        self.assertIn("  workflow_dispatch:\n", WORKFLOW)
        self.assertIn("cancel-in-progress: ${{ github.event_name == 'pull_request' }}", WORKFLOW)
        self.assertIn("github.event_name == 'push' && github.sha || github.ref", WORKFLOW)

    def test_each_pr_still_initializes_and_uploads_actual_results(self):
        self.assertIn("  pull_request:\n    branches: [main]\n", WORKFLOW)
        for name, action in (("Initialize CodeQL", "init"), ("Analyze and upload results", "analyze")):
            body = step(name)
            self.assertNotIn("if:", body)
            self.assertIn(f"uses: github/codeql-action/{action}@", body)
        self.assertIn("category: /language:${{ matrix.language }}", WORKFLOW)
        self.assertNotIn("paths-ignore:", WORKFLOW)
        self.assertNotIn("paths:", WORKFLOW)
        self.assertNotIn("queries:", WORKFLOW)
        self.assertIn("build-mode: none", WORKFLOW)

    def test_branch_dispatch_cannot_publish_executable_cache_to_main(self):
        save = step("Save Rust extraction cache from main")
        self.assertIn("github.ref == 'refs/heads/main'", save)
        self.assertIn("github.event_name != 'pull_request'", save)
        self.assertNotIn("always()", save)
        self.assertIn("CODEQL_EXTRACTOR_RUST_OPTION_CARGO_TARGET_DIR=$RUNNER_TEMP/codeql-rust-target", WORKFLOW)
        self.assertIn("${{ runner.temp }}/codeql-rust-target", step("Restore Rust extraction cache"))
        self.assertNotIn("ref:", WORKFLOW)  # Dispatch scans its selected branch.

    def test_policy_contract_is_run_by_ci_and_optional_local_gates(self):
        for path in (".github/workflows/ci.yml", "scripts/gates.sh"):
            self.assertIn("python3 scripts/test-codeql-policy.py", (ROOT / path).read_text())


if __name__ == "__main__":
    unittest.main()
