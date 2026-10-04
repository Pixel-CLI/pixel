#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Rank arena arms: quality (rubric score), tokens, wall time, saving vs raw."""
import argparse, json, sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from score import load_result, score_answer  # noqa: E402


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--results", required=True)
    ap.add_argument("--scenarios-dir", required=True)
    ap.add_argument("--arms", nargs="+", required=True)
    ap.add_argument("--baseline-arm", default="raw")
    args = ap.parse_args()
    rubrics = {p.stem: json.loads(p.read_text())
               for p in Path(args.scenarios_dir).glob("*.json")}
    results = Path(args.results)

    rows = []  # arm, task, rep, score, max, tokens, seconds
    for f in sorted(results.rglob("*.jsonl")):
        stem = f.stem
        scenario, arm, rep = None, None, None
        for k in rubrics:
            idx = stem.find(f"-{k}-")
            if idx != -1:
                scenario, arm = k, stem[:idx]
                rep = stem[idx + len(k) + 2:]
                break
        if scenario is None or not arm:
            continue
        answer, metrics = load_result(f, "codex")
        if not metrics or not metrics.get("answered"):
            score = 0
        else:
            score, _, _ = score_answer(answer, rubrics[scenario])
        seconds = None
        sec_file = f.with_suffix(".seconds")
        if sec_file.exists():
            seconds = int(sec_file.read_text().strip() or 0)
        rows.append({"arm": arm, "task": scenario, "rep": rep, "score": score,
                     "max": sum(m["points"] for m in rubrics[scenario]["must"]),
                     "tokens": (metrics.get("input_tokens") or 0)
                               + (metrics.get("gen_tokens") or 0),
                     "answered": bool(metrics.get("answered")),
                     "seconds": seconds})

    arms = {}
    for r in rows:
        arms.setdefault(r["arm"], []).append(r)

    # means over the matched task set (tasks every arm has rows for), so arms
    # with partial runs are not averaged across different denominators
    matched = set.intersection(*(
        {(x["task"], x["rep"]) for x in arms[a]} for a in arms if arms[a]
    )) if arms else set()

    def agg(arm):
        rs = [x for x in arms.get(arm, []) if (x["task"], x["rep"]) in matched]
        if not rs:
            return None
        return {
            "score": sum(x["score"] for x in rs) / len(rs),
            "max": max(x["max"] for x in rs),
            "tokens": sum(x["tokens"] for x in rs),
            "seconds": sum(x["seconds"] or 0 for x in rs),
            "answered": sum(1 for x in rs if x["answered"]),
            "n": len(rs),
        }

    print(f"{'arm':<10} {'score':>10} {'tokens':>10} {'seconds':>8} {'saving':>9}  answered")
    summary = {}
    for arm in args.arms:
        a = agg(arm)
        if not a:
            print(f"{arm:<10} {'—':>10}")
            continue
        summary[arm] = a
    raw = summary.get(args.baseline_arm)
    for arm, a in summary.items():
        saving = ""
        if raw and raw["tokens"] and arm != args.baseline_arm:
            saved = raw["tokens"] - a["tokens"]
            pct = 100 * saved / raw["tokens"]
            saving = f"{saved:+,.0f} ({pct:+.0f}%)"
        print(f"{arm:<10} {a['score']:>5.1f}/{a['max']:<4} {a['tokens']:>10,} "
              f"{a['seconds']:>8} {saving:>9}  {a['answered']}/{a['n']}")

    ranked = sorted(summary.items(), key=lambda kv: (-kv[1]["score"], kv[1]["tokens"]))
    print("\nrank (quality desc, then tokens asc):")
    for i, (arm, a) in enumerate(ranked, 1):
        print(f"  {i}. {arm:<10} score {a['score']:.1f}  tokens {a['tokens']:,}  time {a['seconds']}s")
    (results / "ranking.json").write_text(json.dumps(
        {"rows": rows, "summary": summary, "ranked": [a for a, _ in ranked]}, indent=2))


if __name__ == "__main__":
    main()
