#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Deterministic non-building prompt fact-pack experiment harness.

Runs the same model and harness over paired task families in four arms
(no-pixel, current-pixel, fact-auto, fact-ondemand) and applies the decision
rule of docs/bench/prompt-fact-pack-experiment.md.

The harness is deterministic and non-building: the fact packet is a pure
read of a warm daemon's published index (`pixel scope-task --read-only`), and
the harness never builds the project or writes a manifest. Pixel stays a fact
oracle; lifecycle routing is out of scope.

No provider request is made without explicit setup. The default offline mode
uses a deterministic local fake model and makes no network call; live mode
requires FACT_PACK_MODEL and FACT_PACK_CLI and refuses to run without them.

Usage:
  run.py --families <dir> --out <dir> --pixel <bin> [--limit N] [--live]
"""

import argparse
import json
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent / "lib"))

import arms  # noqa: E402
import decision  # noqa: E402
import packet  # noqa: E402
import record  # noqa: E402
import scenario  # noqa: E402

HARNESS_VERSION = "1"


def fake_model(arm_id: str, task: dict, pkt) -> dict:
    """A deterministic local stand-in for a model run. No provider request.

    The fake succeeds on every task; its elapsed time depends on the arm, so
    the four arms produce distinct trajectories and the decision rule has
    something to compare. fact-auto is fastest (the packet removes
    exploration), then fact-ondemand, then current-pixel, then no-pixel.
    """
    elapsed = {
        "no-pixel": 50.0,
        "current-pixel": 46.0,
        "fact-auto": 38.0,
        "fact-ondemand": 40.0,
    }[arm_id]
    return {
        "verified": "success",
        "elapsed_s": elapsed,
        "api_usage": {"cost_usd": round(elapsed * 0.007, 6), "input_tokens": 12000, "output_tokens": 800},
        "pixel_calls": 0 if arm_id == "no-pixel" else 2,
        "tool_calls": {"no-pixel": 10, "current-pixel": 8, "fact-auto": 6, "fact-ondemand": 7}[arm_id],
        "files_inspected": ["crates/pixel-ops/src/push.rs"],
        "test_time_s": None,
        "edits": 0,
        "rework": False,
        "packet_bytes": pkt.packet_bytes if pkt else 0,
    }


def live_model(arm_id: str, task: dict, pkt, model: str, cli: str) -> dict:
    """Run a real model through the harness CLI. Requires explicit setup."""
    raise RuntimeError("live mode is not wired to a provider in this harness; "
                       "set FACT_PACK_MODEL and FACT_PACK_CLI and implement the dispatch")


def run_experiment(families_dir: str, out_dir: str, pixel_bin: str,
                   limit: int = 20, live: bool = False,
                   model: str | None = None, cli: str | None = None) -> dict:
    """Run every task of every family in every arm, then apply the decision rule.

    Returns the verdict dict. Writes frozen-input and trajectory JSONL under
    `out_dir`. Makes no provider request unless `live` is set with explicit
    `model` and `cli`.
    """
    if live and (not model or not cli):
        raise RuntimeError("live mode requires both --model and --cli (explicit setup)")
    out = Path(out_dir)
    out.mkdir(parents=True, exist_ok=True)
    frozen_path = out / "frozen-inputs.jsonl"
    traj_path = out / "trajectories.jsonl"
    # A fresh run replaces the previous records.
    for path in (frozen_path, traj_path):
        if path.exists():
            path.unlink()

    families = sorted(Path(families_dir).glob("*.json"))
    if not families:
        raise RuntimeError(f"no task families in {families_dir}")

    trajectories_by_arm = {aid: [] for aid in arms.ARM_ORDER}
    packets_by_arm = {aid: [] for aid in arms.ARM_ORDER}
    ground_truth_by_pair = {}

    repo = record.repo_signature(".")
    for family_path in families:
        family = scenario.load(family_path)
        for task in family.get("tasks", []):
            pinned = task.get("commit")
            if pinned and pinned != repo["commit"]:
                raise RuntimeError(
                    f"task {task['id']} pinned to {pinned} but checkout is {repo['commit']}")
            truth = task.get("ground_truth")
            if truth:
                ground_truth_by_pair[task["id"]] = set(truth)
            for arm_id in arms.ARM_ORDER:
                arm = arms.arm(arm_id)
                pkt = None
                if arm.fact_delivery in ("auto-packet", "ondemand-full"):
                    budget = arm.packet_budget
                    pkt = packet.retrieve(task["prompt"], limit, pixel_bin, budget=budget)
                    pkt.pair_id = task["id"]
                    packets_by_arm[arm_id].append(pkt)
                    record.record_frozen_input(frozen_path, pkt, arm_id,
                                               family["id"], task["id"], repo)
                if live:
                    result = live_model(arm_id, task, pkt, model, cli)
                else:
                    result = fake_model(arm_id, task, pkt)
                trajectory = record.build_trajectory(
                    model=model or "fake", harness=HARNESS_VERSION, arm=arm_id,
                    task_family=family["id"], pair_id=task["id"], **result)
                record.record_trajectory(traj_path, trajectory)
                trajectories_by_arm[arm_id].append(trajectory)

    verdicts = {}
    for candidate in arms.CANDIDATE_ARMS:
        verdicts[candidate] = decision.evaluate_candidate(
            candidate, trajectories_by_arm, packets_by_arm,
            ground_truth_by_pair=ground_truth_by_pair)
    (out / "verdict.json").write_text(json.dumps(verdicts, indent=2, sort_keys=True) + "\n")
    return verdicts


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--families", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--pixel", default="pixel")
    parser.add_argument("--limit", type=int, default=20)
    parser.add_argument("--live", action="store_true")
    parser.add_argument("--model")
    parser.add_argument("--cli")
    args = parser.parse_args()
    verdicts = run_experiment(args.families, args.out, args.pixel,
                              limit=args.limit, live=args.live,
                              model=args.model, cli=args.cli)
    print(json.dumps(verdicts, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
