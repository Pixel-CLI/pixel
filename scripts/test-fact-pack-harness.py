#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Contract of the deterministic prompt fact-pack harness, offline.

No model is called and no provider request is made. The packet module runs
against a fake pixel that emits a canned `scope-task --read-only` response;
the full pipeline runs the four arms over a disposable task-family corpus
with the deterministic fake model; the decision rule is checked on
hand-written trajectories. The fake pixel's response carries enough targets
to overflow the 1 KiB budget, so the cap is exercised.
"""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent
FAC = ROOT / "eval" / "fact-pack"
sys.path.insert(0, str(FAC))
sys.path.insert(0, str(FAC / "lib"))

import arms  # noqa: E402
import decision  # noqa: E402
import packet  # noqa: E402
import record  # noqa: E402
import scenario  # noqa: E402
import run as harness  # noqa: E402

# A pre-push hook runs this script with GIT_DIR and friends exported.
GIT_LOCATION_VARS = ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY",
                     "GIT_ALTERNATE_OBJECT_DIRECTORIES", "GIT_COMMON_DIR", "GIT_PREFIX")

FAKE_PIXEL = """#!/usr/bin/env python3
import json, sys
# argv: scope-task --read-only <task> --json --limit <N>
task, limit = sys.argv[3], int(sys.argv[6])
targets = [{"path": f"src/file{i}.rs", "evidence": [{"keyword": "k", "line": 1, "text": "x" * 40}], "reasons": [f"defines symbol `Symbol{i}`", 'content matches: 3 for "push"']} for i in range(60)]
print(json.dumps({
    "status": "available",
    "inputs": {
        "task": task, "limit": limit, "index_commit_oid": "abc123",
        "index_base_files": 100, "index_delta_files": 0, "index_overlay_files": 0,
        "index_tombstones": 0, "graph_generation": 1, "graph_signature": "sig",
        "algorithm_version": 1, "activity_reranking": False, "semantic_fallback": False,
    },
    "facts": {"targets": targets, "envelope": {}, "stats": {}},
}))
"""


def clean_env(**extra):
    env = {k: v for k, v in os.environ.items() if k not in GIT_LOCATION_VARS}
    env.update(extra)
    return env


class ArmContracts(unittest.TestCase):
    def test_four_arms_are_distinct(self):
        self.assertEqual(arms.ARM_ORDER, ("no-pixel", "current-pixel", "fact-auto", "fact-ondemand"))
        self.assertEqual(arms.arm("no-pixel").fact_delivery, "none")
        self.assertEqual(arms.arm("current-pixel").fact_delivery, "installed-hooks")
        self.assertEqual(arms.arm("fact-auto").fact_delivery, "auto-packet")
        self.assertEqual(arms.arm("fact-ondemand").fact_delivery, "ondemand-full")
        self.assertEqual(arms.arm("fact-auto").packet_budget, 1024)
        self.assertIsNone(arms.arm("fact-ondemand").packet_budget)
        self.assertEqual(arms.arm("no-pixel").packet_budget, 0)

    def test_validate_arm_ids_rejects_unknown(self):
        self.assertEqual(arms.validate_arm_ids(arms.ARM_ORDER), [])
        self.assertTrue(arms.validate_arm_ids(["no-pixel", "bogus"]))
        self.assertTrue(arms.validate_arm_ids(["no-pixel", "no-pixel"]))


class PacketContracts(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.pixel = Path(self.tmp.name) / "fake-pixel"
        self.pixel.write_text(FAKE_PIXEL)
        self.pixel.chmod(0o755)

    def tearDown(self):
        self.tmp.cleanup()

    def test_packet_is_capped_at_1kib(self):
        pkt = packet.retrieve("some task", 20, str(self.pixel))
        self.assertTrue(pkt.available)
        self.assertLessEqual(pkt.packet_bytes, 1024)
        self.assertGreater(pkt.total_targets, pkt.kept_targets)

    def test_packet_is_deterministic(self):
        a = packet.retrieve("same task", 20, str(self.pixel))
        b = packet.retrieve("same task", 20, str(self.pixel))
        self.assertEqual(a.packet_text, b.packet_text)
        self.assertEqual(a.inputs, b.inputs)

    def test_frozen_inputs_captured(self):
        pkt = packet.retrieve("some task", 20, str(self.pixel))
        frozen = record.frozen_input_from_packet(pkt)
        self.assertEqual(frozen["task"], "some task")
        self.assertEqual(frozen["limit"], 20)
        self.assertEqual(frozen["algorithm_version"], 1)
        self.assertFalse(frozen["activity_reranking"])
        self.assertFalse(frozen["semantic_fallback"])

    def test_unavailable_response_contributes_no_packet(self):
        # A pixel that exits non-zero yields an unavailable packet with no bytes.
        bad = Path(self.tmp.name) / "bad-pixel"
        bad.write_text("#!/usr/bin/env python3\nimport sys\nsys.exit(3)\n")
        bad.chmod(0o755)
        pkt = packet.retrieve("some task", 20, str(bad))
        self.assertFalse(pkt.available)
        self.assertIsNone(pkt.packet_text)
        self.assertEqual(pkt.packet_bytes, 0)

    def test_ondemand_full_json_is_uncapped(self):
        pkt = packet.retrieve("some task", 20, str(self.pixel), budget=None)
        self.assertTrue(pkt.available)
        self.assertGreater(pkt.packet_bytes, 1024)
        self.assertEqual(pkt.kept_targets, pkt.total_targets)


class DecisionContracts(unittest.TestCase):
    def test_non_inferiority_positive(self):
        diff, lower, ok = decision.non_inferiority([1] * 10, [1] * 10)
        self.assertTrue(ok)
        self.assertEqual(diff, 0.0)

    def test_non_inferiority_negative(self):
        # Candidate fails everywhere the baseline succeeds: clearly inferior.
        diff, lower, ok = decision.non_inferiority([0] * 10, [1] * 10)
        self.assertFalse(ok)
        self.assertEqual(diff, -1.0)

    def test_non_inferiority_mixed(self):
        # 8/10 vs 6/10: difference +0.2, non-inferior.
        diff, lower, ok = decision.non_inferiority([1] * 8 + [0] * 2, [1] * 6 + [0] * 4)
        self.assertAlmostEqual(diff, 0.2)
        self.assertTrue(ok)

    def test_paired_time_improvement_meets_threshold(self):
        # Candidate is 20% faster on every pair.
        result = decision.paired_time_improvement([40.0] * 10, [50.0] * 10)
        self.assertAlmostEqual(result["median"], 0.2)
        self.assertTrue(result["meets_threshold"])

    def test_paired_time_improvement_below_threshold(self):
        result = decision.paired_time_improvement([49.0] * 10, [50.0] * 10)
        self.assertAlmostEqual(result["median"], 0.02)
        self.assertFalse(result["meets_threshold"])

    def test_evaluate_candidate_disjoint_success_produces_no_time_observations(self):
        # Candidate succeeds where baseline fails and vice versa: no paired
        # time observations survive the both-succeeded filter.
        trajectories = {
            "fact-auto": [
                {"task_family": "f1", "pair_id": "t1", "verified": "success", "elapsed_s": 10.0},
                {"task_family": "f1", "pair_id": "t2", "verified": "success", "elapsed_s": 10.0},
                {"task_family": "f1", "pair_id": "t3", "verified": "failure", "elapsed_s": 10.0},
            ],
            "no-pixel": [
                {"task_family": "f1", "pair_id": "t1", "verified": "failure", "elapsed_s": 50.0},
                {"task_family": "f1", "pair_id": "t2", "verified": "failure", "elapsed_s": 50.0},
                {"task_family": "f1", "pair_id": "t3", "verified": "success", "elapsed_s": 50.0},
            ],
        }
        verdict = decision.evaluate_candidate("fact-auto", trajectories, {})
        self.assertEqual(verdict["time"]["no-pixel"]["n"], 0)
        self.assertFalse(verdict["time"]["no-pixel"]["meets_threshold"])

    def test_cost_per_verified_completion(self):
        self.assertAlmostEqual(decision.cost_per_verified_completion([1.0, 2.0, 3.0], [1, 1, 0]), 3.0)
        self.assertIsNone(decision.cost_per_verified_completion([1.0], [0]))

    def test_unavailable_rate(self):
        class P:
            def __init__(self, a):
                self.available = a
        self.assertEqual(decision.unavailable_rate([P(True), P(False), P(True), P(False)]), 0.5)
        self.assertEqual(decision.unavailable_rate([]), 0.0)

    def test_misleading_candidates(self):
        facts = {"targets": [{"path": "a.rs"}, {"path": "b.rs"}, {"path": "c.rs"}]}
        self.assertEqual(decision.misleading_candidates(facts, {"a.rs"}), ["b.rs", "c.rs"])


class PipelineContracts(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.pixel = Path(self.tmp.name) / "fake-pixel"
        self.pixel.write_text(FAKE_PIXEL)
        self.pixel.chmod(0o755)
        self.families = Path(self.tmp.name) / "families"
        self.families.mkdir()
        (self.families / "f1.json").write_text(json.dumps({
            "id": "f1",
            "tasks": [
                {"id": "f1-1", "prompt": "Task one", "mode": "answer", "must": [{"pattern": "x", "points": 1}],
                 "ground_truth": ["src/file0.rs", "src/file1.rs"]},
                {"id": "f1-2", "prompt": "Task two", "mode": "answer", "must": [{"pattern": "y", "points": 1}]},
            ],
        }))
        self.out = Path(self.tmp.name) / "out"

    def tearDown(self):
        self.tmp.cleanup()

    def test_offline_pipeline_records_and_verdicts(self):
        verdicts = harness.run_experiment(str(self.families), str(self.out), str(self.pixel))
        self.assertIn("fact-auto", verdicts)
        self.assertIn("fact-ondemand", verdicts)
        # 2 tasks x 4 arms = 8 trajectories.
        trajectories = record.read_jsonl(self.out / "trajectories.jsonl")
        self.assertEqual(len(trajectories), 8)
        arms_seen = {t["arm"] for t in trajectories}
        self.assertEqual(arms_seen, set(arms.ARM_ORDER))
        # fact-auto trajectories carry the capped packet; fact-ondemand carries
        # the full uncapped JSON; the two baselines carry none.
        for t in trajectories:
            if t["arm"] == "fact-auto":
                self.assertGreater(t["packet_bytes"], 0)
                self.assertLessEqual(t["packet_bytes"], 1024)
            elif t["arm"] == "fact-ondemand":
                self.assertGreater(t["packet_bytes"], 1024)
            else:
                self.assertEqual(t["packet_bytes"], 0)
        # Frozen inputs recorded for the two fact arms only: 2 tasks x 2 arms.
        frozen = record.read_jsonl(self.out / "frozen-inputs.jsonl")
        self.assertEqual(len(frozen), 4)
        # The verdict is non-inferior and improves on both baselines.
        for candidate in ("fact-auto", "fact-ondemand"):
            for baseline in ("no-pixel", "current-pixel"):
                self.assertTrue(verdicts[candidate]["non_inferiority"][baseline]["non_inferior"])
                self.assertTrue(verdicts[candidate]["time"][baseline]["meets_threshold"])
        # Misleading candidates are reported against the task's ground truth.
        misleading = verdicts["fact-auto"]["cost"]["misleading_candidates"]
        self.assertIn("src/file2.rs", misleading)
        self.assertNotIn("src/file0.rs", misleading)

    def test_live_mode_requires_explicit_setup(self):
        with self.assertRaises(RuntimeError):
            harness.run_experiment(str(self.families), str(self.out), str(self.pixel), live=True)

    def test_scenario_corpus_validates(self):
        defects = scenario.check(str(self.families))
        self.assertEqual(defects, [])
        self.assertEqual(scenario.family_ids(str(self.families)), ["f1"])


if __name__ == "__main__":
    unittest.main()
