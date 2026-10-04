#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Decide whether a `Mutants` run actually gated anything, and say so.

`cargo mutants --in-diff` exits 0 both when every mutant was caught and when
it produced no mutants at all. The second case is not a pass: it is the gate
having nothing to say. PR #203 hit it -- its whole diff lived in a file
`.cargo/mutants.toml` excluded, so the job went green in 56 s, tested
nothing, and uploaded no `mutants.out`. A reviewer reading the check saw a
mutation gate pass over code that was never mutated.

This script runs after the mutants step and reports what was tested. A diff
that touches no mutable Rust has nothing to answer for -- docs, benches, a
crate-root build script, `Cargo.toml` alone. A diff that touches Rust the
config does NOT exclude, and still produced zero mutants, is the vacuous
case: it gets a warning annotation and an unmissable summary block.

The workflow shards the run, so the script also plays the two roles around
the shards. Before them, `--github-output` sizes the matrix from the listing.
After them, `--outcomes-root` totals every shard's `outcomes.json` and holds
the total against the listing. A shard that crashed, was cancelled or ran a
different list leaves mutants without a verdict. Without that check, the
shards that did finish would add up to a pass.

Usage:
    mutants-gate.py --diff pr.diff --list mutants-list.txt [--fail-on-vacuous]
        [--github-output FILE] [--outcomes-root DIR]

`--list` is the stdout of `cargo mutants --list --in-diff <diff>`, which
costs no build. Exit 0 unless `--fail-on-vacuous` is passed and the diff is
vacuous, or `--outcomes-root` is passed and a listed mutant survived, timed
out, never reached a verdict or was unviable only because the disk was full.
"""

import argparse
from collections import Counter
import json
from pathlib import Path
import re
import sys

REPO = Path(__file__).resolve().parent.parent
CONFIG = REPO / ".cargo/mutants.toml"

#: Mutants one shard is sized for. A shard pays about 40 s of setup and a
#: baseline (35 to 80 s with the shared cache) before its first mutant, then
#: about 25 s per `pixel-cli` mutant and 10 to 15 s per library mutant. The
#: pull request waits for the slowest shard, and runners are free on this
#: public repository, so 10 mutants keep a `pixel-cli` shard near six
#: minutes where 20 take about ten, for one more baseline per 10 mutants.
MUTANTS_PER_SHARD = 10
#: Upper bound on parallel shard jobs. Free accounts run 20 jobs at a time,
#: and this leaves room for the CI workflow of the same push. Past 150
#: mutants, shards grow beyond `MUTANTS_PER_SHARD`: a 658-mutant diff puts
#: 44 on each shard, about 20 minutes against the job's 90-minute limit.
MAX_SHARDS = 15
#: The default `runs-on` value for every shard: GitHub's hosted image. The
#: `PIXEL_MUTANTS_SHARD_RUNNERS` repository variable can replace it with a
#: JSON array of `runs-on` values (self-hosted labels among them); the pool
#: is then assigned round-robin, one member per shard, so a pool of one
#: behaves exactly like the old single-host routing. A pool member that is
#: offline queues its own shards instead of failing them -- the same way a
#: lone busy host always has -- and moving capacity or retiring a host is a
#: variable edit, not a workflow change.
DEFAULT_RUNNER = "ubuntu-26.04"

#: `outcomes.json` summaries, by the name the report prints for them.
OUTCOME_NAMES = {
    "CaughtMutant": "caught",
    "MissedMutant": "missed",
    "Timeout": "timeout",
    "Unviable": "unviable",
}
#: The only outcomes that pass: a test failed under the mutant, or the
#: mutant did not compile. Anything else fails the gate, including a summary
#: this script does not know (a later cargo-mutants may add one).
HELD = ("caught", "unviable")
#: Outcomes that fail the gate: the mutant survived or never finished.
SURVIVING = ("missed", "timeout")
#: What an `Unviable` becomes when its build died of a full disk. The mutant
#: never compiled because the runner ran out of space, not because the
#: mutation broke the code, so nothing was judged. PR #222's run filled the
#: disk at its 101st mutant: 546 of its 566 `unviable` were this, and the
#: gate, which holds `unviable`, called the run `tested`.
DISK_FULL = "disk-full"
#: The build error that marks it, from the linker or any tool writing a
#: temporary file (`ENOSPC`).
DISK_FULL_MARK = "No space left on device"


def exclude_globs(config: Path = CONFIG) -> list[str]:
    """The `exclude_globs` array, read without a TOML dependency."""
    match = re.search(
        r"^exclude_globs\s*=\s*\[(.*?)\]", config.read_text(), re.M | re.S
    )
    return re.findall(r'"([^"]+)"', match.group(1)) if match else []


def to_regex(glob: str) -> re.Pattern[str]:
    """One glob to a regex with globset's semantics: `*` stops at `/`, `**` does not."""
    out = ["^"]
    i = 0
    while i < len(glob):
        if glob.startswith("**/", i):
            out.append("(?:.*/)?")
            i += 3
        elif glob.startswith("**", i):
            out.append(".*")
            i += 2
        elif glob[i] == "*":
            out.append("[^/]*")
            i += 1
        elif glob[i] == "?":
            out.append("[^/]")
            i += 1
        else:
            out.append(re.escape(glob[i]))
            i += 1
    out.append("$")
    return re.compile("".join(out))


