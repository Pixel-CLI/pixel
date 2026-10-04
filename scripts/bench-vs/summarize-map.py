#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Aggregate the repo-map rows into the size-vs-coverage table.

Both axes are printed side by side and never combined into a single score: a
map is only "efficient" relative to how much of the tree it lets an agent find,
and the two tools deliberately sit at opposite ends of that trade-off.
"""
import json
import sys
from pathlib import Path

ARMS = (("stacklit", "stacklit derive"),
        ("pixel_list_areas", "pixel list-areas"),
        ("pixel_repo_map", "pixel repo-map --markdown"))


def main():
    hdr = (f"{'repo':14s} {'artifact':26s} {'tokens':>8s} {'file-cov':>9s} "
           f"{'dir-cov':>8s} {'src files':>10s}")
    print(hdr)
    print("-" * len(hdr))
    for p in sys.argv[1:]:
        d = json.load(open(p))
        for key, label in ARMS:
            a = d[key]
            if a.get("rc", 0) != 0 or a.get("generate_rc", 0) != 0:
                print(f"{d['repo'][:14]:14s} {label:26s} {'FAILED':>8s} "
                      f"rc={a.get('rc')} generate_rc={a.get('generate_rc')}")
                continue
            print(f"{d['repo'][:14]:14s} {label:26s} {a['approx_tokens']:8d} "
                  f"{a['path_coverage']:9.3f} {a['dir_coverage']:8.3f} "
                  f"{a['source_files']:10d}")
        print()


if __name__ == "__main__":
    main()
