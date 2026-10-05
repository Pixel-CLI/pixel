#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Nightly diffs must neither lose unjudged commits nor rerun unchanged main."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).with_name("mutants-nightly-range.py")
SPEC = importlib.util.spec_from_file_location("nightly_range", SCRIPT)
nightly = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(nightly)


class RangeContract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.old = Path.cwd()
        os.chdir(self.root)
        self.addCleanup(os.chdir, self.old)
        self.git("init", "-q")
        self.git("config", "user.email", "test@example.invalid")
        self.git("config", "user.name", "Test")
        self.before = self.commit("initial")
        (self.root / "scripts").mkdir()
        (self.root / "scripts/mutants-nightly-range.py").write_text("rollout")
        self.rollout = self.commit("rollout")
        self.head = self.commit("next")

    def git(self, *args):
        return subprocess.check_output(["git", *args], text=True).strip()

    def commit(self, message):
        (self.root / "source").write_text(message)
        self.git("add", ".")
        self.git("-c", "core.hooksPath=/dev/null", "commit", "-qm", message)
        return self.git("rev-parse", "HEAD")

    def run_plan(self, artifacts, run=None):
        def api(endpoint, **kwargs):
            if "/artifacts?" in endpoint:
                return [{"artifacts": artifacts}]
            return run or {
                "path": ".github/workflows/mutants.yml", "head_branch": "main",
                "head_sha": artifacts[0]["workflow_run"]["head_sha"],
                "event": "schedule", "status": "completed", "conclusion": "failure",
            }
        with patch.object(nightly, "gh_json", side_effect=api):
            return nightly.select_range("owner/repo", self.head)

    def artifact(self, sha, **updates):
        return {"id": 42, "name": nightly.CHECKPOINT, "workflow_run": {
            "id": 100, "head_branch": "main", "head_sha": sha}, **updates}

    def test_first_night_includes_rollout_and_all_later_commits(self):
        self.assertEqual(self.run_plan([]), f"{self.before}...{self.head}")

    def test_unchanged_main_does_not_start_a_campaign(self):
        self.assertEqual(self.run_plan([self.artifact(self.head)]), "")

    def test_a_ref_suffixed_workflow_path_does_not_replay_judged_commits(self):
        run = {"path": f"{nightly.WORKFLOW}@main", "head_branch": "main",
               "head_sha": self.head, "event": "schedule", "status": "completed",
               "conclusion": "success"}
        self.assertEqual(self.run_plan([self.artifact(self.head)], run), "")
        run["path"] = ".github/workflows/other.yml@main"
        self.assertEqual(self.run_plan([self.artifact(self.head)], run),
                         f"{self.before}...{self.head}")

    def test_a_completed_red_campaign_advances_without_hiding_its_survivors(self):
        self.assertEqual(self.run_plan([self.artifact(self.rollout)]),
                         f"{self.rollout}...{self.head}")

    def test_an_unfinished_campaign_is_not_a_checkpoint(self):
        run = {"path": ".github/workflows/mutants.yml", "head_branch": "main",
               "head_sha": self.rollout, "event": "schedule", "status": "in_progress"}
        self.assertEqual(self.run_plan([self.artifact(self.rollout)], run),
                         f"{self.before}...{self.head}")

    def test_another_workflow_cannot_supply_the_checkpoint(self):
        run = {"path": ".github/workflows/other.yml", "head_branch": "main",
               "head_sha": self.head, "event": "schedule", "status": "completed"}
        self.assertEqual(self.run_plan([self.artifact(self.head)], run),
                         f"{self.before}...{self.head}")

    def test_a_foreign_branch_cannot_skip_main(self):
        artifact = self.artifact(self.head)
        artifact["workflow_run"]["head_branch"] = "feature"
        self.assertEqual(self.run_plan([artifact]), f"{self.before}...{self.head}")

    def test_api_failure_is_not_treated_as_no_previous_run(self):
        with patch.object(nightly, "gh_json", side_effect=RuntimeError("unavailable")):
            with self.assertRaisesRegex(RuntimeError, "unavailable"):
                nightly.select_range("owner/repo", self.head)

    def test_cancelled_runs_do_not_consume_unjudged_commits(self):
        run = {"path": nightly.WORKFLOW, "head_branch": "main",
               "head_sha": self.head, "event": "schedule", "status": "completed",
               "conclusion": "cancelled"}
        self.assertEqual(self.run_plan([self.artifact(self.head)], run),
                         f"{self.before}...{self.head}")

    def test_deleted_checkpoint_history_fails_instead_of_guessing_a_base(self):
        self.git("checkout", "-qb", "unrelated", self.before)
        foreign = self.commit("foreign")
        self.git("checkout", "-q", self.head)
        with self.assertRaisesRegex(ValueError, "not in main history"):
            self.run_plan([self.artifact(foreign)])

    def test_plan_cli_returns_the_exact_range_that_jobs_will_consume(self):
        output = self.root / "outputs"
        with patch.object(nightly, "gh_json", return_value=[{"artifacts": []}]):
            code = nightly.main(["plan", "--repository", "owner/repo", "--head", self.head,
                                 "--github-output", str(output)])
        self.assertEqual(code, 0)
        self.assertEqual(output.read_text(), f"diff_range={self.before}...{self.head}\nrun=true\n")


