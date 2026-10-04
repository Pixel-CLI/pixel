#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Never-worse quality/turn gate with matched controlled-trial evidence.

Every host is compared with its own baseline. Missing metrics are unknown.
Controlled trials require matching frozen inputs, heldout verification,
complete request coverage and no interaction regression.
"""
import argparse
import json
import sys
from pathlib import Path


def compare(baseline, candidate, turns_slack=1.5, interactions_slack=1.0):
    if not baseline or not candidate:
        return False, "missing comparison data"
    if any(not row.get("answered") for row in candidate):
        return False, "candidate did not answer every trial"
    if any(row.get("turns") is None for row in baseline + candidate):
        return False, "unknown turn coverage"
    controlled = any("comparison_key" in row for row in baseline + candidate)
    if controlled:
        b_keys = [(row.get("comparison_key"), row.get("rep")) for row in baseline]
        c_keys = [(row.get("comparison_key"), row.get("rep")) for row in candidate]
        if (any(key is None or rep is None for key, rep in b_keys + c_keys)
                or len(set(b_keys)) != len(b_keys) or len(set(c_keys)) != len(c_keys)
                or set(b_keys) != set(c_keys)):
            return False, "unmatched or duplicate contract/source/runtime/repetition identities"
        if any(row.get("coverage") != "complete" or row.get("model_tool_requests") is None
               for row in baseline + candidate):
            return False, "unknown model-issued request coverage"
        if any(row.get("verifier_success") is not True for row in baseline + candidate):
            return False, "heldout verification did not pass on every matched attempt"
        b_requests = sum(row["model_tool_requests"] for row in baseline) / len(baseline)
        c_requests = sum(row["model_tool_requests"] for row in candidate) / len(candidate)
        if c_requests > interactions_slack * b_requests:
            return False, f"interaction regression: {c_requests:g} vs {b_requests:g}"
    b_score = sum(row["score"] for row in baseline) / len(baseline)
    c_score = sum(row["score"] for row in candidate) / len(candidate)
    b_turns = sum(row["turns"] for row in baseline) / len(baseline)
    c_turns = sum(row["turns"] for row in candidate) / len(candidate)
    passed = c_score >= b_score and c_turns <= turns_slack * b_turns
    return passed, f"score {c_score:g} vs {b_score:g}; turns {c_turns:g} vs {b_turns:g}"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--results", required=True)
    ap.add_argument("--scenarios-dir")
    ap.add_argument("--candidate", required=True)
    ap.add_argument("--baseline", default="baseline")
    ap.add_argument("--turns-slack", type=float, default=1.5)
    ap.add_argument("--interactions-slack", type=float, default=1.0)
    args = ap.parse_args()
    if args.turns_slack < 0 or args.interactions_slack < 0:
        ap.error("slack must be nonnegative")
    rows = json.loads((Path(args.results) / "scores.json").read_text())
    selected = [r for r in rows if r["arm"] in (args.baseline, args.candidate)]
    hosts = sorted({r["cli"] for r in selected})
    scenarios = sorted(p.stem for p in Path(args.scenarios_dir).glob("*.json")) if args.scenarios_dir else sorted({r["scenario"] for r in selected})
    failures = []
    if not hosts or not scenarios:
        failures.append("no comparison data")
    for host in hosts:
        for scenario in scenarios:
            baseline = [r for r in selected if r["cli"] == host and r["scenario"] == scenario and r["arm"] == args.baseline]
            candidate = [r for r in selected if r["cli"] == host and r["scenario"] == scenario and r["arm"] == args.candidate]
            passed, reason = compare(baseline, candidate, args.turns_slack, args.interactions_slack)
            print(f"  {host}/{scenario}: {reason} -> {'PASS' if passed else 'FAIL'}")
            if not passed:
                failures.append(f"{host}/{scenario}")
    if failures:
        print(f"GATE FAIL: {', '.join(failures)}")
        sys.exit(1)
    print("GATE PASS")


if __name__ == "__main__":
    main()
