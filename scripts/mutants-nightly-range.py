#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Select main's unjudged diff; checkpoint only complete mutation campaigns."""

import argparse
import importlib.util
import json
from pathlib import Path
import re
import subprocess

CHECKPOINT = "mutants-nightly-checkpoint"
WORKFLOW = ".github/workflows/mutants.yml"
SCRIPT = "scripts/mutants-nightly-range.py"
SHA = re.compile(r"[0-9a-f]{40}\Z")


def git(*args: str) -> str:
    return subprocess.check_output(["git", *args], text=True).strip()


def gh_json(endpoint: str, *, paginate: bool = False):
    command = ["gh", "api", endpoint]
    if paginate:
        command += ["--paginate", "--slurp"]
    return json.loads(subprocess.check_output(command, text=True))


def select_range(repository: str, head: str) -> str:
    """Artifacts are trusted only from this workflow's completed main runs.

    Metadata remains sufficient after artifact expiry: no download or execution
    of artifact content is needed. If metadata was deleted, replay from rollout
    rather than silently losing coverage. API failures stop the run.
    """
    if not SHA.fullmatch(head):
        raise ValueError("head must be a full commit SHA")
    pages = gh_json(
        f"repos/{repository}/actions/artifacts?name={CHECKPOINT}&per_page=100",
        paginate=True,
    )
    artifacts = sorted(
        (a for page in pages for a in page["artifacts"]),
        key=lambda a: a["id"], reverse=True,
    )
    base = None
    for artifact in artifacts:
        source = artifact.get("workflow_run", {})
        candidate = source.get("head_sha", "")
        if (artifact["name"] != CHECKPOINT or source.get("head_branch") != "main"
                or not SHA.fullmatch(candidate)):
            continue
        run = gh_json(f"repos/{repository}/actions/runs/{source['id']}")
        if (run.get("path", "").split("@", 1)[0] != WORKFLOW or run.get("head_branch") != "main"
                or run.get("head_sha") != candidate
                or run.get("event") != "schedule"
                or run.get("status") != "completed"
                or run.get("conclusion") not in {"success", "failure"}):
            continue
        ancestor = subprocess.run(["git", "merge-base", "--is-ancestor", candidate, head])
        if ancestor.returncode != 0:
            raise ValueError("checkpoint is not in main history; refusing to discard coverage")
        base = candidate
        break
    if base is None:
        introductions = git("log", head, "--diff-filter=A", "--format=%H", "--", SCRIPT).splitlines()
        if not introductions:
            raise ValueError("cannot find nightly rollout; refusing an arbitrary starting point")
        base = git("rev-parse", f"{introductions[-1]}^")
    return "" if base == head else f"{base}...{head}"


def fully_judged(listed: int, counts: dict[str, int]) -> bool:
    """A red verdict with survivors is complete; missing/invalid results are not."""
    return sum(counts.values()) == listed and set(counts) <= {
        "caught", "unviable", "missed", "timeout",
    }


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    plan = commands.add_parser("plan")
    plan.add_argument("--repository", required=True)
    plan.add_argument("--head", required=True)
    plan.add_argument("--github-output", type=Path, required=True)
    checkpoint = commands.add_parser("checkpoint")
    checkpoint.add_argument("--list", type=Path, required=True)
    checkpoint.add_argument("--outcomes-root", type=Path, required=True)
    checkpoint.add_argument("--sha", required=True)
    checkpoint.add_argument("--output", type=Path, required=True)
    args = parser.parse_args(argv)
    if args.command == "plan":
        diff = select_range(args.repository, args.head)
        with args.github_output.open("a") as output:
            output.write(f"diff_range={diff}\nrun={'true' if diff else 'false'}\n")
        print(f"Nightly diff: {diff}" if diff else "main unchanged since last completed campaign")
        return 0
    if not SHA.fullmatch(args.sha):
        raise ValueError("checkpoint must name a full commit SHA")
    spec = importlib.util.spec_from_file_location("mutants_gate", Path(__file__).with_name("mutants-gate.py"))
    gate = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(gate)
    listed = gate.count_mutants(args.list.read_text())
    counts, _ = gate.tally(args.outcomes_root)
    if not fully_judged(listed, counts):
        print("Campaign incomplete: keeping the previous checkpoint")
        return 1
    args.output.write_text(json.dumps({"sha": args.sha, "listed": listed, "outcomes": counts}) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
