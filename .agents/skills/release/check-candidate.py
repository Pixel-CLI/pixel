#!/usr/bin/env python3
"""Read-only guard between prepare, merge and tag. Run from the repository root.

Usage: check-candidate.py BASE PREPARE_HEAD --tip FETCHED_BASE [--merge MERGE] [--maintenance]
Pass the recorded SHAs, not moving branch names, for BASE and PREPARE_HEAD.
Without --merge, the fetched target must still equal BASE. After squash merge,
MERGE must have BASE as its sole parent and the exact PREPARE_HEAD tree.
Use --maintenance for a fully tested backport PR: its diff may include code.
This does not attest CI, approvals or semantic changelog completeness.
"""

import argparse
from pathlib import Path
import subprocess
import sys


def git(*args):
    return subprocess.check_output(["git", *args], text=True).strip()


def check(base, head, tip, merge, maintenance):
    base, head, tip = (git("rev-parse", "--verify", f"{ref}^{{commit}}")
                       for ref in (base, head, tip))
    subprocess.run(["git", "merge-base", "--is-ancestor", base, head], check=True)
    if base == head:
        raise ValueError("prepare head must contain a release commit")
    scope = Path(__file__).resolve().parents[3] / "scripts/release-prepare-only.py"
    if not maintenance:
        subprocess.run([sys.executable, str(scope), base, head], check=True)
    fragments = git("ls-tree", "-r", "--name-only", head, "changelog.d").splitlines()
    if any(path.endswith(".md") for path in fragments):
        raise ValueError("candidate still contains unreleased changelog fragments")
    if merge is None:
        if tip != base:
            raise ValueError("base moved: refresh candidate and changelog coverage, then revalidate")
    else:
        merge = git("rev-parse", "--verify", f"{merge}^{{commit}}")
        subprocess.run(["git", "merge-base", "--is-ancestor", merge, tip], check=True)
        if git("show", "-s", "--format=%P", merge) != base:
            raise ValueError("merge parent differs from recorded base; candidate is stale")
        if git("rev-parse", f"{head}^{{tree}}") != git("rev-parse", f"{merge}^{{tree}}"):
            raise ValueError("merge tree differs from validated prepare head; do not tag")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base")
    parser.add_argument("head")
    parser.add_argument("--tip", required=True)
    parser.add_argument("--merge")
    parser.add_argument("--maintenance", action="store_true")
    args = parser.parse_args()
    try:
        check(args.base, args.head, args.tip, args.merge, args.maintenance)
    except (ValueError, subprocess.CalledProcessError) as error:
        print(f"release candidate refused: {error}", file=sys.stderr)
        return 1
    print("release candidate: ancestry, scope and content verified")
    return 0


if __name__ == "__main__":
    sys.exit(main())
