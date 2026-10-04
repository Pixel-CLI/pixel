#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Turn results/scores.json into the host × task-class verdict against baseline.

  report.py --results eval/results [--baseline baseline] [--tie-margin 0.05] [--json out.json]

Every comparison is paired: an arm's run of (host, scenario, rep) against
the baseline's run of the same cell. Quality is score.py's `quality` (an
answer task's rubric score/max, an edit task's held-out verdict 1/0). A pair
is a win when the arm's quality beats the baseline's by more than the tie
margin, a loss when it trails by more, a tie otherwise. A pair with an
unknown quality on either side is counted as `unknown`, never as a tie.

Hosts are never pooled: each host gets its own table, and a host's "all"
row pools task classes of that host only. Efficiency (wall time, tokens,
tool calls) is reported beside the verdict as the median per-pair ratio
arm/baseline, never folded into it. The sign test is the exact two-sided
binomial test on wins against losses (ties dropped): with n=3 pairs per
cell no cell can reach p < 0.25, which is the point of printing it.
"""
import argparse
import json
import math
import statistics
import sys
from collections import defaultdict
from pathlib import Path


def sign_test(wins: int, losses: int):
    n = wins + losses
    if n == 0:
        return None
    k = min(wins, losses)
    p = 2 * sum(math.comb(n, i) for i in range(k + 1)) / 2 ** n
    return min(1.0, p)


def tokens(row):
    total = row.get("total_input_tokens")
    if total is None:
        total = row.get("input_tokens")
    gen = row.get("gen_tokens")
    return None if total is None or gen is None else total + gen


def ratio(a, b):
    return None if a is None or b is None or b == 0 else a / b


def median_or_none(values):
    values = [v for v in values if v is not None]
    return (statistics.median(values), len(values)) if values else (None, 0)


def compare(rows, baseline, margin):
    """{host: {task_class: {arm: cell}}} with "all" pooling classes within a host."""
    cells = {}
    for r in rows:
        key = (r["cli"], r["scenario"], r.get("rep"), r["arm"])
        if key in cells:
            # Two runs without a rep (legacy transcripts outside rep-N) share
            # a key: say so instead of silently keeping the last one.
            print(f"report: duplicate run {key}; only the last is compared", file=sys.stderr)
        cells[key] = r
    out = defaultdict(lambda: defaultdict(dict))
    arms = sorted({r["arm"] for r in rows} - {baseline})
    hosts = sorted({r["cli"] for r in rows})
    for host in hosts:
        for arm in arms:
            per_class = defaultdict(list)
            for (cli, scenario, rep, a), row in cells.items():
                if cli != host or a != arm:
                    continue
                base = cells.get((cli, scenario, rep, baseline))
                for cls in (row.get("task_class") or "unclassified", "all"):
                    per_class[cls].append((row, base))
            for cls, pairs in per_class.items():
                w = t = l = unknown = unpaired = 0
                deltas, wall, tok, tools = [], [], [], []
                for row, base in pairs:
                    if base is None:
                        unpaired += 1
                        continue
                    qa, qb = row.get("quality"), base.get("quality")
                    if qa is None or qb is None:
                        unknown += 1
                    else:
                        d = qa - qb
                        deltas.append(d)
                        if d > margin:
                            w += 1
                        elif d < -margin:
                            l += 1
                        else:
                            t += 1
                    wall.append(ratio(row.get("wall_ms"), base.get("wall_ms")))
                    tok.append(ratio(tokens(row), tokens(base)))
                    tools.append(ratio(row.get("tool_calls"), base.get("tool_calls")))
                mean = statistics.mean(deltas) if deltas else None
                if not deltas:
                    verdict = "unknown"
                elif w > l and mean > margin:
                    verdict = "win"
                elif l > w and mean < -margin:
                    verdict = "loss"
                else:
                    verdict = "tie"
                out[host][cls][arm] = {
                    "n": len(deltas), "wins": w, "ties": t, "losses": l,
                    "unknown": unknown, "unpaired": unpaired,
                    "mean_delta": mean,
                    "min_delta": min(deltas) if deltas else None,
                    "max_delta": max(deltas) if deltas else None,
                    "sign_p": sign_test(w, l), "verdict": verdict,
                    "wall_ratio": median_or_none(wall), "token_ratio": median_or_none(tok),
                    "tool_ratio": median_or_none(tools),
                }
    return out


def adoption(rows):
    """{host: {arm: numbers}}: does the agent use pixel, and how does it read after a hit."""
    out = defaultdict(dict)
    groups = defaultdict(list)
    for r in rows:
        groups[(r["cli"], r["arm"])].append(r)
    for (host, arm), rs in sorted(groups.items()):
        known = [r for r in rs if r.get("pixel_calls") is not None]
        pixel = sum(r["pixel_calls"] for r in known)
        native = sum(r.get("native_search_calls") or 0 for r in known)
        widths = [w for r in known for w in (r.get("pixel_hit_read_widths") or [])]
        bounded = [w for w in widths if w is not None]
        out[host][arm] = {
            "runs": len(rs), "runs_with_metrics": len(known),
            "runs_using_pixel": sum(1 for r in known if r["pixel_calls"] > 0),
            "pixel_calls": pixel, "native_search_calls": native,
            "pixel_share": (pixel / (pixel + native)) if pixel + native else None,
            "mean_pixel_calls": (pixel / len(known)) if known else None,
            "mean_native_search_calls": (native / len(known)) if known else None,
            "reads_after_hit": len(widths), "full_file_reads_after_hit": len(widths) - len(bounded),
            "median_read_lines_after_hit": statistics.median(bounded) if bounded else None,
        }
    return out


def fmt(value, spec="{:+.2f}"):
    return "-" if value is None else spec.format(value)


def fmt_ratio(pair):
    value, n = pair
    return "-" if value is None else f"{value:.2f}x (n{n})"


def render(verdicts, adopt, baseline, margin):
    lines = [f"Paired against `{baseline}`, tie margin ±{margin:g} quality. Hosts are never pooled.", ""]
    for host in sorted(verdicts):
        lines += [f"### {host}", "",
                  "| task_class | arm | n | W/T/L | verdict | mean Δq [min, max] | sign p | wall | tokens | tools | unknown/unpaired |",
                  "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |"]
        classes = sorted(c for c in verdicts[host] if c != "all") + ["all"]
        for cls in classes:
            for arm, c in sorted(verdicts[host].get(cls, {}).items()):
                lines.append(
                    f"| {cls} | {arm} | {c['n']} | {c['wins']}/{c['ties']}/{c['losses']} | {c['verdict']} | "
                    f"{fmt(c['mean_delta'])} [{fmt(c['min_delta'])}, {fmt(c['max_delta'])}] | "
                    f"{fmt(c['sign_p'], '{:.2f}')} | {fmt_ratio(c['wall_ratio'])} | {fmt_ratio(c['token_ratio'])} | "
                    f"{fmt_ratio(c['tool_ratio'])} | {c['unknown']}/{c['unpaired']} |")
        lines += ["", f"Pixel adoption on {host}:", "",
                  "| arm | runs | runs using pixel | pixel calls (mean) | native searches (mean) | pixel share | reads after a hit | full-file | median lines |",
                  "| --- | --- | --- | --- | --- | --- | --- | --- | --- |"]
        for arm, a in sorted(adopt.get(host, {}).items()):
            lines.append(
                f"| {arm} | {a['runs']} ({a['runs_with_metrics']} measured) | {a['runs_using_pixel']} | "
                f"{a['pixel_calls']} ({fmt(a['mean_pixel_calls'], '{:.1f}')}) | "
                f"{a['native_search_calls']} ({fmt(a['mean_native_search_calls'], '{:.1f}')}) | "
                f"{fmt(a['pixel_share'], '{:.0%}')} | {a['reads_after_hit']} | {a['full_file_reads_after_hit']} | "
                f"{fmt(a['median_read_lines_after_hit'], '{:g}')} |")
        lines.append("")
    return "\n".join(lines)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--results", required=True)
    ap.add_argument("--baseline", default="baseline")
    ap.add_argument("--tie-margin", type=float, default=0.05)
    ap.add_argument("--json")
    args = ap.parse_args()
    if args.tie_margin < 0:
        ap.error("tie margin must be nonnegative")
    rows = json.loads((Path(args.results) / "scores.json").read_text())
    if not any(r["arm"] == args.baseline for r in rows):
        raise SystemExit(f"no `{args.baseline}` rows in {args.results}/scores.json")
    verdicts = compare(rows, args.baseline, args.tie_margin)
    adopt = adoption(rows)
    print(render(verdicts, adopt, args.baseline, args.tie_margin))
    if args.json:
        Path(args.json).write_text(json.dumps({"baseline": args.baseline, "tie_margin": args.tie_margin,
                                               "verdicts": verdicts, "adoption": adopt}, indent=2))


if __name__ == "__main__":
    main()
