#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Measure the answer excerpts of confident prompt-submit briefs (issue #889).

Over the English rows of ``eval/brief-gate/prompts.jsonl`` that carry
``expected_files``, this runs ``pixel brief --json <prompt> <repo>`` and, for
each brief of the high tier, reads which files its ``answer`` blocks were cut
from and whether it still tells the agent to answer from the evidence (the
directive). It reports

* files@1: the first path of the ranked ``files:`` line is an expected file;
* excerpt@1: the first block's file is an expected file;
* excerpt@2: one of the first two blocks' files is;
* directive rate: high briefs that keep the directive;
* directive precision: excerpt@1 among the briefs that keep the directive,
  the share of "answer from this evidence" briefs whose top excerpt is right.

Run it against a daemon that is already warm (``pixel prepare-repo``)::

    python3 scripts/bench-brief-excerpt.py --pixel target/dev-release/pixel \\
        --repo <indexed checkout> --split dev --out dev.json

Tune on ``dev``; measure ``test`` once. Standard library only.
"""

import argparse
import json
import os
import re
import subprocess
import sys
import unittest
from pathlib import Path

DEFAULT_SET = Path(__file__).resolve().parent.parent / "eval" / "brief-gate" / "prompts.jsonl"
DIRECTIVE = "Answer from this evidence"
SITE = re.compile(r"^(\S+?):\d+\b")
HEADER = re.compile(r"^answer (\S+):$")


def block_paths(brief):
    """The file of each ``answer`` block of ``brief``, in order.

    A generic block is headed by its ``path:line``; a kind block (``config``,
    ``flow``, ``tests``, ``callers``) by its label, and then the first
    ``path:line`` of its body names the file."""
    paths, lines = [], brief.split("\n")
    for index, line in enumerate(lines):
        header = HEADER.match(line)
        if not header:
            continue
        label = header.group(1)
        site = SITE.match(label)
        if site:
            paths.append(site.group(1))
            continue
        body = lines[index + 1] if index + 1 < len(lines) else ""
        site = SITE.match(body.strip())
        paths.append(site.group(1) if site else None)
    return paths


def first_file(brief):
    """The first path of the ``files:`` line of ``brief``, or None."""
    for line in brief.split("\n"):
        if line.startswith("files: "):
            match = re.match(r"([^\s:;]+)", line[len("files: "):])
            return match.group(1) if match else None
    return None


def summarize(rows):
    """Metrics over ``rows``: dicts with ``expected``, ``tier``, ``paths`` and
    ``directive``."""
    high = [row for row in rows if row["tier"] == "high"]
    listed = [row for row in high if row.get("first_file")]
    excerpted = [row for row in high if row["paths"]]
    directed = [row for row in excerpted if row["directive"]]
    first = lambda row: row["paths"][0] in row["expected"]
    second = lambda row: any(path in row["expected"] for path in row["paths"][:2])
    div = lambda a, b: round(a / b, 3) if b else None
    return {
        "rows": len(rows),
        "high": len(high),
        "with_excerpt": len(excerpted),
        "files@1": div(sum(r["first_file"] in r["expected"] for r in listed), len(listed)),
        "excerpt@1": div(sum(map(first, excerpted)), len(excerpted)),
        "excerpt@2": div(sum(map(second, excerpted)), len(excerpted)),
        "directive": len(directed),
        "directive_rate": div(len(directed), len(excerpted)),
        "directive_precision": div(sum(map(first, directed)), len(directed)),
        "undirected_excerpt@1": div(
            sum(map(first, [r for r in excerpted if not r["directive"]])),
            len(excerpted) - len(directed),
        ),
    }


def measure(pixel, repo, prompt, timeout):
    env = dict(os.environ, PIXEL_BRIEF="1", PIXEL_DAEMON_AUTO_START="0", NO_COLOR="1")
    done = subprocess.run([str(pixel), "brief", "--json", "--metrics", "off", prompt, str(repo)],
                          cwd=repo, capture_output=True, text=True, timeout=timeout, env=env)
    first = done.stdout.split("\n", 1)[0]
    record = json.loads(first) if first.startswith("{") else {}
    brief = record.get("brief") or ""
    return {
        "tier": record.get("tier"),
        "paths": block_paths(brief),
        "first_file": first_file(brief),
        "directive": DIRECTIVE in brief,
        "kind": record.get("kind"),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--pixel")
    parser.add_argument("--repo")
    parser.add_argument("--set", default=str(DEFAULT_SET))
    parser.add_argument("--split", choices=("dev", "test"), default="dev")
    parser.add_argument("--timeout", type=float, default=15.0)
    parser.add_argument("--out")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        sys.argv = [sys.argv[0]]
        unittest.main(module=sys.modules[__name__], exit=True)
    if not (args.pixel and args.repo):
        parser.error("--pixel and --repo are required")
    rows = [json.loads(line) for line in open(args.set) if line.strip()]
    rows = [r for r in rows if r["lang"] == "en" and r.get("expected_files") and r["split"] == args.split]
    measured = []
    for row in rows:
        got = measure(args.pixel, args.repo, row["text"], args.timeout)
        got.update(id=row["id"], expected=row["expected_files"])
        measured.append(got)
    result = {"split": args.split, "summary": summarize(measured), "rows": measured}
    if args.out:
        Path(args.out).write_text(json.dumps(result, indent=1))
    print(json.dumps(result["summary"], indent=1))


class Tests(unittest.TestCase):
    def test_block_paths_should_read_generic_and_kind_blocks(self):
        brief = "\n".join([
            "[PIXEL:BRIEF]", "answer config:", "  src/a.rs:3 — const X", "answer src/b.rs:10:",
            "  10| fn b()", "answer flow:", "  nothing here",
        ])
        self.assertEqual(block_paths(brief), ["src/a.rs", "src/b.rs", None])

    def test_first_file_should_read_the_head_of_the_files_line(self):
        self.assertEqual(first_file("x\nfiles: a/b.rs:3 — t; c.rs:1"), "a/b.rs")
        self.assertEqual(first_file("files: a.rs b.rs"), "a.rs")
        self.assertIsNone(first_file("kind: lookup"))

    def test_summarize_should_score_the_first_block_and_the_directive(self):
        rows = [
            {"tier": "high", "expected": ["a"], "paths": ["a"], "directive": True},
            {"tier": "high", "expected": ["a"], "paths": ["x", "a"], "directive": True},
            {"tier": "high", "expected": ["a"], "paths": ["x", "a"], "directive": False},
            {"tier": "high", "expected": ["a"], "paths": [], "directive": False},
            {"tier": "low", "expected": ["a"], "paths": [], "directive": False},
        ]
        got = summarize(rows)
        self.assertEqual((got["high"], got["with_excerpt"], got["directive"]), (4, 3, 2))
        self.assertEqual(got["excerpt@1"], round(1 / 3, 3))
        self.assertEqual(got["excerpt@2"], 1.0)
        self.assertEqual(got["directive_precision"], 0.5)
        self.assertEqual(got["undirected_excerpt@1"], 0.0)


if __name__ == "__main__":
    main()
