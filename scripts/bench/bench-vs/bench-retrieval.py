#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Natural-language retrieval: semble, pixel's two search engines and WarpGrep.

Each arm gets the identical query (a doc comment, see gen-queries.py) and is
scored on whether the file that comment documents appears in its top-k ranked
FILES, after de-duplicating repeated hits in the same file. recall@1/5/10, plus
what the answer cost in bytes and wall clock.

Arm order is rotated per query and recorded, a failed invocation is reported as
such rather than scored as a miss, and every timed repetition is kept alongside
the median.

`gitnexus query` is deliberately absent: it returns execution flows, not a
ranked file list, so scoring it here would measure it on a question it does not
claim to answer. Its retrieval path is `context`/`impact`, benchmarked
separately in vs-gitnexus.md.

pixel has two engines that both take a phrase, so both are run: `search-meaning`
(semantic) and `find-code` (concept index). Reporting only the better of the two
would flatter pixel by letting it pick per query.

WarpGrep (Morph's RL-trained search subagent) is a fourth arm when
WARPGREP_SCRIPT names `warpgrep-search.mjs` and MORPH_API_KEY is set. Each
search is a paid remote call, so it runs WARPGREP_REPS times with no discarded
warm-up: there is no local cache to warm. Its bytes are the code it hands the
agent (`content_bytes`), not its own metadata JSON, and its contexts come in the
order the model listed them, which is the only rank it has.
"""
import json
import os
import re
import subprocess
import sys
import tempfile
import time
from statistics import median
from pathlib import Path

TOPK = 10
REPS = 3


def run(cmd, cwd):
    with tempfile.NamedTemporaryFile(delete=False) as fh:
        tmp = fh.name
    try:
        with open(tmp, "wb") as out:
            t0 = time.perf_counter()
            p = subprocess.run(cmd, cwd=cwd, stdout=out,
                               stderr=subprocess.DEVNULL, timeout=600)
            ms = (time.perf_counter() - t0) * 1000
        return ms, Path(tmp).read_bytes(), p.returncode
    finally:
        Path(tmp).unlink(missing_ok=True)


def dedup(paths):
    seen, out = set(), []
    for p in paths:
        if p and p not in seen:
            seen.add(p)
            out.append(p)
    return out


def semble_files(raw, repo):
    try:
        d = json.loads(raw.decode(errors="replace"))
    except json.JSONDecodeError:
        return []
    return dedup(r.get("file_path", "") for r in d.get("results", []))


def warpgrep_files(raw, repo):
    try:
        d = json.loads(raw.decode(errors="replace"))
    except json.JSONDecodeError:
        return []
    return dedup(rel(r.get("file_path", ""), repo) for r in d.get("results", []))


def answer_bytes(tool, out):
    """What the agent receives: WarpGrep's stdout is metadata about the code."""
    if tool != "warpgrep":
        return len(out)
    try:
        return json.loads(out.decode(errors="replace")).get("content_bytes", 0)
    except json.JSONDecodeError:
        return 0


def meaning_files(raw, repo):
    txt = raw.decode(errors="replace")
    hits = re.findall(r"^\s*\d+\.\s+RRF\s+\S+\s+cosine\s+\S+\s+(\S+)\s*:",
                      txt, re.MULTILINE)
    return dedup(rel(h, repo) for h in hits)


def findcode_files(raw, repo):
    txt = raw.decode(errors="replace")
    hits = re.findall(r"^(\S+?):\d+\s+\(", txt, re.MULTILINE)
    return dedup(rel(h, repo) for h in hits)


def rel(p, repo):
    try:
        return str(Path(p).resolve().relative_to(repo))
    except ValueError:
        return p


# Each arm runs with the corpus as its working directory, so a relative
# WARPGREP_SCRIPT or MORPH_SDK_DIR would resolve inside the corpus; pin both to
# the directory the benchmark was launched from.
WARPGREP_SCRIPT = (str(Path(os.environ["WARPGREP_SCRIPT"]).resolve())
                   if os.environ.get("WARPGREP_SCRIPT") else None)
if os.environ.get("MORPH_SDK_DIR"):
    os.environ["MORPH_SDK_DIR"] = str(Path(os.environ["MORPH_SDK_DIR"]).resolve())


def main():
    repo = Path(sys.argv[1]).resolve()
    cases = json.load(open(sys.argv[2]))
    semble_bin = sys.argv[3]
    rows = []
    for ci, c in enumerate(cases):
        q, truth = c["query"], c["truth_file"]
        arms = [
            ("semble", [semble_bin, "search", q, str(repo), "--top-k", str(TOPK),
                        "--format", "json"], semble_files),
            ("pixel_search_meaning", ["pixel", "search-meaning", q,
                                      "--metrics", "off"], meaning_files),
            ("pixel_find_code", ["pixel", "find-code", q, "--metrics", "off"],
             findcode_files),
        ]
        if WARPGREP_SCRIPT:
            arms.append(("warpgrep", ["node", WARPGREP_SCRIPT, q, str(repo)],
                         warpgrep_files))
        # Rotate which arm goes first. A discarded warm-up does not cancel
        # order effects BETWEEN arms -- page cache, CPU clock and thermal state
        # all carry over from whichever ran before -- so a fixed order would
        # hand the same systematic advantage to the same arm on every query.
        # The order actually used is recorded with the measurements.
        arms = arms[ci % len(arms):] + arms[:ci % len(arms)]
        row = {"truth_file": truth, "symbol": c["symbol"], "query": q,
               "arm_order": [a[0] for a in arms]}
        for tool, cmd, parse in arms:
            times, out, rc, rcs = [], b"", 0, []
            remote = tool == "warpgrep"
            reps = int(os.environ.get("WARPGREP_REPS", "2")) if remote else REPS + 1
            errors = []
            for i in range(reps):
                ms, out, rc = run(cmd, repo)
                rcs.append(rc)
                # A remote arm fails for reasons worth reading (rate limit,
                # timeout); its failure JSON names them, so keep each one.
                if remote and rc != 0:
                    try:
                        errors.append(json.loads(out.decode(errors="replace"))["error"])
                    except (json.JSONDecodeError, KeyError):
                        errors.append(out.decode(errors="replace")[-300:])
                if i or remote:
                    times.append(ms)
            # A non-zero exit is a failed measurement, not a zero score: parsing
            # whatever a crashed run left on stdout would publish a miss the tool
            # never had a chance to answer.
            failed = [r for r in rcs if r != 0]
            files = [] if failed else parse(out, repo)
            rank = files.index(truth) + 1 if truth in files else None
            row.update({
                f"{tool}_failed_reps": len(failed),
                f"{tool}_ms_reps": [round(t, 1) for t in times],
                f"{tool}_rank": rank,
                f"{tool}_r1": int(rank == 1) if rank else 0,
                f"{tool}_r5": int(bool(rank) and rank <= 5),
                f"{tool}_r10": int(bool(rank) and rank <= TOPK),
                # median(), not times[n // 2]: WarpGrep's two reps would
                # otherwise report the slower run as the median.
                f"{tool}_ms_p50": round(median(times), 1),
                f"{tool}_bytes": answer_bytes(tool, out),
                f"{tool}_returned": len(files),
                f"{tool}_rc": rc,
            })
            if remote:
                row["warpgrep_errors"] = errors
            if remote and not failed:
                meta = json.loads(out.decode(errors="replace"))
                row.update({"warpgrep_turns": meta["turns"],
                            "warpgrep_tool_calls": meta["tool_calls"]})
        rows.append(row)
        print(f"  {truth[-46:]:46s} "
              f"semble={str(row['semble_rank']):>4s} "
              f"meaning={str(row['pixel_search_meaning_rank']):>4s} "
              f"findcode={str(row['pixel_find_code_rank']):>4s} "
              f"warpgrep={str(row.get('warpgrep_rank', '-')):>4s}", file=sys.stderr)
    json.dump(rows, sys.stdout, indent=2)


if __name__ == "__main__":
    main()
