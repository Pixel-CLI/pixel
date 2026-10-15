#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Aggregate retrieval rows into the table used in the write-up.

Every arm is reported on every metric; a case where an arm returned nothing
counts as a miss rather than being dropped, so the denominators match.
"""
import json
import sys
from pathlib import Path
from statistics import mean

ARMS = ("semble", "pixel_search_meaning", "pixel_find_code")
LABEL = {"semble": "semble", "pixel_search_meaning": "pixel search-meaning",
         "pixel_find_code": "pixel find-code", "warpgrep": "warpgrep"}


def main():
    global ARMS
    per_corpus, allrows = [], []
    loaded = [(p, json.load(open(p))) for p in sys.argv[1:]]
    # WarpGrep is an optional arm (bench-retrieval.py docstring); report it only
    # when every row of every file carries it, so all arms share one denominator.
    if all("warpgrep_rank" in r for _, rows in loaded for r in rows):
        ARMS = ARMS + ("warpgrep",)
    for p, rows in loaded:
        failed = [(r["truth_file"], a) for r in rows for a in ARMS
                  if r.get(f"{a}_failed_reps")]
        if failed:
            print(f"WARNING: {len(failed)} arm-case pair(s) had a failed "
                  f"invocation in {Path(p).name}; they score 0 by construction "
                  f"and are listed below.", file=sys.stderr)
            for f, a in failed:
                print(f"  {a:22s} {f}", file=sys.stderr)
        allrows += rows
        per_corpus.append((Path(p).stem.replace("retrieval-", ""), rows))

    hdr = (f"{'corpus':16s} {'arm':22s} {'n':>3s} {'r@1':>6s} {'r@5':>6s} "
           f"{'r@10':>6s} {'p50 ms':>8s} {'bytes':>8s} {'files':>5s}")
    print(hdr)
    print("-" * len(hdr))
    for name, rows in per_corpus:
        for a in ARMS:
            print(f"{name:16s} {LABEL[a]:22s} {len(rows):3d} "
                  f"{mean(r[a+'_r1'] for r in rows):6.2f} "
                  f"{mean(r[a+'_r5'] for r in rows):6.2f} "
                  f"{mean(r[a+'_r10'] for r in rows):6.2f} "
                  f"{mean(r[a+'_ms_p50'] for r in rows):8.0f} "
                  f"{mean(r[a+'_bytes'] for r in rows):8.0f} "
                  f"{mean(r[a+'_returned'] for r in rows):5.1f}")
        print()
    print("-" * len(hdr))
    for a in ARMS:
        print(f"{'ALL':16s} {LABEL[a]:22s} {len(allrows):3d} "
              f"{mean(r[a+'_r1'] for r in allrows):6.2f} "
              f"{mean(r[a+'_r5'] for r in allrows):6.2f} "
              f"{mean(r[a+'_r10'] for r in allrows):6.2f} "
              f"{mean(r[a+'_ms_p50'] for r in allrows):8.0f} "
              f"{mean(r[a+'_bytes'] for r in allrows):8.0f} "
              f"{mean(r[a+'_returned'] for r in allrows):5.1f}")

    print("\nCases no arm found (query too generic to be answerable):")
    for r in allrows:
        if not any(r[a + "_r10"] for a in ARMS):
            print(f"  {r['truth_file'][-58:]:58s} {r['query'][:50]}")


if __name__ == "__main__":
    main()
