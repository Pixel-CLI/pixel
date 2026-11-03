#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Sweep the public bench questions through `pixel brief` and print a ✅/❌ table.

For each question in eval/brief-bench/questions.jsonl, runs
`pixel brief "<prompt>" --json` inside the pinned clone (already prepared by
brief-bench.py prepare under ~/.cache/pixel-brief-bench/clones). Verdicts:

    ✅  a non-empty brief was produced (bytes > 0 — what the hook would inject)
    ❌  nothing rendered: gate closed/denied, unjudged with no answer, or
        an open gate whose retrieval answered nothing (fit() -> None)

Each row prints the decision fields (gate, kind, score, bytes, ms, reason) so
the ❌ cases can be studied. Summary + non-zero exit on any ❌.

What a ✅ does not say:

- that the brief is right: a history probe naming the wrong commit is a ✅
  (#1024); read the briefs of the rows you cite;
- what the hook does: `pixel brief` gets the command window (750 ms, at most
  1000 ms, Pi's limit), the prompt-submit hook 3000 ms, so an `unjudged` or
  empty `kind:` row here can be a brief the hook would have finished;
- anything on a loaded machine: the load average is printed beside the
  summary; the same binary went from 55/56 to 22/56 at a load of 383.

    brief-gate-sweep.py [--bin pixel] [--jobs 1] [--only id ...]
"""

import argparse
import concurrent.futures
import json
import os
import subprocess
import sys
from pathlib import Path

WORKTREE = Path(__file__).resolve().parents[2]
QUESTIONS = WORKTREE / "eval" / "brief-bench" / "questions.jsonl"
CACHE = Path(os.environ.get("PIXEL_BENCH_CACHE") or Path.home() / ".cache" / "pixel-brief-bench")


def clone_dir(q):
    return CACHE / "clones" / f"{q['repo'].replace('/', '__')}-{q['sha'][:8]}"


def run_one(q, pixel_bin):
    clone = clone_dir(q)
    if not clone.is_dir():
        return q, None, "no clone"
    try:
        out = subprocess.run(
            [pixel_bin, "brief", q["prompt"], "--json", str(clone)],
            capture_output=True, text=True, timeout=120,
        )
    except subprocess.TimeoutExpired:
        return q, None, "timeout"
    line = next((ln for ln in out.stdout.splitlines() if ln.strip().startswith("{")), None)
    if line is None:
        return q, None, f"no json (rc={out.returncode}): {out.stderr.strip()[:80]}"
    try:
        return q, json.loads(line), None
    except json.JSONDecodeError:
        return q, None, f"bad json: {line[:80]}"


def verdict(d):
    # fit() silences closed/denied gates and empty answers; bytes > 0 is
    # exactly "the hook got a brief to inject". An `unjudged` admission is
    # not silenced, so it still counts when it rendered.
    return (d.get("bytes") or 0) > 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default="pixel")
    # Sequential by default: parallel briefs starve the per-clone daemon and
    # turn answers into unjudged/0, which is not what a real session does.
    ap.add_argument("--jobs", type=int, default=1)
    ap.add_argument("--only", nargs="*", default=None)
    args = ap.parse_args()

    questions = [json.loads(l) for l in QUESTIONS.read_text().splitlines() if l.strip()]
    if args.only:
        questions = [q for q in questions if q["id"] in set(args.only)]

    results = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as ex:
        for q, d, err in ex.map(lambda q: run_one(q, args.bin), questions):
            results.append((q, d, err))

    fails = 0
    print(f"{'':2} {'id':16} {'kind':13} {'gate':8} {'score':>6} {'bytes':>6} {'ms':>6}  detail")
    for q, d, err in results:
        if err is not None:
            fails += 1
            print(f"{'❌':2} {q['id']:16} {q['kind']:13} {'—':8} {'—':>6} {'—':>6} {'—':>6}  {err}")
            continue
        ok = verdict(d)
        fails += 0 if ok else 1
        detail = ""
        if not ok:
            detail = (f"reason={d.get('reason')} signal={d.get('signal')} "
                      f"answered={d.get('answered')} enforced={d.get('enforced')}")
        num = lambda k: "—" if d.get(k) is None else (f"{d[k]:6.2f}" if k == "score" else f"{d[k]:6}")
        print(f"{'✅' if ok else '❌':2} {q['id']:16} {q['kind']:13} {d.get('gate') or '—':8} "
              f"{num('score')} {num('bytes')} {num('elapsed_ms')}  {detail}")

    load = " ".join(f"{x:.0f}" for x in os.getloadavg())
    print(f"\n{len(results) - fails}/{len(results)} briefs produced, {fails} ❌ "
          f"(load average {load} on {os.cpu_count()} cores)")
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()
