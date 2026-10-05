#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Coverage never blocks publication or skips an unmeasured main revision."""

import importlib.util
import json
import subprocess
import tempfile
from unittest.mock import patch
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("coverage_nightly", ROOT / "scripts/coverage-nightly.py")
coverage = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(coverage)


class NightlyCoverage(unittest.TestCase):
    def setUp(self):
        self.head = "a" * 40
        self.run = dict(head_sha=self.head, head_branch="main", event="schedule",
                        status="completed", conclusion="success",
                        path=".github/workflows/coverage.yml")

    def test_first_run_measures_main(self):
        self.assertTrue(coverage.needs_coverage(self.head, []))

    def test_successful_measurement_avoids_rebuilding_unchanged_main(self):
        self.assertFalse(coverage.needs_coverage(self.head, [self.run]))

    def test_new_commits_need_a_new_measurement(self):
        self.assertTrue(coverage.needs_coverage("b" * 40, [self.run]))

    def test_failed_or_incomplete_measurements_are_retried(self):
        for conclusion in ("failure", "cancelled", "timed_out", None):
            with self.subTest(conclusion=conclusion):
                self.assertTrue(coverage.needs_coverage(self.head, [dict(self.run, conclusion=conclusion)]))
        self.assertTrue(coverage.needs_coverage(self.head, [dict(self.run, status="in_progress")]))

    def test_foreign_runs_do_not_satisfy_the_measurement(self):
        for field, value in (("event", "pull_request"), ("event", "workflow_dispatch"),
                             ("head_branch", "feature"), ("path", ".github/workflows/ci.yml")):
            with self.subTest(field=field, value=value):
                self.assertTrue(coverage.needs_coverage(self.head, [dict(self.run, **{field: value})]))

    def test_invalid_sha_is_not_silently_skipped(self):
        with self.assertRaises(ValueError):
            coverage.needs_coverage("main", [])

    def test_api_failure_stops_selection_without_claiming_main_is_measured(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "outputs"
            argv = ["coverage-nightly.py", "--repository", "org/repo", "--head",
                    self.head, "--github-output", str(output)]
            with patch("sys.argv", argv), patch.object(coverage.subprocess, "check_output",
                    side_effect=subprocess.CalledProcessError(1, "gh")):
                with self.assertRaises(subprocess.CalledProcessError):
                    coverage.main()
            self.assertFalse(output.exists())

    def test_cli_emits_the_skip_decision_for_the_workflow(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "outputs"
            argv = ["coverage-nightly.py", "--repository", "org/repo", "--head",
                    self.head, "--github-output", str(output)]
            with patch("sys.argv", argv), patch.object(coverage.subprocess, "check_output",
                    return_value=json.dumps({"workflow_runs": [self.run]})) as api:
                coverage.main()
            self.assertEqual(output.read_text(), "run=false\n")
            endpoint = api.call_args.args[0][-1]
            self.assertIn("/actions/workflows/coverage.yml/runs?", endpoint)
            self.assertIn("branch=main&event=schedule&status=success", endpoint)

    def test_both_expensive_jobs_require_a_new_nightly_measurement(self):
        workflow = (ROOT / ".github/workflows/coverage.yml").read_text()
        triggers = workflow.split("on:\n", 1)[1].split("\nconcurrency:", 1)[0]
        self.assertIn('cron: "47 2 * * *"', triggers)
        for event in ("push:", "pull_request:", "workflow_dispatch:"):
            self.assertNotIn(event, triggers)
        for job in ("coverage", "branches"):
            self.assertIn(f"  {job}:\n    needs: changes\n    if: needs.changes.outputs.run == 'true'", workflow)
        self.assertIn('test "$GITHUB_REF" = refs/heads/main', workflow)
        self.assertIn("--fail-under-lines 90", workflow)
        self.assertNotIn("continue-on-error:", workflow)


if __name__ == "__main__":
    unittest.main()