def diff_paths(diff_text: str) -> list[str]:
    """Repo-relative paths the diff writes to, deleted files aside."""
    paths = []
    for line in diff_text.splitlines():
        if line.startswith("+++ "):
            target = line[4:].strip()
            if target == "/dev/null":
                continue
            paths.append(target[2:] if target.startswith("b/") else target)
    return sorted(set(paths))


def mutable_rust(paths: list[str], globs: list[str]) -> list[str]:
    """The `.rs` paths a mutant could come from: Rust, minus what is excluded."""
    patterns = [to_regex(g) for g in globs]
    return [
        p for p in paths if p.endswith(".rs") and not any(x.match(p) for x in patterns)
    ]


def count_mutants(list_text: str) -> int:
    """Mutants in `cargo mutants --list` output: one `path.rs:line:col: ...` each."""
    return sum(1 for line in list_text.splitlines() if re.match(r"^\S+\.rs:\d+:\d+:", line))


def shard_count(mutants: int) -> int:
    """Shard jobs for `mutants` listed mutants: none when there is nothing to run."""
    return min(MAX_SHARDS, -(-mutants // MUTANTS_PER_SHARD))


def shard_matrix(mutants: int) -> list[str]:
    """The `--shard k/n` values, ready to pass to cargo-mutants as they are.

    cargo-mutants numbers shards from 0 (`n/n` is rejected), and its default
    `slice` sharding cuts the listing into consecutive ranges, so a shard's
    mutants sit in few packages and its baseline builds only those.
    """
    count = shard_count(mutants)
    return [f"{k}/{count}" for k in range(count)]


def runner_pool(runners: list[str] | None) -> list[str]:
    """The validated shard-runner pool: non-empty strings, or the default.

    An empty or blank entry would ask GitHub for a runner whose labels are
    the empty string and queue forever, so the whole variable is rejected
    rather than partly honored. Every member must be a string: a `null`
    silently dropped here would route its shards somewhere their operator
    never configured, and a number or dict is a mistyped variable, not a
    label. The all-blank fallback stays: an unset variable and an empty
    string mean the same thing.
    """
    members = runners or []
    for member in members:
        if not isinstance(member, str):
            raise ValueError(f"pool member is not a string: {member!r}")
    pool = [r.strip() for r in members if r.strip()]
    return pool or [DEFAULT_RUNNER]


def shard_runners(mutants: int, runners: list[str] | None) -> list[str]:
    """The `runs-on` value per `--shard k/n`, assigned round-robin.

    Consecutive slices of the mutant list are independent jobs, so which
    host takes which shard matters only for load: round-robin spreads a
    matrix evenly and keeps each shard on exactly one host (a shard's
    `target/` is rebuilt per job, and the outcome cache is per-tree, not
    per-host).
    """
    pool = runner_pool(runners)
    return [pool[k % len(pool)] for k in range(shard_count(mutants))]


def shard_include(mutants: int, runners: list[str] | None) -> list[dict[str, str]]:
    """The shard matrix as GitHub reads it: one `{shard, runner}` per job.

    Emitted as the matrix's `include`, so the workflow's `runs-on` reads
    `matrix.runner` instead of zipping two parallel lists through an index
    dimension.
    """
    return [
        {"shard": shard, "runner": runner}
        for shard, runner in zip(shard_matrix(mutants), shard_runners(mutants, runners))
    ]


def tally(root: Path) -> tuple[Counter[str], list[str]]:
    """Outcome counts over every shard's `outcomes.json`, and the survivors' names.

    Each shard's artifact is extracted to `root/<artifact>/`, except when it
    is the only one: `actions/download-artifact` then puts its files in
    `root/` itself, so `root/outcomes.json` is read too.

    A mutant with a summary outside `OUTCOME_NAMES` still counts, under its
    raw summary, so the total always accounts for every scenario the shard
    reported. An `Unviable` whose build log (`log_path`, relative to its
    `outcomes.json`) holds `DISK_FULL_MARK` counts as `DISK_FULL`; one whose
    log is missing stays `unviable`, since nothing says otherwise. Every
    mutant outside `HELD` is named. The baseline is not a mutant and is not
    counted.
    """
    counts: Counter[str] = Counter()
    survivors = []
    paths = sorted(root.glob("*/outcomes.json"))
    if (root / "outcomes.json").is_file():
        paths.insert(0, root / "outcomes.json")
    for path in paths:
        for outcome in json.loads(path.read_text())["outcomes"]:
            scenario = outcome["scenario"]
            if not isinstance(scenario, dict) or "Mutant" not in scenario:
                continue
            name = OUTCOME_NAMES.get(outcome["summary"], outcome["summary"])
            if name == "unviable" and disk_full(path.parent, outcome.get("log_path")):
                name = DISK_FULL
            counts[name] += 1
            if name not in HELD:
                survivors.append(f"{name.upper()} {scenario['Mutant']['name']}")
    return counts, survivors


def disk_full(shard: Path, log_path: str | None) -> bool:
    """Whether the build log of a mutant says the runner's disk was full."""
    if not log_path:
        return False
    log = shard / log_path
    return log.is_file() and DISK_FULL_MARK in log.read_text(errors="replace")


def outcome_failure(listed: int, counts: Counter[str]) -> str | None:
    """Why the shards' outcomes fail the gate, or `None` when every mutant was held."""
    reached = sum(counts.values())
    if reached != listed:
        return (
            f"{listed} mutant(s) listed, {reached} reached a verdict: a shard "
            "crashed, timed out, was cancelled or ran a different list. The "
            "missing mutants were never judged; re-run the failed shard jobs."
        )
    if counts[DISK_FULL]:
        return (
            f"{counts[DISK_FULL]} mutant(s) reported unviable because the "
            f"runner's disk filled up ('{DISK_FULL_MARK}' in their build log): "
            "they never compiled, so they were never judged. Free space on "
            "the shard runner and re-run the shard jobs."
        )
    surviving = sum(counts[name] for name in SURVIVING)
    if surviving:
        return f"{surviving} mutant(s) survived; each one needs a test or a reasoned skip."
    unknown = sorted(name for name in counts if name not in HELD)
    if unknown:
        return (
            f"{sum(counts[name] for name in unknown)} mutant(s) ended with an "
            f"outcome the gate does not know ({', '.join(unknown)}); it cannot "
            "read them as caught."
        )
    return None


def verdict(mutants: int, mutable: list[str]) -> tuple[str, str]:
    """`(status, message)` for a run that produced `mutants` over `mutable` files.

    - `tested`: mutants were generated, so the job's pass or fail means something.
    - `not-applicable`: the diff holds no mutable Rust; nothing was owed.
    - `vacuous`: mutable Rust changed and no mutant came out of it. The gate
      reports nothing about this diff and must not be read as coverage.
    """
    if mutants > 0:
        return "tested", f"{mutants} mutant(s) tested from the diff."
    if not mutable:
        return (
            "not-applicable",
            "No mutable Rust in the diff (docs, benches, build scripts or "
            "manifests only), so no mutants were owed.",
        )
    listed = ", ".join(mutable)
    return (
        "vacuous",
        "0 mutants tested, yet the diff changes Rust the mutation config does "
        f"not exclude: {listed}. This check proves nothing about that code -- "
        "do not read it as coverage. Either the change generates no mutants "
        "(a comment- or test-only edit) or an exclusion in .cargo/mutants.toml "
        "is swallowing the file.",
    )


def render(status: str, message: str, mutants: int, mutable: list[str]) -> str:
    """The job-summary block. The tested count is always stated outright."""
    lines = [
        "### Mutation gate outcome",
        "",
        f"- **mutants tested:** {mutants}",
        f"- **mutable Rust files in the diff:** {len(mutable)}",
        f"- **verdict:** `{status}`",
        "",
        message,
    ]
    if status == "vacuous":
        lines[0] = "### :warning: Mutation gate tested nothing"
    return "\n".join(lines) + "\n"


def render_outcomes(
    counts: Counter[str], survivors: list[str], failure: str | None
) -> str:
    """The shards' totals, every survivor by name, and the gate's decision."""
    named = [*OUTCOME_NAMES.values(), DISK_FULL]
    known = [f"{counts[name]} {name}" for name in named]
    other = [f"{n} {name}" for name, n in sorted(counts.items()) if name not in named]
    lines = [
        "",
        f"- **outcomes across shards:** {sum(counts.values())} judged: "
        + ", ".join(known + other),
        f"- **gate:** {'failed' if failure else 'passed'}",
    ]
    if failure:
        lines += ["", failure]
    if survivors:
        lines += ["", "```", *survivors, "```"]
    return "\n".join(lines) + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--diff", required=True, type=Path)
    parser.add_argument("--list", dest="listing", required=True, type=Path)
    parser.add_argument("--config", type=Path, default=CONFIG)
    parser.add_argument(
        "--fail-on-vacuous",
        action="store_true",
        help="exit 1 when the diff changed mutable Rust and no mutant came out",
    )
    parser.add_argument("--summary", type=Path, help="append the report here")
    parser.add_argument(
        "--github-output",
        type=Path,
        help="append `mutants=<count>` and `shards=<JSON list of k/n>` here",
    )
    parser.add_argument(
        "--runners",
        type=Path,
        help="file holding the `PIXEL_MUTANTS_SHARD_RUNNERS` JSON array; "
        "absent or empty means the GitHub-hosted default",
    )
    parser.add_argument(
        "--outcomes-root",
        type=Path,
        help="directory holding one mutants.out per shard; fail unless every "
        "listed mutant was caught or unviable",
    )
    args = parser.parse_args(argv)

    paths = diff_paths(args.diff.read_text(errors="replace"))
    mutable = mutable_rust(paths, exclude_globs(args.config))
    mutants = count_mutants(args.listing.read_text(errors="replace"))
    status, message = verdict(mutants, mutable)

    if args.github_output:
        runners = None
        if args.runners and args.runners.is_file():
            text = args.runners.read_text(errors="replace").strip()
            if text:
                try:
                    parsed = json.loads(text)
                except json.JSONDecodeError:
                    parsed = None
                if not isinstance(parsed, list):
                    print(
                        "::error title=Bad PIXEL_MUTANTS_SHARD_RUNNERS::"
                        f"expected a JSON array of runs-on values, got {text!r}"
                    )
                    return 1
                try:
                    runners = runner_pool(parsed)
                except ValueError as bad_pool:
                    print(
                        "::error title=Bad PIXEL_MUTANTS_SHARD_RUNNERS::"
                        f"expected a JSON array of runs-on values: {bad_pool}"
                    )
                    return 1
        with args.github_output.open("a") as fh:
            fh.write(f"mutants={mutants}\n")
            fh.write(f"shards={json.dumps(shard_matrix(mutants))}\n")
            fh.write(f"matrix={json.dumps({'include': shard_include(mutants, runners)})}\n")

    report = render(status, message, mutants, mutable)
    failure = None
    if args.outcomes_root:
        counts, survivors = tally(args.outcomes_root)
        failure = outcome_failure(mutants, counts)
        report += render_outcomes(counts, survivors, failure)
    print(report, end="")
    if args.summary:
        with args.summary.open("a") as fh:
            fh.write(report)
    if status == "vacuous":
        # A workflow command, so the verdict lands on the pull request itself
        # and not only in a summary nobody opens.
        print(f"::warning title=Mutation gate tested nothing::{message}")
        if args.fail_on_vacuous:
            return 1
    if failure:
        print(f"::error title=Mutation gate failed::{failure}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
