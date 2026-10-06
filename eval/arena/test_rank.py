#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Focused contract tests for paired arena ranking."""
import json
import hashlib
import tempfile
import unittest
from pathlib import Path

from rank import rank_results
from context_manifest import collect_manifest
from score import load_result


def write_scenario(folder, name, patterns):
    (folder / f"{name}.json").write_text(json.dumps({
        "id": name,
        "must": [{"pattern": pattern, "points": points}
                  for pattern, points in patterns],
        "never": [],
    }))


def write_transcript(folder, arm, task, rep, answer, input_tokens=100,
                     output_tokens=10, complete=True, include_usage=True,
                     commands=(), usage_payload=None):
    completion = {"type": "turn.completed"}
    if include_usage:
        completion["usage"] = usage_payload or {
            "input_tokens": input_tokens,
            "cached_input_tokens": 0,
            "output_tokens": output_tokens,
        }
    events = [
        {"type": "item.completed", "item": {
            "type": "command_execution", "command": command,
            "aggregated_output": "",
        }} for command in commands
    ] + [
        {"type": "item.completed", "item": {"type": "agent_message", "text": answer}},
        completion if complete else {"type": "turn.failed"},
    ]
    path = folder / f"{arm}-{task}-{rep}.jsonl"
    path.write_text("".join(json.dumps(event) + "\n" for event in events))
    (folder / f"{arm}-{task}-{rep}.seconds").write_text("7\n")
    return path


def write_context(folder, arm, rep, developer="baseline", agents="repo-agents"):
    (folder / f"context-{arm}-{rep}.json").write_text(json.dumps({
        "developer_instructions": {"present": True, "sha256": developer},
        "agents_files": {"AGENTS.md": agents},
        "hook_config_files": {},
        "pixel_reference_lines": 0,
        "skills": {},
    }))


class RankResultsTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.results = self.root / "results"
        self.scenarios = self.root / "scenarios"
        self.results.mkdir()
        self.scenarios.mkdir()

    def tearDown(self):
        self.temp.cleanup()

    def test_only_selected_cells_and_macro_averages_normalized_task_scores(self):
        write_scenario(self.scenarios, "small", [("alpha", 10)])
        write_scenario(self.scenarios, "large", [("beta", 10), ("gamma", 10)])
        for arm in ("raw", "pixel", "other"):
            write_transcript(self.results, arm, "small", 1, "alpha")
            write_transcript(self.results, arm, "large", 1,
                             "beta" if arm == "raw" else "beta gamma")
        report = rank_results(self.results, self.scenarios, ["raw", "pixel"],
                              ["small", "large"], 1)

        self.assertEqual(report["arms"], ["raw", "pixel"])
        self.assertEqual([row["task"] for row in report["rows"]],
                         ["small", "small", "large", "large"])
        self.assertEqual(report["summary"]["raw"]["macro_avg_score_pct"], 75.0)
        self.assertEqual(report["task_summaries"]["large"]["arms"]["pixel"]["median_score_pct"], 100)

    def test_failed_and_missing_pairs_are_reported_and_excluded(self):
        write_scenario(self.scenarios, "task", [("answer", 4)])
        write_transcript(self.results, "raw", "task", 1, "answer")
        write_transcript(self.results, "pixel", "task", 1, "partial", complete=False)
        (self.results / "pixel-task-1.failed").touch()
        write_transcript(self.results, "raw", "task", 2, "answer")

        report = rank_results(self.results, self.scenarios, ["raw", "pixel"],
                              ["task"], 2)
        summary = report["task_summaries"]["task"]

        self.assertEqual([pair["status"] for pair in report["pairs"]],
                         ["incomplete", "incomplete"])
        self.assertEqual(report["pairs"][0]["arms"]["pixel"], "failed")
        self.assertEqual(report["pairs"][1]["arms"]["pixel"], "missing")
        self.assertIsNone(summary["arms"]["raw"]["median_score"])
        self.assertIsNone(summary["arms"]["raw"]["median_tokens"])
        self.assertEqual(summary["arms"]["pixel"]["failed"], 1)
        self.assertEqual(summary["arms"]["pixel"]["missing"], 1)

    def test_failed_marker_wins_even_if_transcript_looks_complete(self):
        write_scenario(self.scenarios, "task", [("answer", 1)])
        for arm in ("raw", "pixel"):
            write_transcript(self.results, arm, "task", 1, "answer")
        (self.results / "pixel-task-1.failed").touch()

        report = rank_results(self.results, self.scenarios, ["raw", "pixel"],
                              ["task"], 1)

        self.assertEqual(report["pairs"][0]["status"], "incomplete")
        self.assertEqual(report["pairs"][0]["arms"]["pixel"], "failed")
        self.assertIsNone(report["summary"]["pixel"]["macro_avg_score_pct"])

    def test_token_medians_use_identical_paired_repetitions(self):
        write_scenario(self.scenarios, "task", [("answer", 1)])
        write_transcript(self.results, "raw", "task", 1, "answer")
        write_transcript(self.results, "pixel", "task", 1, "answer", include_usage=False)
        write_transcript(self.results, "raw", "task", 2, "answer", include_usage=False)
        write_transcript(self.results, "pixel", "task", 2, "answer")

        report = rank_results(self.results, self.scenarios, ["raw", "pixel"],
                              ["task"], 2)
        summary = report["task_summaries"]["task"]

        self.assertEqual(summary["metric_pair_counts"]["quality"], 2)
        self.assertEqual(summary["metric_pair_counts"]["tokens"], 0)
        self.assertEqual(summary["metric_pair_counts"]["seconds"], 2)
        self.assertIsNone(summary["arms"]["raw"]["median_tokens"])
        self.assertIsNone(summary["arms"]["pixel"]["median_tokens"])
        self.assertEqual(report["rank_basis"], "quality_only_incomplete_paired_tokens")

    def test_receipt_records_native_program_counts_and_cache_usage(self):
        write_scenario(self.scenarios, "task", [("answer", 1)])
        write_transcript(self.results, "raw", "task", 1, "answer",
                         include_usage=False,
                         commands=["sed -n '1,40p' src.ts && rg -n foo src.ts"])
        write_transcript(self.results, "pixel", "task", 1, "answer")

        report = rank_results(self.results, self.scenarios, ["raw", "pixel"],
                              ["task"], 1, run_metadata={"run_id": "receipt-test"})
        raw = next(row for row in report["rows"] if row["arm"] == "raw")

        self.assertEqual(raw["native_command_count"], 2)
        self.assertEqual(raw["native_command_counts"], {"sed": 1, "rg": 1})
        self.assertEqual(raw["native_search_calls"], 1)
        self.assertEqual(report["run"]["run_id"], "receipt-test")
        self.assertIsNone(raw["cache_read_tokens"])
        self.assertIsNone(raw["tokens"])
        context_audit = report["context_audit"]
        self.assertFalse(context_audit["required"])
        self.assertIn("mixed-purpose line", context_audit["raw_snapshot_policy"])
        self.assertEqual(context_audit["checks"], [])

    def test_reranking_same_selection_preserves_known_provenance(self):
        write_scenario(self.scenarios, "task", [("answer", 1)])
        for arm in ("raw", "pixel"):
            write_transcript(self.results, arm, "task", 1, "answer")
        (self.results / "ranking.json").write_text(json.dumps({
            "arms": ["raw", "pixel"],
            "tasks": ["task"],
            "reps": 1,
            "baseline_arm": "raw",
            "run": {
                "run_id": "known-run",
                "model": "gpt-test",
                "effort": "medium",
                "repo_snapshot": "/repo",
                "pixel_image_id": "sha256:known",
                "pixel_source_id": "local-source-fingerprint",
                "codex_version": "codex-test",
            },
        }))

        report = rank_results(self.results, self.scenarios, ["raw", "pixel"],
                              ["task"], 1, run_metadata={
                                  "run_id": None,
                                  "model": None,
                                  "effort": None,
                                  "repo_snapshot": None,
                                  "pixel_image_id": None,
                                  "pixel_source_id": None,
                                  "codex_version": None,
                              })

        self.assertEqual(report["run"]["run_id"], "known-run")
        self.assertEqual(report["run"]["model"], "gpt-test")
        self.assertEqual(report["run"]["pixel_image_id"], "sha256:known")
        self.assertEqual(report["run"]["pixel_source_id"], "local-source-fingerprint")

    def test_reranking_different_selection_does_not_reuse_provenance(self):
        write_scenario(self.scenarios, "task", [("answer", 1)])
        for arm in ("raw", "pixel"):
            write_transcript(self.results, arm, "task", 1, "answer")
        (self.results / "ranking.json").write_text(json.dumps({
            "arms": ["raw", "pixel"],
            "tasks": ["different-task"],
            "reps": 1,
            "baseline_arm": "raw",
            "run": {"run_id": "wrong-run", "model": "wrong-model"},
        }))

        report = rank_results(self.results, self.scenarios, ["raw", "pixel"],
                              ["task"], 1)

        self.assertIsNone(report["run"].get("run_id"))
        self.assertIsNone(report["run"].get("model"))

    def test_context_parity_requires_identical_static_context_and_abstention(self):
        write_scenario(self.scenarios, "task", [("answer", 1)])
        for arm in ("raw", "pixel"):
            write_transcript(self.results, arm, "task", 1, "answer")
            write_context(self.results, arm, 1)

        report = rank_results(self.results, self.scenarios, ["raw", "pixel"],
                              ["task"], 1, assert_context_parity=True)

        self.assertTrue(report["context_audit"]["required"])
        self.assertEqual(report["context_audit"]["checks"], [{
            "task": "task", "rep": "1", "static_context_equal": True,
            "pixel_calls": 0, "pixel_calls_zero": True,
        }])

    def test_context_parity_rejects_different_context_and_pixel_use(self):
        write_scenario(self.scenarios, "task", [("answer", 1)])
        write_transcript(self.results, "raw", "task", 1, "answer")
        write_transcript(self.results, "pixel", "task", 1, "answer",
                         commands=["pixel find-code callers"])
        write_context(self.results, "raw", 1)
        write_context(self.results, "pixel", 1, developer="pixel-injected")

        with self.assertRaisesRegex(ValueError, "static context differs"):
            rank_results(self.results, self.scenarios, ["raw", "pixel"],
                          ["task"], 1, assert_context_parity=True)

        write_context(self.results, "pixel", 1)
        with self.assertRaisesRegex(ValueError, "zero Pixel calls"):
            rank_results(self.results, self.scenarios, ["raw", "pixel"],
                          ["task"], 1, assert_context_parity=True)

    def test_context_parity_requires_captured_manifests(self):
        write_scenario(self.scenarios, "task", [("answer", 1)])
        for arm in ("raw", "pixel"):
            write_transcript(self.results, arm, "task", 1, "answer")
        with self.assertRaisesRegex(ValueError, "missing manifest"):
            rank_results(self.results, self.scenarios, ["raw", "pixel"],
                          ["task"], 1, assert_context_parity=True)

    def test_skill_only_profile_allows_only_candidate_discovery_difference(self):
        write_scenario(self.scenarios, "task", [("answer", 1)])
        for arm in ("raw", "pixel"):
            write_transcript(self.results, arm, "task", 1, "answer")
            write_context(self.results, arm, 1)
        candidate = {
            "sha256": "candidate-hash", "files": 2,
            "allow_implicit_invocation": True, "policy_file_present": True,
        }
        pixel_path = self.results / "context-pixel-1.json"
        pixel_context = json.loads(pixel_path.read_text())
        pixel_context["skills"]["repo:/repo/.agents/skills/pixel-impact"] = candidate
        pixel_path.write_text(json.dumps(pixel_context))

        report = rank_results(self.results, self.scenarios, ["raw", "pixel"],
                              ["task"], 1, assert_skill_only="pixel-impact")

        self.assertEqual(report["skill_audit"]["checks"][0]["candidate_skill_sha256"],
                         "candidate-hash")
        self.assertEqual(report["rows"][1]["candidate_skill_file_reads"], 0)

    def test_skill_only_profile_rejects_pixel_instruction_or_hook_differences(self):
        write_scenario(self.scenarios, "task", [("answer", 1)])
        for arm in ("raw", "pixel"):
            write_transcript(self.results, arm, "task", 1, "answer")
            write_context(self.results, arm, 1)
        pixel_path = self.results / "context-pixel-1.json"
        pixel_context = json.loads(pixel_path.read_text())
        pixel_context["pixel_reference_lines"] = 1
        pixel_context["skills"]["repo:/repo/.agents/skills/pixel-impact"] = {
            "sha256": "candidate-hash", "files": 2,
            "allow_implicit_invocation": True, "policy_file_present": True,
        }
        pixel_path.write_text(json.dumps(pixel_context))

        with self.assertRaisesRegex(ValueError, "Pixel instruction/hook text"):
            rank_results(self.results, self.scenarios, ["raw", "pixel"],
                         ["task"], 1, assert_skill_only="pixel-impact")

    def test_skill_only_profile_rejects_other_shared_pixel_skills(self):
        write_scenario(self.scenarios, "task", [("answer", 1)])
        for arm in ("raw", "pixel"):
            write_transcript(self.results, arm, "task", 1, "answer")
            write_context(self.results, arm, 1)
        raw_path = self.results / "context-raw-1.json"
        pixel_path = self.results / "context-pixel-1.json"
        candidate = {
            "sha256": "candidate-hash", "files": 2,
            "allow_implicit_invocation": True, "policy_file_present": True,
            "pixel_reference_lines": 2,
        }
        shared_pixel_skill = {
            "sha256": "old-pixel-skill", "files": 1,
            "allow_implicit_invocation": None, "policy_file_present": False,
            "pixel_reference_lines": 3,
        }
        raw_context = json.loads(raw_path.read_text())
        pixel_context = json.loads(pixel_path.read_text())
        raw_context["skills"]["$HOME/.agents/skills/pixel"] = shared_pixel_skill
        pixel_context["skills"]["$HOME/.agents/skills/pixel"] = shared_pixel_skill
        pixel_context["skills"]["repo:/repo/.agents/skills/pixel-impact"] = candidate
        raw_path.write_text(json.dumps(raw_context))
        pixel_path.write_text(json.dumps(pixel_context))

        with self.assertRaisesRegex(ValueError, "other Pixel skill context"):
            rank_results(self.results, self.scenarios, ["raw", "pixel"],
                         ["task"], 1, assert_skill_only="pixel-impact")

    def test_context_manifest_captures_codex_prompt_and_agent_files(self):
        repo = self.root / "context-repo"
        codex_home = self.root / "codex-home"
        (repo / "node_modules").mkdir(parents=True)
        codex_home.mkdir()
        (repo / "AGENTS.md").write_text("repo instructions")
        (repo / "node_modules" / "AGENTS.md").write_text("generated")
        (codex_home / "AGENTS.md").write_text("global instructions")
        (codex_home / "config.toml").write_text('developer_instructions = "static prompt"\n')

        manifest = collect_manifest(repo, codex_home)

        self.assertEqual(manifest["developer_instructions"], {
            "nonempty": True,
            "sha256": hashlib.sha256(b"static prompt").hexdigest(),
            "characters": len("static prompt"),
        })
        self.assertEqual(set(manifest["agents_files"]), {
            "$CODEX_HOME/AGENTS.md", "AGENTS.md",
        })

    def test_context_manifest_treats_missing_and_empty_prompt_as_same(self):
        repo = self.root / "context-repo"
        codex_home = self.root / "codex-home"
        repo.mkdir()
        codex_home.mkdir()
        missing = collect_manifest(repo, codex_home)
        (codex_home / "config.toml").write_text('developer_instructions = "  \\n  "\n')
        empty = collect_manifest(repo, codex_home)

        self.assertEqual(missing["developer_instructions"], empty["developer_instructions"])

    def test_context_manifest_records_skill_discovery_policy_and_hook_sources(self):
        repo = self.root / "context-repo"
        codex_home = self.root / "codex-home"
        skill_dir = repo / ".agents" / "skills" / "sample"
        (skill_dir / "agents").mkdir(parents=True)
        codex_home.mkdir()
        (skill_dir / "SKILL.md").write_text("name: sample\n")
        (skill_dir / "agents" / "openai.yaml").write_text(
            "policy:\n  allow_implicit_invocation: false\n"
        )
        (codex_home / "hooks.json").write_text('{"hooks": []}\n')

        manifest = collect_manifest(repo, codex_home)

        entry = manifest["skills"][f"repo:{repo}/.agents/skills/sample"]
        self.assertFalse(entry["allow_implicit_invocation"])
        self.assertTrue(entry["policy_file_present"])
        self.assertEqual(entry["files"], 2)
        self.assertIn("$CODEX_HOME/hooks.json", manifest["hook_config_files"])

    def test_missing_usage_fields_make_totals_unknown_but_zero_is_known(self):
        cases = [
            ("missing-input", {"cached_input_tokens": 0, "output_tokens": 3}, None, 3),
            ("missing-output", {"input_tokens": 8, "cached_input_tokens": 0}, 8, None),
            ("explicit-zero", {"input_tokens": 0, "cached_input_tokens": 0,
                                "output_tokens": 0}, 0, 0),
        ]
        for name, usage, expected_input, expected_output in cases:
            write_scenario(self.scenarios, name, [("answer", 1)])
            transcript = write_transcript(
                self.results, "raw", name, 1, "answer", usage_payload=usage,
            )
            write_transcript(self.results, "pixel", name, 1, "answer")
            _, metrics = load_result(transcript, "codex")
            self.assertEqual(metrics["input_tokens"], expected_input, name)
            self.assertEqual(metrics["gen_tokens"], expected_output, name)
            report = rank_results(self.results, self.scenarios, ["raw", "pixel"],
                                  [name], 1)
            raw_row = next(row for row in report["rows"] if row["arm"] == "raw")
            expected_total = (expected_input + expected_output
                              if expected_input is not None and expected_output is not None
                              else None)
            self.assertEqual(raw_row["tokens"], expected_total, name)


if __name__ == "__main__":
    unittest.main()
