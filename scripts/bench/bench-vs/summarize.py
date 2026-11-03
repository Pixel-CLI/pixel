#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Aggregate raw benchmark rows into the tables that go in the write-up.

Reports per-corpus means and the per-case spread. Nothing here rounds a loss
into a win: every metric is printed for both tools side by side, and cases
where a tool returned nothing are counted as recall 0, not dropped.

Precision is reported ONLY where the truth set is complete. The Ruby truth sets
are built by matching call syntax with parentheses, which Ruby callers routinely
omit, so they are a subset of the real call sites: a tool that correctly returns
a paren-less caller is scored as imprecise for being right. Recall survives that
(a subset of truth still bounds recall from below), precision does not, so Ruby
precision prints `n/a` and is excluded from the overall mean rather than being
averaged into a number the corpus cannot support.
"""
import json
import sys
from pathlib import Path
from statistics import mean


def precision_scorable(rows):
    """Ruby truth sets under-count paren-less calls; precision is not scorable."""
    return [r for r in rows if r.get("lang") != "ruby"]


def summarize(path):
    rows = json.load(open(path))
    name = Path(path).stem.replace("impact-", "")
    scorable = precision_scorable(rows)
    out = {"corpus": name, "cases": len(rows), "precision_cases": len(scorable)}
    for tool in ("pixel", "gitnexus"):
        out[f"{tool}_recall_d1"] = round(mean(r[f"{tool}_recall_d1"] for r in rows), 3)
        out[f"{tool}_precision_d1"] = (
            round(mean(r[f"{tool}_precision_d1"] for r in scorable), 3)
            if scorable else None)
        out[f"{tool}_recall_all"] = round(mean(r[f"{tool}_recall_all"] for r in rows), 3)
        out[f"{tool}_ms_p50"] = round(mean(r[f"{tool}_ms_p50"] for r in rows), 1)
        out[f"{tool}_bytes"] = round(mean(r[f"{tool}_bytes"] for r in rows))
        out[f"{tool}_zero_recall_cases"] = sum(
            1 for r in rows if r[f"{tool}_recall_d1"] == 0.0)
    return out, rows


def main():
    alls, allrows = [], []
    for p in sys.argv[1:]:
        s, rows = summarize(p)
        alls.append(s)
        allrows.extend(rows)

    hdr = f"{'corpus':22s} {'n':>3s} | {'px rec':>6s} {'gn rec':>6s} | " \
          f"{'px prec':>7s} {'gn prec':>7s} | {'px ms':>7s} {'gn ms':>7s} | " \
          f"{'px B':>7s} {'gn B':>7s}"
    print(hdr)
    print("-" * len(hdr))
    def prec(v):
        return f"{v:7.2f}" if v is not None else f"{'n/a':>7s}"

    for s in alls:
        print(f"{s['corpus']:22s} {s['cases']:3d} | "
              f"{s['pixel_recall_d1']:6.2f} {s['gitnexus_recall_d1']:6.2f} | "
              f"{prec(s['pixel_precision_d1'])} {prec(s['gitnexus_precision_d1'])} | "
              f"{s['pixel_ms_p50']:7.0f} {s['gitnexus_ms_p50']:7.0f} | "
              f"{s['pixel_bytes']:7d} {s['gitnexus_bytes']:7d}")
    n = len(allrows)
    pr = precision_scorable(allrows)
    print("-" * len(hdr))
    print(f"{'ALL':22s} {n:3d} | "
          f"{mean(r['pixel_recall_d1'] for r in allrows):6.2f} "
          f"{mean(r['gitnexus_recall_d1'] for r in allrows):6.2f} | "
          f"{prec(mean(r['pixel_precision_d1'] for r in pr))} "
          f"{prec(mean(r['gitnexus_precision_d1'] for r in pr))} | "
          f"{mean(r['pixel_ms_p50'] for r in allrows):7.0f} "
          f"{mean(r['gitnexus_ms_p50'] for r in allrows):7.0f} | "
          f"{mean(r['pixel_bytes'] for r in allrows):7.0f} "
          f"{mean(r['gitnexus_bytes'] for r in allrows):7.0f}")

    print(f"\nprecision: mean over {len(pr)}/{n} cases "
          f"(Ruby excluded, truth set incomplete by construction)")
    print("\nCases where a tool found NOTHING at depth 1:")
    for r in allrows:
        for tool in ("pixel", "gitnexus"):
            if r[f"{tool}_recall_d1"] == 0.0:
                print(f"  {tool:9s} {r['repo']:12s} {r['symbol']:38s} "
                      f"truth={r['truth_size']} reported_d1={r[f'{tool}_d1_count']}")

    print("\nPer-case head-to-head (recall d1):")
    for r in allrows:
        px, gn = r["pixel_recall_d1"], r["gitnexus_recall_d1"]
        flag = "=" if abs(px - gn) < 1e-9 else ("px" if px > gn else "GN")
        print(f"  {flag:2s} {r['repo']:12s} {r['symbol']:38s} "
              f"px={px:.2f} gn={gn:.2f}")


if __name__ == "__main__":
    main()
