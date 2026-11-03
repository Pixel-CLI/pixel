#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Skip instrumentation only when this main SHA already passed nightly coverage."""

import argparse
import json
from pathlib import Path
import re
import subprocess


def needs_coverage(head: str, runs: list[dict]) -> bool:
    if not re.fullmatch(r"[0-9a-f]{40}", head):
        raise ValueError("head must be a full commit SHA")
    return not any(
        run.get("head_sha") == head
        and run.get("head_branch") == "main"
        and run.get("event") == "schedule"
        and run.get("status") == "completed"
        and run.get("conclusion") == "success"
        and run.get("path", "").split("@", 1)[0] == ".github/workflows/coverage.yml"
        for run in runs
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--head", required=True)
    parser.add_argument("--github-output", type=Path, required=True)
    args = parser.parse_args()
    # Latest successful scheduled main run is sufficient: a new SHA runs;
    # failures and cancellations never replace the successful measurement.
    endpoint = (f"repos/{args.repository}/actions/workflows/coverage.yml/runs"
                "?branch=main&event=schedule&status=success&per_page=1")
    data = json.loads(subprocess.check_output(["gh", "api", endpoint], text=True))
    run = needs_coverage(args.head, data["workflow_runs"])
    with args.github_output.open("a") as output:
        output.write(f"run={'true' if run else 'false'}\n")
    print("Coverage required" if run else "main already measured successfully")


if __name__ == "__main__":
    main()
