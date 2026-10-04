#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""List the uncovered branches of an lcov report, file by file.

The Coverage workflow's `branches` job uploads `coverage-branch.lcov`
(artifact `coverage-branch-lcov`). Each `BRDA:<line>,<block>,<branch>,<taken>`
record is one branch; `taken` is `-` when the line never ran and `0` when it
ran but the branch was never taken. Both count as uncovered.

    gh run download <run> -n coverage-branch-lcov
    python3 scripts/coverage-uncovered-branches.py coverage-branch.lcov \\
        --prefix crates/pixel-daemon/ --top 10 --lines

Without `--lines` it prints one row per file: uncovered, total, percent
covered, path (most uncovered first). With `--lines` each row is followed by
the lines holding an uncovered branch and how many it holds there. Paths are
shown relative to the first `crates/` component, so a prefix written as in
the repository matches the absolute paths a CI runner records.
"""
import argparse
import sys
from collections import defaultdict


def repo_path(path):
    """`/home/runner/work/pixel/pixel/crates/x/src/a.rs` -> `crates/x/src/a.rs`."""
    idx = path.find("/crates/")
    if idx >= 0:
        return path[idx + 1 :]
    return path


def parse(lines):
    """{file: {line: [total, uncovered]}} from lcov text lines."""
    files = defaultdict(lambda: defaultdict(lambda: [0, 0]))
    current = None
    for raw in lines:
        line = raw.strip()
        if line.startswith("SF:"):
            current = repo_path(line[3:])
        elif line == "end_of_record":
            current = None
        elif line.startswith("BRDA:") and current is not None:
            fields = line[5:].split(",")
            if len(fields) != 4:
                continue
            lineno, _block, _branch, taken = fields
            counts = files[current][int(lineno)]
            counts[0] += 1
            if taken == "-" or taken == "0":
                counts[1] += 1
    return files


def rows(files, prefix):
    out = []
    for path, by_line in files.items():
        if prefix and not path.startswith(prefix):
            continue
        total = sum(t for t, _ in by_line.values())
        uncovered = sum(u for _, u in by_line.values())
        if total:
            out.append((uncovered, total, path, by_line))
    out.sort(key=lambda r: (-r[0], r[2]))
    return out


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("lcov", help="lcov file, or - for stdin")
    ap.add_argument("--prefix", default="", help="keep files under this repository path")
    ap.add_argument("--top", type=int, default=0, help="keep the N files with most uncovered branches")
    ap.add_argument("--lines", action="store_true", help="list the lines holding uncovered branches")
    args = ap.parse_args(argv)

    if args.lcov == "-":
        files = parse(sys.stdin)
    else:
        with open(args.lcov, encoding="utf-8") as fh:
            files = parse(fh)

    selected = rows(files, args.prefix)
    if args.top > 0:
        selected = selected[: args.top]
    all_total = sum(r[1] for r in selected)
    all_uncovered = sum(r[0] for r in selected)
    for uncovered, total, path, by_line in selected:
        pct = 100 * (total - uncovered) / total
        print(f"{uncovered:5} / {total:5}  {pct:6.2f}%  {path}")
        if args.lines:
            for lineno in sorted(by_line):
                miss = by_line[lineno][1]
                if miss:
                    print(f"        {path}:{lineno}  {miss} uncovered")
    if all_total:
        pct = 100 * (all_total - all_uncovered) / all_total
        print(f"{all_uncovered:5} / {all_total:5}  {pct:6.2f}%  (selected files)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
