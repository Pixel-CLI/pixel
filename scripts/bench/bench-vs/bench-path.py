#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Call-path finding: `pixel call-path` vs `gitnexus trace`.

Each case is a (caller, callee) pair whose call site was located in the source
and recorded in the case file, so "a path exists" is a fact about the code, not
about either tool's graph. Scored as found / not found, plus latency and output
size. N is small and stated as such: these are hand-verifiable pairs, not a
sample big enough to rank the two graph engines.
"""
import json
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from bench_common import gitnexus_cli, positional  # noqa: E402

REPS = 3


def run(cmd, cwd):
    with tempfile.NamedTemporaryFile(delete=False) as fh:
        tmp = fh.name
    try:
        with open(tmp, "wb") as out:
            t0 = time.perf_counter()
            p = subprocess.run(cmd, cwd=cwd, stdout=out,
                               stderr=subprocess.DEVNULL, timeout=300)
            ms = (time.perf_counter() - t0) * 1000
        return ms, Path(tmp).read_bytes(), p.returncode
    finally:
        Path(tmp).unlink(missing_ok=True)


def pixel_found(raw, rc=0):
    txt = raw.decode(errors="replace")
    if rc != 0 and not txt.strip():
        return "error"   # pixel reports "no symbol named X" on stderr, exit 1
    # pixel prints the "ambiguous name -- re-run with one of these uids" header
    # on stderr and the candidate uids on stdout, so the stdout side is a list
    # of indented `path#symbol#kind` lines, not JSON.
    lines = [l for l in txt.splitlines() if l.strip()]
    if lines and all(l.startswith("  ") and "#" in l for l in lines):
        return "ambiguous"
    try:
        return "yes" if json.loads(txt).get("found") else "no"
    except json.JSONDecodeError:
        return "unparsed"


def gitnexus_found(raw):
    txt = raw.decode(errors="replace")
    start = txt.find("{")
    if start < 0:
        return "unparsed"
    try:
        d = json.loads(txt[start:])
    except json.JSONDecodeError:
        return "unparsed"
    status = d.get("status")
    if status == "ambiguous":
        return "ambiguous"
    # `gitnexus trace` reports a found path as status "ok" with a non-empty
    # `hops` list -- there is no `found` flag. Scoring on a key it never emits
    # would record every hit as a miss.
    if status == "ok" and d.get("hops"):
        return "yes"
    return "no"


def main():
    args = positional()
    GN_CLI = gitnexus_cli()
    repo = Path(args[0]).resolve()
    cases = json.load(open(args[1]))
    rows = []
    for c in cases:
        row = {"from": c["from"], "to": c["to"], "verified_at":
               f"{c['from_file']}:{c['verified_line']}"}
        for tool, cmd, verdict in (
            ("pixel", ["pixel", "call-path", c["from"], c["to"], "--metrics", "off"],
             pixel_found),
            ("gitnexus", GN_CLI + ["trace", c["from"], c["to"]], gitnexus_found),
        ):
            times, out, rc = [], b"", 0
            for i in range(REPS + 1):
                ms, out, rc = run(cmd, repo)
                if i:
                    times.append(ms)
            times.sort()
            row[f"{tool}_verdict"] = (verdict(out, rc) if tool == "pixel"
                                      else verdict(out))
            row[f"{tool}_ms_p50"] = round(times[len(times) // 2], 1)
            row[f"{tool}_bytes"] = len(out)
        rows.append(row)
        print(f"  {c['from']:22s} -> {c['to']:26s} "
              f"pixel={row['pixel_verdict']:9s} {row['pixel_ms_p50']:6.0f}ms "
              f"{row['pixel_bytes']:6d}B | gitnexus={row['gitnexus_verdict']:9s} "
              f"{row['gitnexus_ms_p50']:6.0f}ms {row['gitnexus_bytes']:6d}B",
              file=sys.stderr)
    json.dump(rows, sys.stdout, indent=2)


if __name__ == "__main__":
    main()
