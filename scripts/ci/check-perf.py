#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Compare a `pixel prepare-repo --json` receipt with the checked-in perf ceilings.

The weekly `perf.yml` lane builds the pinned corpus from nothing, then again
after one file change, and passes each receipt here. A metric over its ceiling
prints the number and exits 1; so does a receipt that is shaped like a pass but
measured nothing (a cold run that reused an index, or one with no symbols), so
the lane fails on a real regression and never on a vacuous one.

Usage: check-perf.py --label cold|incremental --timings receipt.json
                     [--baseline .github/perf-baseline.json]
                     [--multiplier N] [--summary "$GITHUB_STEP_SUMMARY"]
       check-perf.py --self-test
"""
from pathlib import Path
import contextlib
import io
import json
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[2]
DEFAULT_BASELINE = ROOT / ".github" / "perf-baseline.json"

# name -> (label, dotted path into the receipt). The ceilings only cover the
# phases a regression would move; the shape checks below cover the rest.
METRICS = {
    "cold": {
        "total_ms": "timings.total_ms",
        "index_base_ms": "timings.index.base_ms",
        "graph_elapsed_ms": "timings.graph.elapsed_ms",
    },
    "incremental": {
        "total_ms": "timings.total_ms",
        "graph_elapsed_ms": "timings.graph.elapsed_ms",
    },
}


def dig(doc, path):
    for part in path.split("."):
        if not isinstance(doc, dict) or part not in doc:
            return None
        doc = doc[part]
    return doc


def evaluate(label, receipt, baseline, multiplier):
    """(problems, rows): rows are (metric, measured, ceiling) for the summary."""
    problems = []
    rows = []
    plan = METRICS[label]
    limits = baseline[label]
    for name, path in plan.items():
        measured = dig(receipt, path)
        ceiling = limits[name] * multiplier
        if not isinstance(measured, (int, float)):
            problems.append(f"{label}.{name}: receipt has no {path} (got {measured!r})")
            continue
        rows.append((name, measured, ceiling))
        if measured > ceiling:
            problems.append(
                f"{label}.{name}: {measured} over the {ceiling} ceiling "
                f"(baseline {limits[name]} x{multiplier})"
            )
    for key, receipt_key in (("min_files", "files"), ("min_symbols", "symbols")):
        measured, floor = dig(receipt, f"graph.{receipt_key}"), limits[key]
        if not isinstance(measured, int) or measured < floor:
            problems.append(
                f"{label}: graph.{receipt_key} is {measured!r}, under the floor {floor}"
            )
    if label == "cold":
        if dig(receipt, "timings.index.base") != "built_from_git":
            problems.append(
                f"cold: index built from {dig(receipt, 'timings.index.base')!r}, "
                "not `built_from_git` (a reused index is not a cold run)"
            )
        if dig(receipt, "timings.graph.build.mode") != "full":
            problems.append(
                f"cold: graph mode is {dig(receipt, 'timings.graph.build.mode')!r}, not `full`"
            )
    elif dig(receipt, "timings.graph.build.mode") != "incremental":
        problems.append(
            f"incremental: graph mode is {dig(receipt, 'timings.graph.build.mode')!r}, "
            "not `incremental`"
        )
    return problems, rows


def report(label, rows, problems, summary_path):
    lines = [f"| {label} | metric | measured | ceiling |", "| --- | --- | ---: | ---: |"]
    lines += [f"| {label} | {name} | {measured} | {ceiling} |" for name, measured, ceiling in rows]
    lines += [f"| {label} | verdict | {'REGRESSED' if problems else 'ok'} | |"]
    text = "\n".join(lines)
    print(text)
    for problem in problems:
        print(f"  - {problem}")
    if summary_path:
        with open(summary_path, "a") as summary:
            summary.write(text + "\n\n")


def self_test():
    baseline = {
        "cold": {
            "total_ms": 100,
            "index_base_ms": 20,
            "graph_elapsed_ms": 50,
            "min_files": 10,
            "min_symbols": 20,
        },
        "incremental": {"total_ms": 10, "graph_elapsed_ms": 5, "min_files": 10, "min_symbols": 20},
    }
    good_cold = {
        "timings": {
            "total_ms": 120,
            "index": {"base": "built_from_git", "base_ms": 25},
            "graph": {"elapsed_ms": 60, "build": {"mode": "full"}},
        },
        "graph": {"files": 12, "symbols": 30},
    }
    good_incr = {
        "timings": {"total_ms": 12, "graph": {"elapsed_ms": 6, "build": {"mode": "incremental"}}},
        "graph": {"files": 12, "symbols": 30},
    }
    over = json.loads(json.dumps(good_cold))
    over["timings"]["total_ms"] = 100 * 3 + 1
    vacuous = json.loads(json.dumps(good_cold))
    vacuous["timings"]["index"]["base"] = "reused"
    vacuous["graph"]["symbols"] = 0
    cases = [
        ("cold pass", "cold", good_cold, 3, []),
        ("incremental pass", "incremental", good_incr, 3, []),
        ("over ceiling", "cold", over, 3, ["cold.total_ms"]),
        ("vacuous cold", "cold", vacuous, 3, ["cold: index built", "graph.symbols"]),
    ]
    failures = []
    for name, label, receipt, multiplier, expected in cases:
        problems, _rows = evaluate(label, receipt, baseline, multiplier)
        for fragment in expected:
            if not any(fragment in problem for problem in problems):
                failures.append(f"{name}: no problem matching {fragment!r} in {problems}")
    # The same receipts through main(), spelled the way perf.yml calls it: the
    # cases above judge evaluate() and would stay green on a broken command line.
    with tempfile.TemporaryDirectory() as tmp:
        files = {"baseline": baseline | {"corpus": "self-test", "ceiling_multiplier": 3},
                 "good": good_cold, "over": over}
        for stem, doc in files.items():
            (Path(tmp) / f"{stem}.json").write_text(json.dumps(doc))
        cli_cases = [("cli pass", "good", 0), ("cli over ceiling", "over", 1)]
        for name, stem, expected in cli_cases:
            argv = ["--label", "cold", "--timings", f"{tmp}/{stem}.json",
                    "--baseline", f"{tmp}/baseline.json"]
            with contextlib.redirect_stdout(io.StringIO()):
                try:
                    code = main(argv)
                except Exception as error:  # a crash is the failure being guarded
                    code = repr(error)
            if code != expected:
                failures.append(f"{name}: main({argv}) returned {code!r}, not {expected}")
        cases += cli_cases
    if failures:
        for failure in failures:
            print(f"SELF-TEST FAIL: {failure}")
        return 1
    print(f"check-perf self-test: {len(cases)} cases pass")
    return 0


def main(argv):
    if "--self-test" in argv:
        return self_test()
    args = {
        "--label": None,
        "--timings": None,
        "--baseline": str(DEFAULT_BASELINE),
        "--multiplier": None,
        "--summary": None,
    }
    positional = []
    it = iter(argv)
    for arg in it:
        if arg in args:
            args[arg] = next(it)
        elif arg.startswith("--") and "=" in arg:
            key, value = arg.split("=", 1)
            if key not in args:
                print(f"unknown option {key}", file=sys.stderr)
                return 2
            args[key] = value
        else:
            positional.append(arg)
    label = args["--label"]
    timings = args["--timings"] or (positional[0] if positional else None)
    if label not in METRICS or not timings:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    baseline = json.loads(Path(args["--baseline"]).read_text())
    multiplier = int(args["--multiplier"] or baseline.get("ceiling_multiplier", 3))
    receipt = json.loads(Path(timings).read_text())
    problems, rows = evaluate(label, receipt, baseline, multiplier)
    report(label, rows, problems, args["--summary"])
    print(f"corpus: {baseline['corpus']}")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