class CheckpointContract(unittest.TestCase):
    def test_all_judged_including_survivors_and_timeouts_advance(self):
        self.assertTrue(nightly.fully_judged(4, {
            "caught": 1, "unviable": 1, "missed": 1, "timeout": 1}))

    def test_missing_duplicate_disk_full_and_unknown_outcomes_do_not_advance(self):
        for counts in ({"caught": 1}, {"caught": 3}, {"disk-full": 2}, {"future": 2}):
            with self.subTest(counts=counts):
                self.assertFalse(nightly.fully_judged(2, counts))

    def test_docs_only_diff_can_advance_without_building_any_mutant(self):
        self.assertTrue(nightly.fully_judged(0, {}))

    def test_real_outcome_files_control_whether_a_checkpoint_is_written(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            listing = root / "list.txt"
            listing.write_text("src/lib.rs:1:1: mutant\n")
            output = root / "checkpoint.json"
            args = ["checkpoint", "--list", str(listing), "--outcomes-root", str(root),
                    "--sha", "a" * 40, "--output", str(output)]
            self.assertEqual(nightly.main(args), 1)
            self.assertFalse(output.exists(), "a missing shard must be retried")
            (root / "outcomes.json").write_text(json.dumps({"outcomes": [{
                "scenario": {"Mutant": {"name": "survivor"}}, "summary": "MissedMutant",
            }]}))
            self.assertEqual(nightly.main(args), 0)
            self.assertEqual(json.loads(output.read_text()), {
                "sha": "a" * 40, "listed": 1, "outcomes": {"missed": 1},
            })


class WorkflowContract(unittest.TestCase):
    def test_only_main_diff_is_scheduled_and_no_pr_starts_mutations(self):
        root = SCRIPT.parent.parent
        workflow = (root / nightly.WORKFLOW).read_text()
        triggers = workflow.split("on:\n", 1)[1].split("\nconcurrency:", 1)[0]
        self.assertIn('cron: "17 1 * * *"', triggers)
        self.assertNotIn("pull_request:", triggers)
        self.assertNotIn("workflow_dispatch:", triggers)
        self.assertFalse((root / ".github/workflows/mutants-nightly.yml").exists())
        self.assertIn("if: needs.range.outputs.run == 'true'", workflow)
        self.assertIn("if: always() && needs.range.outputs.run == 'true'", workflow)
        self.assertIn('test "$GITHUB_REF" = refs/heads/main', workflow)
        self.assertIn("steps.checkpoint.outcome == 'success'", workflow)
        self.assertNotIn("continue-on-error:", workflow)
        self.assertIn("ref: ${{ github.sha }}", workflow)
        self.assertNotIn("ref: ${{ needs.plan.outputs.ref }}", workflow)


if __name__ == "__main__":
    unittest.main()
