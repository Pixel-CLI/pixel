#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Score decide-bakeoff raw runs (protocol: docs/bench/decide-bakeoff-protocol.md).

Usage: score.py --run <dir> [--baseline pixel-static]

Per model x set: accuracy (and plan-routing exact-set via the item index),
macro-F1, Brier, ECE-10, risk-coverage at top-probability {0.5,0.7,0.9},
latency p50/p95 (first record excluded as warm-up), and paired bootstrap 95%
CIs vs the baseline (2,000 resamples, seed 42, not stratified):
  delta_acc_ci95_vs_baseline        per-spec accuracy, resampling specs;
  delta_exact_set_ci95_vs_baseline  plan-routing only: exact-set, resampling
                                    items (all five label specs of an item
                                    move together), the integration gate's CI.
Writes <run>/summary.json and prints a table.
"""
from __future__ import annotations

import argparse
import json
import math
import random
from pathlib import Path

LABELS_ROUTING = ["dead-interactive", "dead-code", "hotspots", "recent-changes", "by-concept"]


def load_raw(path):
    return [json.loads(l) for l in Path(path).read_text().splitlines() if l.strip()]


def ece(records, bins=10):
    total, err = 0, 0.0
    buckets = [[] for _ in range(bins)]
    for r in records:
        if not r["ok"]:
            continue
        conf = max(r["probs"].values())
        hit = r["predicted"] == r["expected"]
        buckets[min(int(conf * bins), bins - 1)].append((conf, hit))
        total += 1
    for b in buckets:
        if b:
            acc = sum(h for _, h in b) / len(b)
            conf = sum(c for c, _ in b) / len(b)
            err += len(b) * abs(acc - conf)
    return err / total if total else None


def brier(records):
    vals = []
    for r in records:
        if not r["ok"]:
            continue
        vals.append(sum((p - float(l == r["expected"])) ** 2
                        for l, p in r["probs"].items()) / len(r["probs"]))
    return sum(vals) / len(vals) if vals else None


def macro_f1(records):
    labels = sorted({l for r in records for l in r["labels"]})
    f1s = []
    for lab in labels:
        tp = sum(1 for r in records if r["ok"] and r["predicted"] == lab and r["expected"] == lab)
        fp = sum(1 for r in records if r["ok"] and r["predicted"] == lab and r["expected"] != lab)
        fn = sum(1 for r in records if r["ok"] and r["predicted"] != lab and r["expected"] == lab)
        f1s.append(2 * tp / (2 * tp + fp + fn) if 2 * tp + fp + fn else 0.0)
    return sum(f1s) / len(f1s) if f1s else None


def risk_coverage(records):
    out = []
    for t in (0.5, 0.7, 0.9):
        sel = [r for r in records if r["ok"] and max(r["probs"].values()) >= t]
        out.append({"threshold": t, "n": len(sel),
                    "coverage": len(sel) / len(records) if records else 0,
                    "risk": (sum(r["predicted"] != r["expected"] for r in sel) / len(sel)) if sel else None})
    return out


def latency(records):
    xs = sorted(r["ms"] for r in records[1:] if r["ok"])
    if not xs:
        return {"p50": None, "p95": None}
    return {"p50": xs[len(xs) // 2], "p95": xs[min(int(len(xs) * 0.95), len(xs) - 1)]}


def routing_predictions(records):
    """Predicted label set per plan-routing item: labels whose spec answered
    yes >= 0.5. A failed spec contributes no label (a miss, never a drop)."""
    by_item = {}
    for r in records:
        if r["set"] != "plan-routing" or not r["ok"]:
            continue
        item, label = r["id"].rsplit("::", 1)
        by_item.setdefault(item, {})[label] = r["probs"].get("yes", 0.0) >= 0.5
    return {item: {l for l, yes in labels.items() if yes} for item, labels in by_item.items()}


def routing_exact_hits(records, index):
    """{item: 1 if the predicted label set equals the gold set, else 0}."""
    preds = routing_predictions(records)
    return {item: int(preds.get(item, set()) == set(gold)) for item, gold in index.items()}


def routing_exact_set(records, index):
    preds = routing_predictions(records)
    exact = sum(routing_exact_hits(records, index).values())
    per_label = {l: {"tp": 0, "fp": 0, "fn": 0} for l in LABELS_ROUTING}
    for item, gold in index.items():
        pred = preds.get(item, set())
        gold = set(gold)
        for l in LABELS_ROUTING:
            per_label[l]["tp"] += int(l in gold and l in pred)
            per_label[l]["fp"] += int(l not in gold and l in pred)
            per_label[l]["fn"] += int(l in gold and l not in pred)
    f1 = [2 * c["tp"] / (2 * c["tp"] + c["fp"] + c["fn"]) if 2 * c["tp"] + c["fp"] + c["fn"] else 0
          for c in per_label.values()]
    return {"n_items": len(index), "exact_set": exact / len(index),
            "macro_f1": sum(f1) / len(f1), "per_label": per_label,
            "omissions": sum(c["fn"] for c in per_label.values())}


def _hit(record):
    return int(record["ok"] and record["predicted"] == record["expected"])


def _paired_bootstrap_ci(diffs, resamples=2000, seed=42):
    """95% percentile CI of the mean of paired differences, resampling units."""
    if not diffs:
        return None
    rng = random.Random(seed)
    deltas = sorted(sum(rng.choices(diffs, k=len(diffs))) / len(diffs)
                    for _ in range(resamples))
    return [deltas[int(0.025 * len(deltas))], deltas[int(0.975 * len(deltas))]]


def bootstrap_delta(records, baseline_records):
    """CI on the per-spec accuracy delta, resampling specs paired by id.

    On plan-routing the five label specs of one item are correlated, so this
    interval is narrower than, and not the same metric as, the exact-set gate:
    use `bootstrap_exact_set_delta` for that. This is the method that produced
    `delta_acc_ci95_vs_baseline` in runs/run1/summary.json.
    """
    base_by_id = {r["id"]: r for r in baseline_records}
    return _paired_bootstrap_ci([_hit(r) - _hit(base_by_id[r["id"]])
                                 for r in records if r["id"] in base_by_id])


def bootstrap_exact_set_delta(records, baseline_records, index):
    """CI on the plan-routing exact-set delta, resampling items paired by id.

    An item is the resampling unit (`id.rsplit("::", 1)[0]`): exact-set needs
    all five of its label decisions right, so they move together.
    """
    a = routing_exact_hits(records, index)
    b = routing_exact_hits(baseline_records, index)
    return _paired_bootstrap_ci([a[item] - b[item] for item in sorted(index)])


def metrics(records, index=None):
    ok = [r for r in records if r["ok"]]
    acc = sum(r["predicted"] == r["expected"] for r in ok) / len(ok) if ok else None
    out = {"n": len(records), "n_ok": len(ok), "n_err": len(records) - len(ok),
           "accuracy": acc, "macro_f1": macro_f1(ok), "brier": brier(ok),
           "ece": ece(ok), "risk_coverage": risk_coverage(ok), "latency_ms": latency(ok)}
    if index and any(r["set"] == "plan-routing" for r in ok):
        out["routing"] = routing_exact_set(ok, index)
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--run", required=True)
    ap.add_argument("--baseline", default="pixel-static")
    args = ap.parse_args()
    run = Path(args.run)
    index = json.loads((Path(__file__).resolve().parent.parent
                        / "fixtures" / "decide-plan-routing-index.json").read_text())

    raws = {p.stem.replace(".raw", ""): load_raw(p) for p in run.glob("*.raw.jsonl")}
    summary = {}
    base = raws.get(args.baseline)
    for name, records in sorted(raws.items()):
        per_set = {}
        for s in sorted({r["set"] for r in records}):
            sub = [r for r in records if r["set"] == s]
            m = metrics(sub, index if s == "plan-routing" else None)
            if base:
                base_sub = [r for r in base if r["set"] == s]
                m["delta_acc_ci95_vs_baseline"] = bootstrap_delta(sub, base_sub)
                if s == "plan-routing":
                    m["delta_exact_set_ci95_vs_baseline"] = bootstrap_exact_set_delta(
                        sub, base_sub, index)
            per_set[s] = m
        summary[name] = per_set

    (run / "summary.json").write_text(json.dumps(summary, indent=1, allow_nan=False) + "\n")

    hdr = f"{'model':16s} {'set':12s} {'n':>4s} {'acc':>7s} {'exact':>6s} {'f1':>6s} {'ece':>6s} {'p50ms':>8s} {'p95ms':>8s}"
    print(hdr)
    for name, per_set in summary.items():
        for s, m in per_set.items():
            exact = m.get("routing", {}).get("exact_set")
            print(f"{name:16s} {s:12s} {m['n']:4d} "
                  f"{(m['accuracy'] or 0):7.3f} {(exact if exact is not None else 0):6.3f} "
                  f"{(m['macro_f1'] or 0):6.3f} {(m['ece'] or 0):6.3f} "
                  f"{(m['latency_ms']['p50'] or 0):8.1f} {(m['latency_ms']['p95'] or 0):8.1f}")


if __name__ == "__main__":
    main()
