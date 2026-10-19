#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Compare a perf receipt with the checked-in perf ceilings.

The weekly `perf.yml` lane builds the pinned corpus from nothing, then again
after one file change, and passes each `pixel prepare-repo --json` receipt
here (`cold`, `incremental`). It also scores the brief on the 56 public
questions and passes the scorer's `report.json` (`brief`): p95 and max
`elapsed_ms`, and how many briefs came back `partial` (cut by the window, or
past their `deadline_ms`). A metric over its ceiling
prints the number and exits 1; so does a receipt that is shaped like a pass but
measured nothing (a cold run that reused an index, one with no symbols, a
brief run that timed fewer questions than it asked or ran installed probes),
so the lane fails on a real regression and never on a vacuous one.

Usage: check-perf.py --label cold|incremental|brief --timings receipt.json
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
    "brief": {
        "p95_ms": "means.all.latency.p95_ms",
        "max_ms": "means.all.latency.max_ms",
    },
}
# Scorer statuses that mean a question was not measured. `partial` is measured:
# `score_brief.py` gives it to a brief flagged `cut` or whose `elapsed_ms`
# passed its `deadline_ms`, and the lane counts it against `max_partial`. The
# `cut` flag alone undercounts: with a 400 ms window, 18 of 56 briefs were
# `partial` and 2 carried `cut`.
UNMEASURED = ("timeout", "process_error", "malformed", "explicit_error")


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
    if label == "brief":
        rows.append(("partial", (dig(receipt, "result.counts") or {}).get("partial", 0),
                     limits["max_partial"]))
        problems += brief_shape(receipt, limits)
        return problems, rows
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


def brief_shape(receipt, limits):
    """Problems of a `score_brief.py` report that would make its latency vacuous or not comparable."""
    problems = []
    latency = dig(receipt, "means.all.latency") or {}
    timed, floor = latency.get("n"), limits["min_timed"]
    if not isinstance(timed, int) or timed < floor:
        problems.append(f"brief: {timed!r} briefs reported elapsed_ms, under the floor {floor}")
    counts = dig(receipt, "result.counts")
    if not isinstance(counts, dict) or not counts:
        problems.append(f"brief: report has no result.counts (got {counts!r})")
        counts = {}
    partial, ceiling = counts.get("partial", 0), limits["max_partial"]
    if partial > ceiling:
        problems.append(
            f"brief: {partial} briefs partial (cut or past deadline_ms), over the ceiling of {ceiling}"
        )
    lost = {status: counts[status] for status in UNMEASURED if counts.get(status)}
    if lost:
        problems.append(f"brief: questions not measured: {lost}")
    if dig(receipt, "route") != limits["route"]:
        problems.append(f"brief: route {dig(receipt, 'route')!r}, the baseline is {limits['route']!r}")
    if dig(receipt, "probe_home.frozen") is not True:
        problems.append("brief: the briefs ran the installed probes, not a frozen probe home")
    if dig(receipt, "questions.sha256") != limits["questions_sha256"]:
        problems.append(
            f"brief: question file {dig(receipt, 'questions.sha256')!r} is not the baseline's "
            f"{limits['questions_sha256']!r}: refresh the baseline in the same PR"
        )
    return problems


def report(label, rows, problems, summary_path, provenance=""):
    lines = [f"| {label} | metric | measured | ceiling |", "| --- | --- | ---: | ---: |"]
    lines += [f"| {label} | {name} | {measured} | {ceiling} |" for name, measured, ceiling in rows]
    lines += [f"| {label} | verdict | {'REGRESSED' if problems else 'ok'} | |"]
    lines += [f"- {problem}" for problem in problems] + ([provenance] if provenance else [])
    text = "\n".join(lines)
    print(text)
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
        "brief": {"p95_ms": 100, "max_ms": 200, "max_partial": 1, "min_timed": 4, "route": "hook",
                  "questions_sha256": "q1"},
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
    # One partial brief is within `max_partial`: the scorer marks the run failed
    # (`partial` is not complete), but the lane judges the count.
    good_brief = {
        "route": "hook",
        "probe_home": {"frozen": True},
        "questions": {"sha256": "q1"},
        "result": {"status": "failed", "counts": {"ok": 3, "partial": 1}},
        "means": {"all": {"latency": {"n": 4, "p95_ms": 250, "max_ms": 500, "cut": 0}}},
    }
    slow_brief = json.loads(json.dumps(good_brief))
    slow_brief["means"]["all"]["latency"]["p95_ms"] = 301
    slow_brief["result"]["counts"] = {"ok": 2, "partial": 2}
    vacuous_brief = json.loads(json.dumps(good_brief))
    vacuous_brief["means"]["all"]["latency"]["n"] = 3
    vacuous_brief["result"]["counts"] = {"ok": 3, "timeout": 1}
    vacuous_brief["probe_home"]["frozen"] = False
    vacuous_brief["questions"]["sha256"] = "q2"
    vacuous_brief["route"] = "command"
    cases = [
        ("cold pass", "cold", good_cold, 3, []),
        ("incremental pass", "incremental", good_incr, 3, []),
        ("over ceiling", "cold", over, 3, ["cold.total_ms"]),
        ("vacuous cold", "cold", vacuous, 3, ["cold: index built", "graph.symbols"]),
        ("brief pass, one cut", "brief", good_brief, 3, []),
        ("brief slower and cut more", "brief", slow_brief, 3, ["brief.p95_ms", "2 briefs partial"]),
        ("vacuous brief", "brief", vacuous_brief, 3,
         ["3 briefs reported", "'timeout': 1", "installed probes", "question file", "route 'command'"]),
    ]
    failures = []
    for name, label, receipt, multiplier, expected in cases:
        problems, _rows = evaluate(label, receipt, baseline, multiplier)
        if not expected and problems:
            failures.append(f"{name}: expected a pass, got {problems}")
        for fragment in expected:
            if not any(fragment in problem for problem in problems):
                failures.append(f"{name}: no problem matching {fragment!r} in {problems}")
    # The same receipts through main(), spelled the way perf.yml calls it: the
    # cases above judge evaluate() and would stay green on a broken command line.
    with tempfile.TemporaryDirectory() as tmp:
        files = {"baseline": baseline | {"corpus": "self-test", "ceiling_multiplier": 3},
                 "good": good_cold, "over": over, "brief": good_brief, "slow": slow_brief}
        for stem, doc in files.items():
            (Path(tmp) / f"{stem}.json").write_text(json.dumps(doc))
        cli_cases = [("cli pass", "cold", "good", 0), ("cli over ceiling", "cold", "over", 1),
                     ("cli brief pass", "brief", "brief", 0), ("cli brief slow", "brief", "slow", 1)]
        for name, label, stem, expected in cli_cases:
            argv = ["--label", label, "--timings", f"{tmp}/{stem}.json",
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
    limits = baseline[label]
    # Each section names the doc its numbers refresh and the run that set them.
    provenance = (f"corpus: {limits.get('corpus', baseline.get('corpus'))} · baseline: "
                  f"{limits.get('measured', baseline.get('measured'))} · refreshes: {limits.get('doc')}")
    report(label, rows, problems, args["--summary"], provenance)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
