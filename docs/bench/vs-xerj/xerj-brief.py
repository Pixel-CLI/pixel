#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Arena --brief-extra that REPLACES Pixel's brief with a xerj brief: the passages `xerj search` ranks highest for
the prompt, nothing from Pixel.

hook-wrap.py runs it with CLONE (the arm's working copy) and PROMPT. The clone's HEAD picks the xerj node that holds
only that repository (default `ax` prefix). Each hit is located in the clone's own file and printed as a brief block,
`### path:lo-hi — label` then `N| text`, under the same channel tag and closing line as Pixel's brief, within
BUDGET bytes (the mean size of Pixel's census briefs, so both arms get the same room). The output always starts with
@@REPLACE@@, even with no hit or on an error, so Pixel's brief never reaches the agent. Never logs the prompt.
"""
import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
XERJ = os.path.join(HERE, "xerj-dl", "xerj-1.0.0-rc.95-aarch64-apple-darwin", "xerj")
PORT_BY_SHA = {"3fce3b5b": 9310, "611c6162": 9320, "541653c7": 9330, "85bede7d": 9340}
K, MAX_LINES, BUDGET = 10, 60, 13_000
HEAD = "[PIXEL:BRIEF]\nxerj search: the passages xerj ranks highest for this prompt. Lines are exact source, numbered.\n"
FOOT = "Answer from this context. Use a tool only if something you need is not shown above.\n"


def blocks(clone, prompt):
    sha = subprocess.run(["git", "-C", clone, "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
    port = PORT_BY_SHA.get(sha[:8])
    if not port or not prompt:
        return []
    raw = subprocess.run([XERJ, "search", prompt, "--url", f"http://localhost:{port}", "-k", str(K), "--full", "8000",
                          "--json"], capture_output=True, text=True, timeout=3).stdout
    out, used = [], len(HEAD) + len(FOOT)
    for hit in json.loads(raw)["hits"]["hits"]:
        path = (hit.get("_source") or {}).get("ax_path")
        passages = (hit.get("fields") or {}).get("_passage") or []
        if not path or not passages:
            continue
        try:
            text = open(os.path.join(clone, path), encoding="utf-8").read()
        except (OSError, UnicodeDecodeError):
            continue
        passage = passages[0].get("text", "").strip()
        at = text.find(passage[:200]) if passage else -1
        if at < 0:
            continue
        lines = text.splitlines()
        lo = text.count("\n", 0, at) + 1
        hi = min(len(lines), lo + min(passage.count("\n") + 1, MAX_LINES) - 1)
        chunk = f"### {path}:{lo}-{hi} — xerj search hit (score {hit.get('_score', 0):.1f})\n"
        chunk += "".join(f"{n}| {lines[n - 1]}\n" for n in range(lo, hi + 1))
        if used + len(chunk.encode()) > BUDGET:
            break
        out.append(chunk)
        used += len(chunk.encode())
    return out


def main():
    try:
        found = blocks(os.environ.get("CLONE", "."), os.environ.get("PROMPT", "").strip())
    except Exception:  # a failed search still replaces Pixel's brief
        found = []
    body = "".join(found) if found else "xerj search returned no passage for this prompt.\n"
    sys.stdout.write("@@REPLACE@@\n" + HEAD + body + FOOT)


if __name__ == "__main__":
    main()
