#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Throwaway arena --brief-extra: append xerj's top passages to the Pixel brief (no Pixel integration).

hook-wrap.py runs it with CLONE (the arm's working copy) and PROMPT. The clone's HEAD picks the xerj index prefix
(indexed beforehand with `xerj autoindex <copy at that sha> --prefix <p>`). Each hit is located in the clone's own
file and printed as a brief block, `### path:lo-hi — label` then `N| text`. Prints nothing on any failure.
Never logs the prompt (census prompts are private).
"""
import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
XERJ = os.path.join(HERE, "xerj-dl", "xerj-1.0.0-rc.95-aarch64-apple-darwin", "xerj")
PREFIX_BY_SHA = {"3fce3b5b": "xb-ripgrep", "541653c7": "xb-excalidraw", "611c6162": "xb-requests",
                 "85bede7d": "xc-pixel", "fe0a1eab": "xb-pixel", "a419aa73": "xb-express",
                 "d086db85": "xb-flask", "14dcd92f": "xb-fd"}
K, MAX_LINES, MAX_BYTES = 5, 40, 6000


def main():
    clone, prompt = os.environ.get("CLONE", "."), os.environ.get("PROMPT", "").strip()
    if not prompt:
        return
    sha = subprocess.run(["git", "-C", clone, "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
    prefix = PREFIX_BY_SHA.get(sha[:8])
    if not prefix:
        return
    raw = subprocess.run([XERJ, "search", prompt, "--prefix", prefix, "-k", str(K), "--full", "4000", "--json"],
                         capture_output=True, text=True, timeout=4).stdout
    hits = json.loads(raw)["hits"]["hits"]
    out, used = [], 0
    for hit in hits:
        path = (hit.get("_source") or {}).get("ax_path")
        passages = (hit.get("fields") or {}).get("_passage") or []
        if not path or not passages:
            continue
        try:
            text = open(os.path.join(clone, path), encoding="utf-8").read()
        except (OSError, UnicodeDecodeError):
            continue
        passage = passages[0].get("text", "")
        at = text.find(passage.strip()[:200]) if passage.strip() else -1
        if at < 0:
            continue
        lo = text.count("\n", 0, at) + 1
        lines = text.splitlines()
        span = passage.strip().count("\n") + 1
        hi = min(len(lines), lo + min(span, MAX_LINES) - 1)
        block = [f"### {path}:{lo}-{hi} — xerj search hit (score {hit.get('_score', 0):.1f})"]
        block += [f"{n}| {lines[n - 1]}" for n in range(lo, hi + 1)]
        chunk = "\n".join(block) + "\n"
        if used + len(chunk.encode()) > MAX_BYTES:
            break
        out.append(chunk)
        used += len(chunk.encode())
    sys.stdout.write("".join(out))


if __name__ == "__main__":
    try:
        main()
    except Exception:  # a throwaway prototype: any failure leaves the brief as it was
        pass
