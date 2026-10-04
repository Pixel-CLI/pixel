# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Print the repository roots with a .pixel/actions.jsonl among the sessions of a window.

Usage: python3 roots.py <window>   (12h, 7d, 3w, or an ISO date)

`pixel recall sessions` caps --limit at 200, has no offset, filters
`ts_last >= since AND ts_first <= until` and orders by ts_last, so no page
cursor is exact. The window is cut into slices instead: a slice that returns
fewer than 200 sessions is complete, a full one is split in two. A session
spanning several slices is counted once (by id). A full slice whose sessions
all span it (halving cannot shrink it), or a spent query budget, ends the run with exit 1 and an
`incomplete:` line, after printing the roots found so far.
"""
import datetime, json, os, re, subprocess, sys

CAP = 200
UTC = datetime.timezone.utc


def parse_window(arg, now):
    m = re.fullmatch(r"(\d+)([hdw])", arg)
    if m:
        hours = {"h": 1, "d": 24, "w": 168}[m.group(2)] * int(m.group(1))
        return now - datetime.timedelta(hours=hours)
    start = datetime.datetime.fromisoformat(arg)
    return start if start.tzinfo else start.replace(tzinfo=UTC)


def sessions(since, until):
    cmd = ["pixel", "recall", "sessions", "--since", since.isoformat(),
           "--until", until.isoformat(), "--limit", str(CAP), "--subagents",
           "--json", "--metrics", "off"]
    out = subprocess.run(cmd, capture_output=True, text=True, check=True).stdout
    return json.loads(out)["sessions"]


BUDGET = 400  # recall queries; a real 30-day window needed 5


def collect(since, until, seen, incomplete, budget):
    if budget[0] <= 0:
        incomplete.append(since)
        return
    budget[0] -= 1
    page = sessions(since, until)
    seen.update((s["id"], s["cwd"]) for s in page)
    if len(page) < CAP:
        return
    # When every returned session spans the whole slice, both halves return
    # the same 200 again: splitting cannot make progress, so flag and stop.
    lo, hi = since.timestamp() * 1000, until.timestamp() * 1000
    spans_all = all(s["ts_first"] <= lo and s["ts_last"] >= hi for s in page)
    if spans_all or until - since <= datetime.timedelta(seconds=1):
        incomplete.append(since)
        return
    mid = since + (until - since) / 2
    collect(since, mid, seen, incomplete, budget)
    collect(mid, until, seen, incomplete, budget)


now = datetime.datetime.now(UTC)
seen, incomplete = set(), []
budget = [BUDGET]
collect(parse_window(sys.argv[1], now), now, seen, incomplete, budget)
roots = set()
for d in {cwd for _, cwd in seen}:
    r = subprocess.run(["git", "-C", d, "rev-parse", "--show-toplevel"],
                       capture_output=True, text=True)
    top = r.stdout.strip()
    if r.returncode == 0 and os.path.isfile(os.path.join(top, ".pixel", "actions.jsonl")):
        roots.add(top)
print(f"{len(seen)} sessions, {len({c for _, c in seen})} cwds", file=sys.stderr)
print(f"{BUDGET - budget[0]} recall queries", file=sys.stderr)
print("\n".join(sorted(roots)))
if incomplete:
    first = min(incomplete).isoformat()
    sys.exit(f"incomplete: {len(incomplete)} slice(s) still hold {CAP} sessions "
             f"(first at {first}); roots above may miss some repositories")
