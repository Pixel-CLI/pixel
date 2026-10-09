#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""The daemon's `meaning` op: recall@k and latency against a running daemon.

    bench-meaning-op.py REPO CASES.json [--pixel PIXEL] [--reps N]
                        [--spaced-reps N] [--baseline]

REPO is a repository with a daemon already running (`pixel daemon start`); the
socket is read from `pixel daemon status`. CASES is one of
docs/bench/vs-tools/cases/queries-<lang>.json, the same cases
bench-retrieval.py scores `search-meaning` on: the query, and the file whose
doc comment it was cut from.

What is measured, each over one fresh socket connection per request, as the
prompt-submit brief makes them, timed from connect to the parsed reply:

  warm-up   requests until the op answers `ready`, with the reasons it gave
            before (the cold start a brief would have fallen back through)
  recall    the truth file's rank among the returned files, limit 10
  ping      the socket and envelope floor, no op behind it
  spaced    `meaning` requests 1.05 s apart, so each one finds the daemon's
            one-second snapshot cache empty and pays its three git calls
  burst     `meaning` requests back to back, the snapshot cached after the
            first of each second

--baseline also runs `pixel search-meaning` once per case, for the recall the
resident index has to match and the wall clock of the CLI it replaces.

The report is JSON on stdout; a one-screen summary goes to stderr.
"""
import argparse
import json
import re
import socket
import subprocess
import sys
import time
from pathlib import Path

TOPK = 10
SNAPSHOT_TTL_S = 1.05
WARMUP_DEADLINE_S = 180


def percentile(values, pct):
    """Nearest-rank percentile: the smallest value with pct% of them at or below."""
    ordered = sorted(values)
    rank = max(1, -(-len(ordered) * pct // 100))
    return ordered[int(rank) - 1]


def summary(values):
    return {
        "n": len(values),
        "p50_ms": round(percentile(values, 50), 2),
        "p95_ms": round(percentile(values, 95), 2),
        "p99_ms": round(percentile(values, 99), 2),
        "max_ms": round(max(values), 2),
    }


def socket_of(pixel, repo):
    out = subprocess.run([pixel, "daemon", "status"], cwd=repo,
                         capture_output=True, text=True).stdout
    found = re.search(r"daemon running \((.+\.sock)\)", out)
    if not found:
        sys.exit(f"no daemon running for {repo}: start one with `pixel daemon start`")
    return found.group(1)


def ask(sock_path, payload):
    """One request on its own connection: (milliseconds, parsed reply)."""
    started = time.perf_counter()
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(30)
        sock.connect(sock_path)
        sock.sendall(json.dumps(payload).encode() + b"\n")
        reply = b""
        while not reply.endswith(b"\n"):
            chunk = sock.recv(1 << 16)
            if not chunk:
                break
            reply += chunk
    elapsed = (time.perf_counter() - started) * 1000
    return elapsed, json.loads(reply)


def meaning(query, limit=TOPK):
    return {"op": "meaning", "query": query, "limit": limit}


def warm_up(sock_path, query):
    """Ask until ready; the reasons seen on the way, and the seconds it took."""
    started = time.perf_counter()
    reasons = []
    while time.perf_counter() - started < WARMUP_DEADLINE_S:
        _, reply = ask(sock_path, meaning(query, 1))
        result = reply.get("result") or {}
        if result.get("status") == "ready":
            return {"seconds_to_ready": round(time.perf_counter() - started, 2),
                    "reasons_before_ready": reasons, "pool": result["pool"]}
        reason = result.get("reason") or reply.get("error", {}).get("message", "?")
        if not reasons or reasons[-1] != reason:
            reasons.append(reason)
        time.sleep(0.25)
    sys.exit(f"the op never became ready within {WARMUP_DEADLINE_S}s: {reasons}")


def rank_of(truth, paths):
    return paths.index(truth) + 1 if truth in paths else None


def meaning_files(raw):
    """The files `pixel search-meaning` ranks, best first (as bench-retrieval.py)."""
    hits = re.findall(r"^\s*\d+\.\s+RRF\s+\S+\s+cosine\s+\S+\s+(\S+)\s*:", raw, re.MULTILINE)
    return list(dict.fromkeys(hits))


def recall(ranks):
    count = len(ranks)
    return {f"r{k}": round(sum(1 for r in ranks if r and r <= k) / count, 3)
            for k in (1, 5, 10)}


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("repo")
    parser.add_argument("cases")
    parser.add_argument("--pixel", default="pixel")
    parser.add_argument("--reps", type=int, default=20,
                        help="burst passes over the cases (default 20)")
    parser.add_argument("--spaced-reps", type=int, default=2,
                        help="spaced passes over the cases (default 2)")
    parser.add_argument("--baseline", action="store_true",
                        help="also score `pixel search-meaning` on the cases")
    args = parser.parse_args()

    repo = Path(args.repo).resolve()
    cases = json.load(open(args.cases))
    sock_path = socket_of(args.pixel, repo)
    report = {"repo": str(repo), "cases": len(cases), "socket": sock_path}

    report["warm_up"] = warm_up(sock_path, cases[0]["query"])

    ranks, rows = [], []
    for case in cases:
        _, reply = ask(sock_path, meaning(case["query"]))
        paths = list(dict.fromkeys(h["path"] for h in reply["result"]["hits"]))
        rank = rank_of(case["truth_file"], paths)
        ranks.append(rank)
        rows.append({"truth_file": case["truth_file"], "rank": rank, "returned": len(paths)})
    report["recall"] = recall(ranks)
    report["rows"] = rows

    pings = [ask(sock_path, {"op": "ping"})[0] for _ in range(200)]
    report["ping"] = summary(pings)

    spaced = []
    for _ in range(args.spaced_reps):
        for case in cases:
            time.sleep(SNAPSHOT_TTL_S)
            spaced.append(ask(sock_path, meaning(case["query"]))[0])
    report["spaced"] = summary(spaced)

    burst = []
    for _ in range(args.reps):
        for case in cases:
            burst.append(ask(sock_path, meaning(case["query"]))[0])
    report["burst"] = summary(burst)

    if args.baseline:
        base_ranks, times = [], []
        for case in cases:
            runs = []
            for _ in range(3):
                started = time.perf_counter()
                out = subprocess.run([args.pixel, "search-meaning", case["query"],
                                      "--metrics", "off"], cwd=repo,
                                     capture_output=True, text=True).stdout
                runs.append((time.perf_counter() - started) * 1000)
            times.extend(runs[1:])  # the first run of each case is the warm-up
            base_ranks.append(rank_of(case["truth_file"], meaning_files(out)))
        report["baseline"] = {"recall": recall(base_ranks), "ranks": base_ranks,
                              "cli_wall": summary(times)}

    json.dump(report, sys.stdout, indent=2)
    print(file=sys.stdout)
    print(f"recall {report['recall']}  ping p50 {report['ping']['p50_ms']} ms\n"
          f"spaced p50 {report['spaced']['p50_ms']} p95 {report['spaced']['p95_ms']} ms  "
          f"burst p50 {report['burst']['p50_ms']} p95 {report['burst']['p95_ms']} ms",
          file=sys.stderr)


if __name__ == "__main__":
    main()
