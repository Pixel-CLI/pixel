#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Plan and report the nightly slice of the whole-tree mutation run.

The pull-request gate (`mutants.yml`) mutates the lines a diff changes, and
nothing else ever re-checks the rest of the tree. Four things escape it:
code merged before the gate existed, a pull request that only weakens or
deletes tests (its diff yields no mutant, so the gate owes nothing), a newer
cargo-mutants whose new operators the old code never faced, and a
`mutants::skip` or an `unviable` that no longer holds. The whole tree is
13 624 mutants (cargo-mutants 27.1.0, 2026-09-29), about 60 runner-hours:
too much for one night inside the free plan's 20 concurrent jobs, so
`mutants-nightly.yml` runs one slice a night and covers the tree in a week.

The list is cut into `SLICES * SHARDS_PER_SLICE` round-robin shards
(`cargo mutants --shard k/N --sharding round-robin`: mutant `i` runs on
shard `i % N`), so every shard mixes crates instead of holding one crate's
slowest mutants. Night `d` (Monday = 0) runs shards `d * SHARDS_PER_SLICE`
to `(d + 1) * SHARDS_PER_SLICE - 1`.

Usage:
    mutants-nightly.py plan --weekday D --list mutants-list.txt --github-output FILE
    mutants-nightly.py report --slice S --expected E --outcomes-root DIR
        --run-url URL --sha SHA --issue-body CURRENT.md > NEW.md

`report` totals the slice's shards with the pull-request gate's own
`tally` and `outcome_failure` (a crashed shard, a full disk or an unknown
outcome fails the same way), and rewrites the slice's section of the
tracking issue, leaving the other six nights' sections as they were. It
exits 1 when the slice has a survivor or an unjudged mutant, so the run is
red when there is something to fix; the issue is updated either way.
"""

from __future__ import annotations

import argparse
from collections import Counter
import importlib.util
import json
from pathlib import Path
import re
import sys

#: Nights in the rotation: the whole tree is covered once a week.
SLICES = 7
#: Parallel shard jobs one night runs. Ten leaves the other ten of the free
#: plan's 20 concurrent jobs to whatever else runs at night, as MAX_SHARDS
#: does for the pull-request gate.
SHARDS_PER_SLICE = 10
#: Shards in the whole-tree list, the `N` of every `--shard k/N`.
TOTAL_SHARDS = SLICES * SHARDS_PER_SLICE
#: GitHub refuses an issue body longer than this (characters).
ISSUE_BODY_LIMIT = 65_536
#: Room kept for the header and for whatever a human writes outside the
#: slice sections (triage notes, links), which the run never trims.
PROSE_ALLOWANCE = 4_096
#: The most one slice's section may take, so that all seven fit together:
#: the survivors named in it stop there, and the rest are counted and left
#: in the run's artifacts.
SECTION_BUDGET = (ISSUE_BODY_LIMIT - PROSE_ALLOWANCE) // SLICES

GATE_PATH = Path(__file__).with_name("mutants-gate.py")
_spec = importlib.util.spec_from_file_location("mutants_gate", GATE_PATH)
gate = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(gate)


def slice_shards(slice_index: int) -> list[str]:
    """The `--shard` values of one night, as cargo-mutants takes them."""
    if not 0 <= slice_index < SLICES:
        raise ValueError(f"slice {slice_index} outside 0..{SLICES - 1}")
    first = slice_index * SHARDS_PER_SLICE
    return [f"{k}/{TOTAL_SHARDS}" for k in range(first, first + SHARDS_PER_SLICE)]


def expected_mutants(total: int, slice_index: int) -> int:
    """Mutants the slice's shards receive under round-robin sharding."""
    return sum(len(range(k, total, TOTAL_SHARDS))
               for k in range(slice_index * SHARDS_PER_SLICE,
                              (slice_index + 1) * SHARDS_PER_SLICE))


def begin(slice_index: int) -> str:
    return f"<!-- mutants-nightly slice {slice_index} -->"


def end(slice_index: int) -> str:
    return f"<!-- /mutants-nightly slice {slice_index} -->"


