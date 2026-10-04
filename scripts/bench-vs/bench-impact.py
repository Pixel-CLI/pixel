#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Run the impact/blast-radius benchmark: pixel vs GitNexus, same cases.

Both tools are asked the same question (upstream callers of a symbol, depth 3)
and scored on the same ground truth (direct call-site files, see gen-truth.py).

Scoring is at DEPTH 1 on both sides, because that is the layer whose expected
answer is mechanically derivable: the files that literally contain a call.
Deeper layers are transitive and correct-by-construction for both tools, so
scoring them would reward verbosity. All-depth recall is reported separately
so a tool that finds a caller one layer late is not recorded as having missed it.

Latency is end-to-end CLI wall clock -- what an agent actually pays, including
each runtime's process-start floor. That floor is a property of the product,
not of this harness.
"""
import json
import tempfile
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from bench_common import gitnexus_cli, positional  # noqa: E402

REPS = 5
TIMEOUT = 300


def run(cmd, cwd):
    """Capture stdout through a FILE, never a pipe.

    GitNexus' Node CLI truncates its own stdout at exactly 64 KiB when the
    other end is a pipe (process exit races the flush); pixel does not. Scoring
    a tool on bytes its runtime dropped would measure this harness, not the
    product, so both arms are redirected to a file and read back.
    """
    with tempfile.NamedTemporaryFile(delete=False) as fh:
        tmp = fh.name
    try:
        with open(tmp, "wb") as out:
            t0 = time.perf_counter()
            p = subprocess.run(cmd, cwd=cwd, stdout=out,
                               stderr=subprocess.DEVNULL, timeout=TIMEOUT)
            ms = (time.perf_counter() - t0) * 1000
        return ms, Path(tmp).read_bytes(), p.returncode
    finally:
        Path(tmp).unlink(missing_ok=True)


def pixel_files(raw):
    d = json.loads(raw)
    d1 = {e["path"] for e in d.get("d1_will_break", [])}
    alld = d1 | {e["path"] for e in d.get("d2_likely_affected", [])} \
              | {e["path"] for e in d.get("d3_may_need_tests", [])}
    return d1, alld


def gitnexus_files(raw):
    d = json.loads(raw)
    by = d.get("byDepth", {})
    d1 = {e["filePath"] for e in by.get("1", []) if e.get("filePath")}
    alld = set(d1)
    for k in ("2", "3"):
        alld |= {e["filePath"] for e in by.get(k, []) if e.get("filePath")}
    return d1, alld


def score(reported, truth):
    if not reported:
        return 0.0, 0.0
    hit = len(reported & truth)
    return hit / len(truth), hit / len(reported)


def main():
    args = positional()
    GN_CLI = gitnexus_cli()
    repo = Path(args[0]).resolve()
    cases = json.load(open(args[1]))
    rows = []
    for c in cases:
        sym = c["symbol"]
        truth = set(c["truth_files"])
        row = {"symbol": sym, "lang": c["lang"], "repo": repo.name,
               "truth_size": len(truth)}
        for tool, cmd, parse in (
            ("pixel", ["pixel", "impact", sym, "--metrics", "off"], pixel_files),
            ("gitnexus", GN_CLI + ["impact", sym], gitnexus_files),
        ):
            times, out, rc = [], b"", 0
            try:
                for i in range(REPS + 1):
                    ms, out, rc = run(cmd, repo)
                    if i:                      # drop rep 0: cache warm-up
                        times.append(ms)
                d1, alld = parse(out) if rc == 0 else (set(), set())
            except Exception as e:             # noqa: BLE001 - recorded, not hidden
                row[f"{tool}_error"] = f"{type(e).__name__}: {e}"
                d1, alld = set(), set()
            r1, p1 = score(d1, truth)
            ra, _ = score(alld, truth)
            times.sort()
            row.update({
                f"{tool}_ms_p50": round(times[len(times) // 2], 1) if times else None,
                f"{tool}_ms_min": round(times[0], 1) if times else None,
                f"{tool}_bytes": len(out),
                f"{tool}_recall_d1": round(r1, 3),
                f"{tool}_precision_d1": round(p1, 3),
                f"{tool}_recall_all": round(ra, 3),
                f"{tool}_d1_count": len(d1),
                f"{tool}_missed": sorted(truth - alld),
            })
        rows.append(row)
        print(f"  {sym:28s} pixel r={row['pixel_recall_d1']:.2f} "
              f"{row['pixel_ms_p50']}ms {row['pixel_bytes']}B | "
              f"gitnexus r={row['gitnexus_recall_d1']:.2f} "
              f"{row['gitnexus_ms_p50']}ms {row['gitnexus_bytes']}B", file=sys.stderr)
    json.dump(rows, sys.stdout, indent=2)


if __name__ == "__main__":
    main()
