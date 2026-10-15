#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Summarise the brief's read feedback (issue #906): did the brief save the agent its tool calls?

``.pixel/brief-feedback.jsonl`` is written by the registered ``run-hook task-event`` hooks
(``crates/pixel/src/execution_brief/feedback.rs``). Each ``turn`` line closes one prompt: the paths
the brief injected, the paths the agent read or edited before its first answer, their overlap and
the number of tool calls in between. This script aggregates them:

* the zero-call rate: turns that were answered with no tool call, of the briefed turns and of the
  turns that got no brief;
* precision: of the paths the brief injected, how many the agent then read (``overlap / injected``);
* recall: of the paths the agent read, how many the brief had injected (``overlap / read``);
* delivery: the briefed prompts whose hook parts printed no byte to the host ("brief not delivered").

Precision and recall are reported pooled (sum of overlaps over sum of sizes) and averaged per turn.
The file holds paths and counts only; so does the output.

    brief-feedback-summary.py [REPO_OR_FILE ...] [--json]
    brief-feedback-summary.py --self-test

Standard library only; run with ``python3 -I``.
"""

import argparse
import json
import sys
import unittest
from pathlib import Path

LOG = Path(".pixel") / "brief-feedback.jsonl"


def load(path):
    """The parsed lines of a feedback log; a line that is not JSON is skipped."""
    path = Path(path)
    if path.is_dir():
        path = path / LOG
    lines = []
    if path.exists():
        for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
            try:
                lines.append(json.loads(line))
            except ValueError:
                continue
    return lines


def ratio(num, den):
    return None if den == 0 else num / den


def mean(values):
    values = [v for v in values if v is not None]
    return None if not values else sum(values) / len(values)


def summarize(lines):
    """Aggregates of the ``turn`` lines, and the delivery of the ``prompt`` and ``part`` lines."""
    turns = [line for line in lines if line.get("t") == "turn"]
    briefed = [t for t in turns if t.get("briefed")]
    plain = [t for t in turns if not t.get("briefed")]

    def zero(group):
        return sum(1 for t in group if t.get("calls") == 0)

    inj = sum(len(t.get("injected") or []) for t in briefed)
    read = sum(len(set(t.get("read") or []) | set(t.get("edited") or [])) for t in briefed)
    hit = sum(len(t.get("overlap") or []) for t in briefed)
    per_precision = [ratio(len(t.get("overlap") or []), len(t.get("injected") or [])) for t in briefed]
    per_recall = [ratio(len(t.get("overlap") or []),
                        len(set(t.get("read") or []) | set(t.get("edited") or []))) for t in briefed]
    printed = {}
    for line in lines:
        if line.get("t") == "part":
            printed[(line.get("session"), line.get("prompt"))] = printed.get(
                (line.get("session"), line.get("prompt")), 0) + int(line.get("bytes") or 0)
    prompts = [line for line in lines if line.get("t") == "prompt" and int(line.get("bytes") or 0) > 0]
    # A host that runs part hooks reports their bytes; one that runs a single hook has a part line too.
    not_delivered = [p for p in prompts
                     if (p.get("session"), p.get("prompt")) in printed
                     and printed[(p.get("session"), p.get("prompt"))] == 0]
    return {
        "turns": len(turns),
        "briefed_turns": len(briefed),
        "unbriefed_turns": len(plain),
        "zero_call_briefed": zero(briefed),
        "zero_call_rate_briefed": ratio(zero(briefed), len(briefed)),
        "zero_call_unbriefed": zero(plain),
        "zero_call_rate_unbriefed": ratio(zero(plain), len(plain)),
        "mean_calls_briefed": mean([t.get("calls") for t in briefed]),
        "mean_calls_unbriefed": mean([t.get("calls") for t in plain]),
        "injected_paths": inj,
        "read_paths": read,
        "overlap_paths": hit,
        "precision_pooled": ratio(hit, inj),
        "recall_pooled": ratio(hit, read),
        "precision_per_turn": mean(per_precision),
        "recall_per_turn": mean(per_recall),
        "briefed_prompts": len(prompts),
        "brief_not_delivered": len(not_delivered),
    }


def percent(value):
    return "n/a" if value is None else f"{100 * value:.0f}%"


def number(value):
    return "n/a" if value is None else f"{value:.1f}"


def render(summary):
    s = summary
    return "\n".join([
        f"turns {s['turns']}  (briefed {s['briefed_turns']}, not briefed {s['unbriefed_turns']})",
        f"zero-call rate  briefed {percent(s['zero_call_rate_briefed'])} "
        f"({s['zero_call_briefed']}/{s['briefed_turns']})  not briefed {percent(s['zero_call_rate_unbriefed'])} "
        f"({s['zero_call_unbriefed']}/{s['unbriefed_turns']})",
        f"mean tool calls before the first answer  briefed {number(s['mean_calls_briefed'])}  "
        f"not briefed {number(s['mean_calls_unbriefed'])}",
        f"precision (injected paths the agent read)  pooled {percent(s['precision_pooled'])} "
        f"({s['overlap_paths']}/{s['injected_paths']})  per turn {percent(s['precision_per_turn'])}",
        f"recall (read paths the brief injected)     pooled {percent(s['recall_pooled'])} "
        f"({s['overlap_paths']}/{s['read_paths']})  per turn {percent(s['recall_per_turn'])}",
        f"brief not delivered: {s['brief_not_delivered']} of {s['briefed_prompts']} briefed prompts",
    ])


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("paths", nargs="*", default=["."], help="repository roots or feedback files (default .)")
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(SelfTest)
        return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1
    lines = [line for path in args.paths for line in load(path)]
    summary = summarize(lines)
    print(json.dumps(summary, indent=1) if args.json else render(summary))
    return 0


def _turn(briefed, calls, injected, read, edited=()):
    overlap = [p for p in injected if p in read or p in edited]
    return {"t": "turn", "briefed": briefed, "calls": calls, "injected": list(injected), "read": list(read),
            "edited": list(edited), "overlap": overlap}


class SelfTest(unittest.TestCase):
    def test_rates_pool_and_average_over_briefed_turns_only(self):
        lines = [
            _turn(True, 0, ["a", "b"], []),
            _turn(True, 2, ["a", "b", "c", "d"], ["a", "x"]),
            _turn(False, 3, [], ["z"]),
            _turn(False, 0, [], []),
            {"t": "tool", "calls": 9},
        ]
        s = summarize(lines)
        self.assertEqual((s["turns"], s["briefed_turns"], s["unbriefed_turns"]), (4, 2, 2))
        self.assertEqual(s["zero_call_rate_briefed"], 0.5)
        self.assertEqual(s["zero_call_rate_unbriefed"], 0.5)
        self.assertEqual(s["injected_paths"], 6)
        self.assertEqual(s["read_paths"], 2)
        self.assertEqual(s["overlap_paths"], 1)
        self.assertAlmostEqual(s["precision_pooled"], 1 / 6)
        self.assertAlmostEqual(s["recall_pooled"], 1 / 2)
        # The zero-call turn has nothing read: its recall is undefined, not zero.
        self.assertAlmostEqual(s["recall_per_turn"], 1 / 2)
        self.assertAlmostEqual(s["precision_per_turn"], (0 + 1 / 4) / 2)
        self.assertEqual(s["mean_calls_briefed"], 1.0)

    def test_an_empty_log_reports_nothing_rather_than_zero(self):
        s = summarize([])
        self.assertIsNone(s["zero_call_rate_briefed"])
        self.assertIsNone(s["precision_pooled"])
        self.assertIn("zero-call rate  briefed n/a", render(s))

    def test_a_briefed_prompt_whose_parts_printed_nothing_is_not_delivered(self):
        lines = [
            {"t": "prompt", "session": "s", "prompt": "p1", "bytes": 500},
            {"t": "part", "session": "s", "prompt": "p1", "part": 1, "bytes": 0},
            {"t": "part", "session": "s", "prompt": "p1", "part": 2, "bytes": 0},
            {"t": "prompt", "session": "s", "prompt": "p2", "bytes": 500},
            {"t": "part", "session": "s", "prompt": "p2", "part": 1, "bytes": 300},
            {"t": "part", "session": "s", "prompt": "p2", "part": 2, "bytes": 0},
            {"t": "prompt", "session": "s", "prompt": "p3", "bytes": 0},
        ]
        s = summarize(lines)
        self.assertEqual((s["briefed_prompts"], s["brief_not_delivered"]), (2, 1))

    def test_load_skips_lines_that_are_not_json_and_finds_the_log_of_a_repository(self):
        import tempfile
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / LOG
            log.parent.mkdir()
            log.write_text('{"t":"stop"}\nnot json\n{"t":"turn","briefed":true,"calls":0}\n')
            self.assertEqual([line["t"] for line in load(tmp)], ["stop", "turn"])
            self.assertEqual(load(Path(tmp) / "elsewhere"), [])


if __name__ == "__main__":
    sys.exit(main())
