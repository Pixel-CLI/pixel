#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Summarise daemonbench.py's events.jsonl. usage: summarize.py RUN_DIR"""
import json, statistics, sys, pathlib

rows = [json.loads(l) for l in open(pathlib.Path(sys.argv[1]) / "events.jsonl")]


def q(xs, p):
    xs = sorted(xs)
    if not xs:
        return float("nan")
    # nearest-rank
    import math
    return xs[max(0, math.ceil(p * len(xs)) - 1)]


def fmt(xs):
    return (f"n={len(xs)} p50={q(xs, .5):.0f} p95={q(xs, .95):.0f} max={max(xs):.0f}"
            if xs else "n=0")


for arm in ("v1", "hybrid"):
    print(f"== {arm}")
    for r in rows:
        if r["phase"] == "prepare" and r["arm"] == arm:
            g = r.get("timings", {}) or {}
            print(f"prepare {r['s']} s total_ms={g.get('total_ms')} graph_ms={(g.get('graph') or {}).get('elapsed_ms')}"
                  f" .pixel={r['pixel_kb']/1024:.0f} MB graph={r['graph_kb']/1024:.0f} MB mvp={r['mvp_kb']/1024:.0f} MB"
                  f" edges={(r.get('graph') or {}).get('edges')}")
        if r["phase"] == "end" and r["arm"] == arm:
            print(f"end .pixel={r['pixel_kb']/1024:.0f} MB graph={r['graph_kb']/1024:.0f} MB mvp={r['mvp_kb']/1024:.0f} MB")
    hooks = [r for r in rows if r["phase"] == "hook" and r["arm"] == arm and r["k"] >= 0]
    after = [r for r in hooks if r["k"] % 3 == 0]
    later = [r for r in hooks if r["k"] % 3 != 0]
    print("hook all       ", fmt([r["ms"] for r in hooks]))
    print("hook 1s after edit", fmt([r["ms"] for r in after]))
    print("hook 3s/5s after  ", fmt([r["ms"] for r in later]))
    for label, rs in (("after", after), ("later", later)):
        ops = sum(r.get("ops") or 0 for r in rs)
        ans = sum(r.get("answered") or 0 for r in rs)
        routes = {}
        for r in rs:
            routes[r.get("route")] = routes.get(r.get("route"), 0) + 1
        briefs = sum(1 for r in rs if (r.get("bytes") or 0) > 0)
        print(f"  {label}: ops answered {ans}/{ops}, briefs with content {briefs}/{len(rs)}, out bytes"
              f" {sum(r['out_bytes'] for r in rs)}, routes {routes}, rc!=0 {sum(r['rc'] != 0 for r in rs)}")
    cyc = [r for r in rows if r["phase"] == "cycle" and r["arm"] == arm and "cpu_s" in r]
    if cyc:
        print(f"daemon per cycle: cpu_s median {statistics.median(r['cpu_s'] for r in cyc):.2f}"
              f" max {max(r['cpu_s'] for r in cyc):.2f}; peak cpu% max {max(r['peak_cpu_pct'] for r in cyc):.0f};"
              f" rss max {max(r['rss_max_kb'] for r in cyc)/1024:.0f} MB")
    for mode in ("passive", "active"):
        fr = [r for r in rows if r["phase"] == "fresh" and r["arm"] == arm and r["mode"] == mode]
        ok = [r["s"] for r in fr if r["s"] is not None]
        print(f"fresh {mode}: n={len(fr)} seen={len(ok)} p50={q(ok, .5):.2f} s p95={q(ok, .95):.2f} s"
              f" max={max(ok) if ok else float('nan'):.2f} s over5s={sum(s > 5 for s in ok) + len(fr) - len(ok)}")
        st = [r for r in fr if "cpu_s" in r]
        if st:
            print(f"  daemon during update: cpu_s median {statistics.median(r['cpu_s'] for r in st):.2f}"
                  f" peak cpu% max {max(r['peak_cpu_pct'] for r in st):.0f}; rss max {max(r['rss_max_kb'] for r in st)/1024:.0f} MB")
loads = [r["load1"] for r in rows]
print(f"load1 during run: min {min(loads):.1f} median {statistics.median(loads):.1f} max {max(loads):.1f}")
