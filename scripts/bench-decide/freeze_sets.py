#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Freeze the two decide-bakeoff eval sets (protocol: docs/bench/decide-bakeoff-protocol.md).

Outputs, both under scripts/fixtures/:
  decide-coding-subset.jsonl   14 public JevBench items whose topic is `coding`,
                               normalized to the `pixel classify` spec shape.
  decide-plan-routing.jsonl    every plan-routing-real-gold.jsonl item x 5 labels,
                               decomposed to one `noul` spec per label, plus an
                               item index for exact-set scoring. The counts of
                               the last freeze are in decide-manifest.json
                               (n_routing_items, n_routing_specs).

Spec shape per line:
  {id, set, family, qtype, text, context, labels, criteria, expected, gold_labels?}

Sources are read-only inputs; this script changes no model and no routing.
"""
import argparse
import hashlib
import json
from pathlib import Path

JEVBENCH = Path("/tmp/jevbench/datasets")
OUT = Path(__file__).resolve().parent.parent / "fixtures"

PLAN_LABELS = ["dead-interactive", "dead-code", "hotspots", "recent-changes", "by-concept"]
# Descriptions transcribed from crates/pixel-graph/src/plan.rs PlanQuery docs
# (source sha256 recorded in the manifest at freeze time).
PLAN_DESCRIPTIONS = {
    "dead-interactive": "Find JSX elements missing event handlers — dead buttons, links or navigation.",
    "dead-code": "Find functions/methods with zero callers — dead code.",
    "hotspots": "Find files with the highest fan-in — most depended on, fix-first hotspots.",
    "recent-changes": "Find files changed in recent git history — recent churn marks the likely bug area.",
    "by-concept": "Resolve a named concept or term in the task to matching symbols via the concept index.",
}
PLAN_INSTRUCTIONS = (
    "A coding agent is deciding which deterministic repository analyses to run for the task below. "
    "Should it run `{label}` — {desc} Answer yes only if the task calls for that kind of analysis."
)


def spec_from_jevbench(row, topics):
    q = row["question"]
    qtype = q["type"]
    labels = [str(l) for l in row["labels"]]
    crit = q.get("criteria")
    if qtype == "noul":
        criteria = {"yes": str(crit.get("true", "yes")), "no": str(crit.get("false", "no"))} if isinstance(crit, dict) else {l: l for l in labels}
    elif qtype == "score":
        criteria = {str(i): str(c) for i, c in enumerate(crit or [])}
    else:
        criteria = {str(k): str(v) for k, v in (crit or {}).items()}
        for l in labels:
            criteria.setdefault(l, l)
    state = row["state"]
    text = state if isinstance(state, str) else json.dumps(state, ensure_ascii=False)
    return {
        "id": row["id"],
        "set": "coding-public",
        "family": row["family"],
        "qtype": qtype,
        "text": text,
        "context": q["instructions"],
        "labels": labels,
        "criteria": criteria,
        "expected": str(row["expected"]),
        "topic": topics.get(row["id"]),
    }


def plan_routing_specs(gold_rows, request_rows):
    texts = {r["id"]: r["text"] for r in request_rows}
    missing = [r["id"] for r in gold_rows if r["id"] not in texts]
    if missing:
        raise SystemExit(f"gold ids missing from request texts: {missing}")
    lines, index = [], {}
    for row in gold_rows:
        gold = set(row["labels"])
        index[row["id"]] = sorted(gold)
        for label in PLAN_LABELS:
            yes = label in gold
            lines.append({
                "id": f"{row['id']}::{label}",
                "item": row["id"],
                "set": "plan-routing",
                "family": row.get("family", "plan-routing"),
                "qtype": "noul",
                "text": texts[row["id"]],
                "context": PLAN_INSTRUCTIONS.format(label=label, desc=PLAN_DESCRIPTIONS[label]),
                "labels": ["no", "yes"],
                "criteria": {
                    "yes": f"The task calls for this analysis: {PLAN_DESCRIPTIONS[label]}",
                    "no": "The task does not call for this analysis.",
                },
                "expected": "yes" if yes else "no",
                "gold_label": label,
            })
    return lines, index


def sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--jevbench", default=str(JEVBENCH), help="path to jevbench datasets dir")
    args = ap.parse_args()
    base = Path(args.jevbench)

    topics = json.loads((base / "topics.json").read_text())["public"]
    coding = {k for k, v in topics.items() if v == "coding"}
    out_rows = []
    for name in ["easy", "original", "hard"]:
        for line in (base / "public" / f"{name}.jsonl").read_text().splitlines():
            if not line.strip():
                continue
            row = json.loads(line)
            if row["id"] in coding:
                out_rows.append(spec_from_jevbench(row, topics))
    out_rows.sort(key=lambda r: r["id"])
    coding_path = OUT / "decide-coding-subset.jsonl"
    coding_path.write_text("".join(json.dumps(r, ensure_ascii=False) + "\n" for r in out_rows))

    fixtures = Path(__file__).resolve().parent.parent / "fixtures"
    gold_path = fixtures / "plan-routing-real-gold.jsonl"
    gold = [json.loads(l) for l in gold_path.read_text().splitlines() if l.strip()]
    requests_path = fixtures / "plan-routing-real-requests.json"
    requests = json.loads(requests_path.read_text())["requests"]
    spec_lines, index = plan_routing_specs(gold, requests)
    routing_path = OUT / "decide-plan-routing.jsonl"
    routing_path.write_text("".join(json.dumps(r, ensure_ascii=False) + "\n" for r in spec_lines))
    (OUT / "decide-plan-routing-index.json").write_text(json.dumps(index, indent=1, sort_keys=True) + "\n")

    plan_rs = Path(__file__).resolve().parent.parent.parent / "crates" / "pixel-graph" / "src" / "plan.rs"
    manifest = {
        "coding_subset_sha256": sha256(coding_path),
        "plan_routing_sha256": sha256(routing_path),
        "plan_routing_index_sha256": sha256(OUT / "decide-plan-routing-index.json"),
        "plan_rs_sha256": sha256(plan_rs),
        "topics_sha256": sha256(base / "topics.json"),
        "gold_sha256": sha256(gold_path),
        "requests_sha256": sha256(requests_path),
        "n_coding": len(out_rows),
        "n_routing_specs": len(spec_lines),
        "n_routing_items": len(index),
    }
    (OUT / "decide-manifest.json").write_text(json.dumps(manifest, indent=1, sort_keys=True) + "\n")
    print(json.dumps(manifest, indent=1))


if __name__ == "__main__":
    main()
