#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Every verdict pixel-research-mvp gave a gumroad Ruby edge, by identity
(caller path, line, callee path, callee, callee line), from each results
folder's samples/*.tsv and judgments/*.tsv (id<TAB>verdict<TAB>reason):
the 2026-10-15 known.py, extended to the 2026-10-16 folder. An id that two
samples of one folder give different edges is skipped.
usage: python3 -I known.py RESULTS_DIR OUT.tsv"""
import csv
import glob
import os
import sys

csv.field_size_limit(10**9)
root, out = sys.argv[1], sys.argv[2]
known = {}
for folder in sorted(glob.glob(os.path.join(root, "2026-10-1*"))):
    if not os.path.isdir(os.path.join(folder, "judgments")):
        continue
    rows = {}
    for sp in glob.glob(os.path.join(folder, "samples", "*.tsv")):
        repo = os.path.basename(sp).split("-")[0]
        for r in csv.DictReader(open(sp), delimiter="\t"):
            if r.get("id") and r.get("caller_path") and r.get("callee_line") is not None:
                ident = (r["caller_path"], r["line"], r["callee_path"], r["callee"], r["callee_line"])
                rows.setdefault((repo, r["id"]), set()).add(ident)
    for jp in glob.glob(os.path.join(folder, "judgments", "*.tsv")):
        repo = os.path.basename(jp).split("-")[0]
        if repo not in ("campfire", "gumroad"):
            repo = "campfire" if os.path.basename(jp).startswith("cf") else "gumroad"
        for line in open(jp):
            p = line.rstrip("\n").split("\t")
            if len(p) < 2 or len(rows.get((repo, p[0]), ())) != 1:
                continue
            (ident,) = rows[(repo, p[0])]
            v = p[1].strip().lower()
            if v in ("correct", "wrong", "undecidable"):
                origin = os.path.basename(folder) + "/" + os.path.basename(jp) + ":" + p[0]
                known.setdefault((repo,) + ident, (v, origin))
with open(out, "w") as f:
    for k, (v, o) in sorted(known.items()):
        if k[0] == "gumroad":
            f.write("\t".join(list(k) + [v, o]) + "\n")
print(len(known), "verdicts,", sum(k[0] == "gumroad" for k in known), "on gumroad")