def section(slice_index: int, expected: int, counts, survivors: list[str],
            failure: str | None, run_url: str, sha: str) -> str:
    """The issue section of one slice, between its two markers."""
    reached = sum(counts.values())
    tally_line = ", ".join(f"{counts[name]} {name}" for name in sorted(counts)) or "no outcome"
    status = "held" if failure is None else "needs work"
    lines = [
        begin(slice_index),
        f"### Slice {slice_index} ({status})",
        "",
        f"Run {run_url} on `{sha[:12]}`: {expected} mutant(s) assigned, "
        f"{reached} judged ({tally_line}).",
    ]
    if failure:
        lines += ["", f"**{failure}**"]
    if survivors:
        # Name survivors while the section stays within SECTION_BUDGET, with
        # room left for the closing fence, the count of the rest and the end
        # marker; a name too long for what is left is counted, not cut.
        tail_room = 160
        used = len("\n".join(lines)) + len("\n\n```")
        named = []
        for name in survivors:
            if used + 1 + len(name) + tail_room > SECTION_BUDGET:
                break
            named.append(name)
            used += 1 + len(name)
        lines += ["", "```", *named, "```"]
        if len(named) < len(survivors):
            lines.append(f"{len(survivors) - len(named)} more in the run's `mutants-nightly-out-*` artifacts.")
    lines.append(end(slice_index))
    return "\n".join(lines)


HEADER = (
    "Survivors of the nightly whole-tree mutation run "
    "(`.github/workflows/mutants-nightly.yml`, `scripts/mutants-nightly.py`). "
    f"One slice of {SLICES} runs each night, so each section is at most a week old. "
    "The run rewrites its own section and leaves the others alone."
)


SECTION = re.compile(r"<!-- mutants-nightly slice (\d+) -->.*?<!-- /mutants-nightly slice \1 -->", re.S)


def update_body(body: str, slice_index: int, new_section: str) -> str:
    """`body` with the slice's section replaced, or inserted before the first
    section of a later slice (appended when there is none). Everything
    outside the sections -- the header, a human's triage notes -- is kept as
    it was."""
    if HEADER not in body:
        body = HEADER + ("\n\n" + body.strip() if body.strip() else "")
    for match in SECTION.finditer(body):
        index = int(match.group(1))
        if index == slice_index:
            return (body[:match.start()] + new_section + body[match.end():]).rstrip() + "\n"
        if index > slice_index:
            return (body[:match.start()].rstrip() + "\n\n" + new_section + "\n\n"
                    + body[match.start():].lstrip()).rstrip() + "\n"
    return body.rstrip() + "\n\n" + new_section + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    plan = sub.add_parser("plan")
    plan.add_argument("--weekday", type=int, required=True, help="0 = Monday")
    plan.add_argument("--list", dest="listing", type=Path, required=True)
    plan.add_argument("--github-output", type=Path, required=True)
    report = sub.add_parser("report")
    report.add_argument("--slice", type=int, required=True)
    report.add_argument("--expected", type=int, required=True)
    report.add_argument("--outcomes-root", type=Path, required=True)
    report.add_argument("--run-url", required=True)
    report.add_argument("--sha", required=True)
    report.add_argument("--issue-body", type=Path, required=True)
    report.add_argument("--summary", type=Path)
    args = parser.parse_args(argv)

    if args.command == "plan":
        total = gate.count_mutants(args.listing.read_text(errors="replace"))
        slice_index = args.weekday % SLICES
        with args.github_output.open("a") as fh:
            fh.write(f"slice={slice_index}\n")
            fh.write(f"shards={json.dumps(slice_shards(slice_index))}\n")
            fh.write(f"expected={expected_mutants(total, slice_index)}\n")
            fh.write(f"total={total}\n")
        return 0

    counts, survivors = (gate.tally(args.outcomes_root) if args.outcomes_root.is_dir()
                         else (Counter(), []))
    failure = gate.outcome_failure(args.expected, counts)
    new = section(args.slice, args.expected, counts, survivors, failure, args.run_url, args.sha)
    current = args.issue_body.read_text() if args.issue_body.is_file() else ""
    body = update_body(current, args.slice, new)
    if len(body) > ISSUE_BODY_LIMIT:
        # Only prose outside the sections can get here: each section is held
        # to SECTION_BUDGET. Fail before `gh issue edit` would refuse it.
        print(f"tracking issue body would be {len(body)} characters, over GitHub's "
              f"{ISSUE_BODY_LIMIT}: the text outside the slice sections exceeds "
              f"{PROSE_ALLOWANCE}; move it elsewhere", file=sys.stderr)
        return 2
    sys.stdout.write(body)
    if args.summary:
        with args.summary.open("a") as fh:
            fh.write(new + "\n")
    return 0 if failure is None else 1


if __name__ == "__main__":
    sys.exit(main())
