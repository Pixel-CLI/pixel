#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Compare a native repeated candidate against the preserved published v4 rows."""
import argparse
import collections
import hashlib
import importlib.util
import json
from pathlib import Path
import sys


def read(path):
    return json.loads(path.read_text())


def rows(path):
    return [json.loads(line) for line in path.read_text().splitlines()]


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("source", "reference", "pair", "out"):
        parser.add_argument("--" + name, type=Path, required=True)
    args = parser.parse_args()
    scorer = args.source / "scripts/bench/tool-census/score_brief.py"
    assert digest(scorer) == "95bf349407a86091b7627f0d7726697478408b743345b32d4ed919da36233106"
    sys.path.insert(0, str(scorer.parent))
    spec = importlib.util.spec_from_file_location("integration_comparison", scorer)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    reference = {row["id"]: row for row in rows(args.reference / "native-repeat.jsonl")}
    reference_hashes = {row["id"]: row["brief_sha256"]
                        for row in rows(args.reference / "per-question.jsonl")}
    identity = read(args.reference / "identity.json")
    runs = [read(args.pair / ("integration-" + side) / "report.json") for side in ("base", "head")]
    assert len(reference) == 56
    assert all({r["id"] for r in run["sessions"]} == set(reference) for run in runs)
    assert all(run["result"]["status"] == "complete" and run["result"]["exit_code"] == 0 for run in runs)
    output = []
    baseline = []
    for a, b in zip(runs[0]["sessions"], runs[1]["sessions"]):
        qid = a["id"]
        assert qid == b["id"]
        old = reference[qid]["r1"]
        hashes = [reference_hashes[qid]] + [
            digest(args.pair / ("integration-" + side) / "briefs" / (qid + ".txt"))
            for side in ("base", "head")]
        stable = dict(text_equal=hashes[1] == hashes[2], status_equal=a["status"] == b["status"],
                      metrics_equal={k: v for k, v in a["metrics"].items() if k != "elapsed_ms"}
                      == {k: v for k, v in b["metrics"].items() if k != "elapsed_ms"})
        row = dict(id=qid, category=a["category"], repo=a["repo"], pin=a["sha"], baseline=old,
                   r1=dict(status=a["status"], metrics=a["metrics"], meta=a["brief_meta"]),
                   r2=dict(status=b["status"], metrics=b["metrics"], meta=b["brief_meta"]),
                   brief_sha256=hashes, stability=stable,
                   delta={k: a["metrics"][k] - old["metrics"][k]
                          for k in ("file_recall", "region_cov", "line_cov", "inline_cov")})
        output.append(row)
        baseline.append(dict(id=qid, category=a["category"], mode="answer", status=old["status"],
                             metrics=old["metrics"], brief_meta={}))
        print(qid, "stable" if all(stable.values()) else "UNSTABLE", row["delta"])
    categories = []
    for category in sorted({row["category"] for row in output}):
        groups = [[r for r in group if r["category"] == category]
                  for group in (baseline, runs[0]["sessions"], runs[1]["sessions"])]
        aggregates = [module.aggregate(group) for group in groups]
        aggregates[0]["latency"] = None
        categories.append(dict(category=category, n=len(groups[0]), baseline=aggregates[0],
                               r1=aggregates[1], r2=aggregates[2],
                               statuses=[dict(collections.Counter(r["status"] for r in group))
                                         for group in groups]))
    summary = dict(protocol=runs[0]["protocol"], baseline=module.aggregate(baseline),
                   r1=runs[0]["means"]["all"], r2=runs[1]["means"]["all"],
                   results=[run["result"] for run in runs],
                   changed_questions=[r["id"] for r in output if any(r["delta"].values())],
                   changed_texts=[r["id"] for r in output if r["brief_sha256"][0] != r["brief_sha256"][1]],
                   unstable=[r["id"] for r in output if not all(r["stability"].values())])
    summary["baseline"]["latency"] = {"published_internal_p95_ms": identity["native_summary"]["internal_p95_ms"]}
    args.out.mkdir(parents=True, exist_ok=False)
    for name, data in (("per-question.jsonl", output), ("per-category.jsonl", categories)):
        (args.out / name).write_text("".join(json.dumps(r, separators=(",", ":")) + "\n" for r in data))
    (args.out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print("SUMMARY", len(output), "questions;", len(categories), "categories; unstable:", summary["unstable"])
    return int(bool(summary["unstable"]))


if __name__ == "__main__":
    sys.exit(main())
