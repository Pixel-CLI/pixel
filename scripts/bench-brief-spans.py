#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Does the prompt-submit brief contain the lines that answer the question?

``eval/brief-gate/answer_spans.jsonl`` labels, per question of
``prompts.jsonl``, the line ranges of the code that answers it. For each
labelled question this runs ``pixel brief --json`` and reports, over the
briefs that fired,

* span_hit@excerpt: some line of an ``answer`` block lies inside a gold span;
* span_hit@brief: that, or a ``defined:`` range overlaps a gold span;
* pointer: a ``files:`` entry points inside a gold span;
* zero: briefs that end on the ZERO instruction ("Answer from these lines"),
  and how many of them show an excerpt overlapping a gold span (precision);
* read@1 / read@1-3: a ``read:`` (first) or ``read:``/``also:`` (all) range
  overlaps a gold span (``*_in``: contains the whole span);
* answered: ZERO correct, or no ZERO and a read@1-3 range overlapping.

The labels belong to the fixture checkout (the SHA in ``bench-brief-gate.py``),
so run against an indexed checkout of it with a warm daemon::

    python3 scripts/bench-brief-spans.py --pixel target/dev-release/pixel \
        --repo <fixture checkout> --split dev

Tune on ``dev``; measure ``test`` once. Standard library only.
"""

import argparse
import collections
import json
import os
import re
import subprocess
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PROMPTS = ROOT / "eval" / "brief-gate" / "prompts.jsonl"
SPANS = ROOT / "eval" / "brief-gate" / "answer_spans.jsonl"


def parse(brief):
    """``(excerpt lines by path, defined ranges, files pointers)`` of ``brief``."""
    excerpt, defined, pointers = collections.defaultdict(set), [], []
    current = None
    for line in brief.split("\n"):
        header = re.match(r"answer (\S+):(\d+):$", line)
        if header:
            current = header.group(1)
            continue
        numbered = re.match(r"\s+(\d+)\| ", line)
        if numbered and current:
            excerpt[current].add(int(numbered.group(1)))
            continue
        span = re.match(r"defined: \w+ \S+ (\S+):(\d+)-(\d+)", line)
        if span:
            defined.append((span.group(1), int(span.group(2)), int(span.group(3))))
            current = None
            continue
        if line.startswith("files: "):
            for item in line[7:].split("; "):
                site = re.match(r"([^\s:]+):(\d+)", item)
                if site:
                    pointers.append((site.group(1), int(site.group(2))))
            current = None
            continue
        if not line.startswith(" "):
            current = None
    return excerpt, defined, pointers


def read_ranges(brief):
    """``(path, first, last)`` of each ``read:`` / ``also:`` line, in order."""
    found = []
    for line in brief.split("\n"):
        match = re.match(r"(?:read|also): (\S+):(\d+)-(\d+)", line)
        if match:
            found.append((match.group(1), int(match.group(2)), int(match.group(3))))
    return found


def hits(spans, brief):
    excerpt, defined, pointers = parse(brief)
    ranges = read_ranges(brief)
    over = lambda sp, r: sp["path"] == r[0] and r[1] <= sp["end_line"] and r[2] >= sp["start_line"]
    within = lambda sp, r: sp["path"] == r[0] and r[1] <= sp["start_line"] and r[2] >= sp["end_line"]
    inside = lambda sp, path, n: sp["path"] == path and sp["start_line"] <= n <= sp["end_line"]
    in_excerpt = any(inside(sp, path, n) for sp in spans for path, lines in excerpt.items() for n in lines)
    in_defined = any(sp["path"] == path and start <= sp["end_line"] and end >= sp["start_line"]
                     for sp in spans for path, start, end in defined)
    pointed = any(inside(sp, path, n) for sp in spans for path, n in pointers)
    zero = "Answer from these lines" in brief
    read1 = any(over(sp, ranges[0]) for sp in spans) if ranges else False
    read3 = any(over(sp, r) for sp in spans for r in ranges)
    return {"excerpt": in_excerpt, "brief": in_excerpt or in_defined, "pointer": pointed,
            "zero": zero, "zero_ok": zero and in_excerpt, "read1": read1, "read3": read3,
            "read1_in": any(within(sp, ranges[0]) for sp in spans) if ranges else False,
            "read3_in": any(within(sp, r) for sp in spans for r in ranges),
            "has_read": bool(ranges),
            "answered": (zero and in_excerpt) or (not zero and read3)}


def summarize(rows):
    fired = [row for row in rows if row["fired"]]
    count = lambda key: sum(row[key] for row in fired)
    return {"n": len(rows), "fired": len(fired), "span_hit@excerpt": count("excerpt"),
            "span_hit@brief": count("brief"), "pointer": count("pointer"),
            "zero": count("zero"), "zero_ok": count("zero_ok"), "has_read": count("has_read"),
            "read@1": count("read1"), "read@1-3": count("read3"),
            "read@1_in": count("read1_in"), "read@1-3_in": count("read3_in"),
            "answered": count("answered"),
            "bytes_p50": sorted(r["bytes"] for r in fired)[len(fired) // 2] if fired else None}


def run(pixel, repo, text, timeout=60):
    env = dict(os.environ, PIXEL_BRIEF="1", PIXEL_DAEMON_AUTO_START="0", NO_COLOR="1")
    done = subprocess.run([str(pixel), "brief", "--json", "--metrics", "off", text, str(repo)],
                          cwd=repo, capture_output=True, text=True, timeout=timeout, env=env)
    first = done.stdout.split("\n", 1)[0]
    return json.loads(first) if first.startswith("{") else {}


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--pixel")
    parser.add_argument("--repo")
    parser.add_argument("--split", choices=("dev", "test", "all"), default="dev")
    parser.add_argument("--out")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        sys.argv = [sys.argv[0]]
        unittest.main(module=sys.modules[__name__], exit=True)
    if not (args.pixel and args.repo):
        parser.error("--pixel and --repo are required")
    prompts = {r["id"]: r for r in map(json.loads, open(PROMPTS)) if r.get("id")}
    rows = []
    for gold in map(json.loads, open(SPANS)):
        prompt = prompts[gold["id"]]
        if args.split != "all" and prompt["split"] != args.split:
            continue
        got = run(args.pixel, args.repo, prompt["text"])
        brief = got.get("brief") or ""
        row = {"id": gold["id"], "fired": bool(brief), "bytes": len(brief.encode()),
               "tier": got.get("tier"), "directive": "Answer from this evidence" in brief}
        row.update(hits(gold["answer_spans"], brief))
        rows.append(row)
    groups = {"strong": [r for r in rows if r["fired"] and r["tier"] is None],
              "prose": [r for r in rows if r["tier"] in ("high", "low")]}
    result = {"split": args.split, "summary": summarize(rows), "rows": rows,
              "by_signal": {name: summarize(group) for name, group in groups.items()}}
    if args.out:
        Path(args.out).write_text(json.dumps(result, indent=1))
    print(json.dumps(result["summary"]))
    for name, summary in result["by_signal"].items():
        keys = ("n", "has_read", "read@1", "read@1-3", "answered")
        print(f"  {name}: " + " ".join(f"{k}={summary[k]}" for k in keys))


class Tests(unittest.TestCase):
    BRIEF = "\n".join([
        "[PIXEL:BRIEF]", "defined: function f a.rs:10-20 — x", "files: b.rs:5 — t; c.rs:9",
        "answer a.rs:30:", "  30| fn g() {", "  31|     go();", "answer b.rs:2:", "  2| fn h() {",
    ])

    def test_hits_should_tell_excerpt_defined_and_pointer_apart(self):
        span = lambda path, a, b: [{"path": path, "start_line": a, "end_line": b}]
        pick = lambda got: (got["excerpt"], got["brief"], got["pointer"])
        self.assertEqual(pick(hits(span("a.rs", 31, 40), self.BRIEF)), (True, True, False))
        self.assertEqual(pick(hits(span("a.rs", 15, 16), self.BRIEF)), (False, True, False))
        self.assertEqual(pick(hits(span("c.rs", 8, 12), self.BRIEF)), (False, False, True))
        self.assertEqual(pick(hits(span("z.rs", 1, 9), self.BRIEF)), (False, False, False))

    def test_hits_should_score_the_zero_line_and_the_read_ranges(self):
        brief = "answer a.rs:30:\n  30| fn g() {\nread: a.rs:28-40 — g\nalso: b.rs:1-9 — h\nAnswer from these lines; open."
        span = lambda path, a, b: [{"path": path, "start_line": a, "end_line": b}]
        got = hits(span("a.rs", 30, 31), brief)
        self.assertTrue(got["zero"] and got["zero_ok"] and got["read1"] and got["read1_in"] and got["answered"])
        got = hits(span("b.rs", 5, 6), brief)
        self.assertTrue(not got["read1"] and got["read3"] and got["read3_in"] and not got["zero_ok"])
        self.assertEqual(read_ranges(brief), [("a.rs", 28, 40), ("b.rs", 1, 9)])


if __name__ == "__main__":
    main()
