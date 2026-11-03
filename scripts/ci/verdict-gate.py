#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Judge ci.yml's lanes for the `Record the green verdict` job.

The job used to be skipped whenever a lane failed, and GitHub counts a
skipped required check as passing. It now always runs and calls this script
with `toJSON(needs)` in NEEDS_JSON:

- a lane that is `failure` or `cancelled` fails the verdict;
- a lane that is `skipped` is accepted only when the change filter says the
  diff does not apply to it (`changes.outputs.<flag>` is not `true`); a lane
  with no flag (Test + Format, scope, changes) always has to succeed;
- a lane this table does not know fails closed, so a new lane cannot be
  skipped past the verdict until it is mapped here.

Exit 0 only when every lane is judged green. Usage: NEEDS_JSON=... verdict-gate.py
"""

import json
import os
import sys

# lane -> the `changes` output that says the diff applies to it (None: always).
LANES = {
    "scope": None,
    "changes": None,
    "test": None,
    "linux-install": "installer",
    "lint": "rust-source",
    "msrv": "rust-source",
    "ranking": "rust-source",
    "deny": "deps",
}


def judge(needs: dict) -> list[str]:
    """Return one problem per lane that is not green; empty means green."""
    flags = (needs.get("changes") or {}).get("outputs") or {}
    problems = []
    for lane, need in sorted(needs.items()):
        result = need.get("result")
        if result == "success":
            continue
        if lane not in LANES:
            problems.append(f"{lane}: {result}, and no filter flag is mapped for this lane")
        elif result == "skipped" and LANES[lane] is not None and flags.get(LANES[lane]) != "true":
            continue
        elif result == "skipped" and LANES[lane] is not None:
            problems.append(f"{lane}: skipped although the change filter marked {LANES[lane]}=true")
        else:
            problems.append(f"{lane}: {result}")
    return problems


def main() -> int:
    problems = judge(json.loads(os.environ["NEEDS_JSON"]))
    for problem in problems:
        print(f"::error::{problem}")
    print("verdict: " + ("RED, " + str(len(problems)) + " lane(s)" if problems else "green"))
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
