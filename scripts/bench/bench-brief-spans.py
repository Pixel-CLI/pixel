#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Does the prompt-submit brief contain the lines that answer the question?

``eval/brief-gate/answer_spans.jsonl`` labels, per question of
``prompts.jsonl``, the line ranges of the code that answers it. For each
labelled question this runs ``pixel brief --json`` and reports, over the
briefs that fired,

* span_hit@excerpt: some line of an ``answer`` block lies inside a gold span;
* span_hit@brief: that, or a ``defined:`` range overlaps a gold span;
* pointer: a ``files:`` entry points inside a gold span;
* zero: briefs that end on the ZERO instruction ("Answer from these lines"),
  and how many of them show an excerpt overlapping a gold span (precision);
* read@1 / read@1-3: a ``read:`` (first) or ``read:``/``also:`` (all) range
  overlaps a gold span (``*_in``: contains the whole span);
* answered: ZERO correct, or no ZERO and a read@1-3 range overlapping.

The labels belong to the fixture checkout (the SHA in ``bench-brief-gate.py``),
so run against an indexed checkout of it with a warm daemon::

    python3 scripts/bench/bench-brief-spans.py --pixel target/dev-release/pixel \
        --repo <fixture checkout> --split dev

Tune on ``dev``; measure ``test`` once. Standard library only.
"""

import argparse
import collections
import contextlib
import hashlib
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
PROMPTS = ROOT / "eval" / "brief-gate" / "prompts.jsonl"
SPANS = ROOT / "eval" / "brief-gate" / "answer_spans.jsonl"
FIXTURE_SHA = "85bede7d9a3c4e385e0a2045241a6466efbd8cbd"
SHAPES = {"symbol", "where-behavior", "multi-hop", "edit-set"}


def parse(brief):
    """``(excerpt lines by path, defined ranges, files pointers)`` of ``brief``."""
    excerpt, defined, pointers = collections.defaultdict(set), [], []
    current = None
    for line in brief.split("\n"):
        header = re.match(r"answer (\S+):(\d+):$", line)
        if header:
            current = header.group(1)
            continue
        numbered = re.match(r"\s+(\d+)\| ", line)
        if numbered and current:
            excerpt[current].add(int(numbered.group(1)))
            continue
        span = re.match(r"defined: \w+ \S+ (\S+):(\d+)-(\d+)", line)
        if span:
            defined.append((span.group(1), int(span.group(2)), int(span.group(3))))
            current = None
            continue
        if line.startswith("files: "):
            for item in line[7:].split("; "):
                site = re.match(r"([^\s:]+):(\d+)", item)
                if site:
                    pointers.append((site.group(1), int(site.group(2))))
            current = None
            continue
        if not line.startswith(" "):
            current = None
    return excerpt, defined, pointers


def presented_lines(brief):
    """Unique numbered source lines actually shown in answer/probe blocks."""
    found = collections.defaultdict(dict)
    current = None
    for line in brief.splitlines():
        header = re.match(r"answer (\S+):\d+:$", line)
        probe = re.match(r"### (\S+):\d+(?:-\d+)?(?:\s+—.*)?$", line)
        if header or probe:
            current = (header or probe).group(1)
            continue
        numbered = re.match(r"\s*(\d+)\s*\|\s?(.*)$", line)
        if numbered and current:
            found[current].setdefault(int(numbered.group(1)), numbered.group(2))
            continue
        if not line.startswith(" "):
            current = None
    return dict(found)


def shape_of(gold):
    shape = gold.get("shape")
    if shape is None:
        return "unclassified"
    if not isinstance(shape, str) or shape not in SHAPES:
        raise ValueError(f"{gold.get('id', '<unknown>')}: unknown answer-span shape {shape!r}")
    return shape


def set_scores(predicted, expected):
    tp = len(predicted & expected)
    fp = len(predicted - expected)
    fn = len(expected - predicted)
    precision = tp / len(predicted) if predicted else 0.0
    recall = tp / len(expected) if expected else 0.0
    f05 = 1.25 * precision * recall / (0.25 * precision + recall) if precision + recall else 0.0
    return {"tp": tp, "fp": fp, "fn": fn, "precision": precision, "recall": recall, "f0.5": f05}


def quality_scores(spans, shown):
    expected_lines = {(span["path"], line)
                      for span in spans for line in range(span["start_line"], span["end_line"] + 1)}
    predicted_lines = {(path, line) for path, lines in shown.items() for line in lines}
    expected_files = {span["path"] for span in spans}
    predicted_files = {path for path, lines in shown.items() if lines}
    return {"line": set_scores(predicted_lines, expected_lines),
            "file": set_scores(predicted_files, expected_files)}


def score_totals(rows, key):
    counts = {name: sum(row["quality"][key][name] for row in rows) for name in ("tp", "fp", "fn")}
    tp, fp, fn = counts["tp"], counts["fp"], counts["fn"]
    precision = tp / (tp + fp) if tp + fp else 0.0
    recall = tp / (tp + fn) if tp + fn else 0.0
    f05 = 1.25 * precision * recall / (0.25 * precision + recall) if precision + recall else 0.0
    denominator = len(rows)
    macro = {name: sum(row["quality"][key][name] for row in rows) / denominator if denominator else 0.0
             for name in ("precision", "recall", "f0.5")}
    return {**counts, "precision": precision, "recall": recall, "f0.5": f05,
            "macro": {"n": denominator, **macro}}


def percentile(values, q):
    if not values:
        return None
    return sorted(values)[max(0, (q * len(values) + 99) // 100 - 1)]


def summarize_quality(rows):
    return {"n": len(rows), "failed": sum(row["failed"] for row in rows),
            "line": score_totals(rows, "line"), "file": score_totals(rows, "file"),
            "cli_elapsed_ms": {"p50": percentile([r["elapsed_ms"] for r in rows], 50),
                               "p95": percentile([r["elapsed_ms"] for r in rows], 95)},
            "bytes": {"total": sum(row["bytes"] for row in rows),
                      "p50": percentile([r["bytes"] for r in rows], 50),
                      "p95": percentile([r["bytes"] for r in rows], 95)}}


def print_quality_table(quality, by_shape):
    def number(value):
        return "n/a" if value is None else f"{value:.3f}"

    print("annotated-reference metrics (pooled F0.5; CLI timing, not hook p95)")
    print("shape | n | failed | line pooled P/R/F0.5 | line macro F0.5 | "
          "file pooled P/R/F0.5 | file macro F0.5 | CLI p95 ms | bytes p50/p95")
    rows = [("all", quality), *sorted(by_shape.items())]
    for shape, summary in rows:
        line, file = summary["line"], summary["file"]
        print(f"{shape} | {summary['n']} | {summary['failed']} | "
              f"{number(line['precision'])}/{number(line['recall'])}/{number(line['f0.5'])} | "
              f"{number(line['macro']['f0.5'])} | "
              f"{number(file['precision'])}/{number(file['recall'])}/{number(file['f0.5'])} | "
              f"{number(file['macro']['f0.5'])} | "
              f"{number(summary['cli_elapsed_ms']['p95'])} | "
              f"{number(summary['bytes']['p50'])}/{number(summary['bytes']['p95'])}")


def read_ranges(brief):
    """``(path, first, last)`` of each ``read:`` / ``also:`` line, in order."""
    found = []
    for line in brief.split("\n"):
        match = re.match(r"(?:read|also): (\S+):(\d+)-(\d+)", line)
        if match:
            found.append((match.group(1), int(match.group(2)), int(match.group(3))))
    return found


def hits(spans, brief):
    excerpt, defined, pointers = parse(brief)
    ranges = read_ranges(brief)
    over = lambda sp, r: sp["path"] == r[0] and r[1] <= sp["end_line"] and r[2] >= sp["start_line"]
    within = lambda sp, r: sp["path"] == r[0] and r[1] <= sp["start_line"] and r[2] >= sp["end_line"]
    inside = lambda sp, path, n: sp["path"] == path and sp["start_line"] <= n <= sp["end_line"]
    in_excerpt = any(inside(sp, path, n) for sp in spans for path, lines in excerpt.items() for n in lines)
    in_defined = any(sp["path"] == path and start <= sp["end_line"] and end >= sp["start_line"]
                     for sp in spans for path, start, end in defined)
    pointed = any(inside(sp, path, n) for sp in spans for path, n in pointers)
    zero = "Answer from these lines" in brief
    read1 = any(over(sp, ranges[0]) for sp in spans) if ranges else False
    read3 = any(over(sp, r) for sp in spans for r in ranges)
    return {"excerpt": in_excerpt, "brief": in_excerpt or in_defined, "pointer": pointed,
            "zero": zero, "zero_ok": zero and in_excerpt, "read1": read1, "read3": read3,
            "read1_in": any(within(sp, ranges[0]) for sp in spans) if ranges else False,
            "read3_in": any(within(sp, r) for sp in spans for r in ranges),
            "has_read": bool(ranges),
            "answered": (zero and in_excerpt) or (not zero and read3)}


def summarize(rows):
    fired = [row for row in rows if row["fired"]]
    count = lambda key: sum(row[key] for row in fired)
    return {"n": len(rows), "fired": len(fired), "failed": sum(row["failed"] for row in rows),
            "span_hit@excerpt": count("excerpt"),
            "span_hit@brief": count("brief"), "pointer": count("pointer"),
            "zero": count("zero"), "zero_ok": count("zero_ok"), "has_read": count("has_read"),
            "read@1": count("read1"), "read@1-3": count("read3"),
            "read@1_in": count("read1_in"), "read@1-3_in": count("read3_in"),
            "answered": count("answered"),
            "bytes_p50": sorted(r["bytes"] for r in fired)[len(fired) // 2] if fired else None}


def run(pixel, repo, text, timeout=60):
    started = time.perf_counter()
    env = dict(os.environ, PIXEL_BRIEF="1", PIXEL_DAEMON_AUTO_START="0", NO_COLOR="1")
    try:
        done = subprocess.run([str(pixel), "brief", "--json", "--metrics", "off", "--parts", "1", text, str(repo)],
                              cwd=repo, capture_output=True, text=True, timeout=timeout, env=env)
    except subprocess.TimeoutExpired:
        return {"brief": "", "tier": None, "elapsed_ms": (time.perf_counter() - started) * 1000,
                "failed": True, "error": "timeout"}
    except OSError:
        return {"brief": "", "tier": None, "elapsed_ms": (time.perf_counter() - started) * 1000,
                "failed": True, "error": "launch-error"}
    elapsed_ms = (time.perf_counter() - started) * 1000
    if done.returncode != 0:
        return {"brief": "", "tier": None, "elapsed_ms": elapsed_ms, "failed": True,
                "error": "nonzero-exit"}
    first = done.stdout.split("\n", 1)[0]
    try:
        result = json.loads(first)
    except json.JSONDecodeError:
        result = None
    if not isinstance(result, dict) or "brief" not in result or result["brief"] is not None and not isinstance(result["brief"], str):
        return {"brief": "", "tier": None, "elapsed_ms": elapsed_ms, "failed": True,
                "error": "invalid-response"}
    return {"brief": result["brief"] or "", "tier": result.get("tier"), "elapsed_ms": elapsed_ms,
            "failed": False, "error": None}


def validate_fixture(repo):
    try:
        result = subprocess.run(["git", "rev-parse", "HEAD"], cwd=repo, capture_output=True,
                                text=True, check=True)
    except (OSError, subprocess.CalledProcessError) as error:
        raise ValueError(f"cannot verify fixture HEAD at {repo}") from error
    actual = result.stdout.strip()
    if actual != FIXTURE_SHA:
        raise ValueError(f"answer spans require fixture {FIXTURE_SHA}; got {actual}")
    dirty = subprocess.run(["git", "diff", "--quiet", "HEAD", "--"], cwd=repo)
    if dirty.returncode:
        raise ValueError("answer-span fixture has tracked working-tree changes")


def validate_gold(repo, gold_rows, prompts=None):
    """Reject answer spans that cannot refer to tracked source in ``repo``."""
    root = Path(repo).resolve()
    try:
        result = subprocess.run(["git", "ls-files", "--stage", "-z"], cwd=root,
                                capture_output=True, check=True)
    except (OSError, subprocess.CalledProcessError) as error:
        raise ValueError(f"cannot list tracked fixture files at {root}") from error

    tracked_files = set()
    for entry in result.stdout.split(b"\0"):
        if not entry:
            continue
        try:
            metadata, raw_path = entry.split(b"\t", 1)
            mode, _, stage = metadata.decode("ascii").split()
            path = os.fsdecode(raw_path)
        except (UnicodeDecodeError, ValueError):
            continue
        if stage == "0" and mode in {"100644", "100755"}:
            tracked_files.add(path)

    errors = []
    checked_paths = {}
    seen_ids = set()
    for gold in gold_rows:
        question_id = gold.get("id") if isinstance(gold, dict) else None
        label = question_id if isinstance(question_id, str) and question_id else "<unknown>"
        if not isinstance(gold, dict):
            errors.append(f"{label}: gold row must be an object")
            continue
        if not isinstance(question_id, str) or not question_id:
            errors.append(f"{label}: id must be a non-empty string")
        elif question_id in seen_ids:
            errors.append(f"{label}: duplicate gold question id")
        else:
            seen_ids.add(question_id)
        if prompts is not None and isinstance(question_id, str) and question_id not in prompts:
            errors.append(f"{label}: id has no matching prompt")
        spans = gold.get("answer_spans")
        if not isinstance(spans, list) or not spans:
            errors.append(f"{label}: answer_spans must be a non-empty list")
            continue

        for index, span in enumerate(spans, start=1):
            where = f"{label} span {index}"
            if not isinstance(span, dict):
                errors.append(f"{where}: span must be an object")
                continue
            raw_path = span.get("path")
            if not isinstance(raw_path, str) or not raw_path:
                errors.append(f"{where}: path must be a non-empty relative path")
                continue
            parts = raw_path.split("/")
            if (raw_path.startswith("/") or "\\" in raw_path or
                    any(part in {"", ".", ".."} for part in parts)):
                errors.append(f"{where}: path must be a normalized repository-relative path ({raw_path!r})")
                continue
            source = checked_paths.get(raw_path)
            if source is None:
                source = root.joinpath(*parts)
                checked_paths[raw_path] = source
            if raw_path not in tracked_files:
                errors.append(f"{where}: path is not a tracked regular source file ({raw_path})")
                continue
            try:
                source.resolve(strict=True).relative_to(root)
            except (OSError, RuntimeError, ValueError):
                errors.append(f"{where}: source is missing or resolves outside the fixture ({raw_path})")
                continue
            if source.is_symlink() or not source.is_file():
                errors.append(f"{where}: source is not a regular file ({raw_path})")
                continue

            start, end = span.get("start_line"), span.get("end_line")
            if (not isinstance(start, int) or isinstance(start, bool) or start < 1 or
                    not isinstance(end, int) or isinstance(end, bool) or end < start):
                errors.append(f"{where}: start_line/end_line must be positive ordered integers")
                continue
            try:
                source_bytes = source.read_bytes()
            except OSError:
                errors.append(f"{where}: cannot read source ({raw_path})")
                continue
            line_count = source_bytes.count(b"\n")
            if source_bytes and not source_bytes.endswith(b"\n"):
                line_count += 1
            if end > line_count:
                errors.append(f"{where}: end_line {end} exceeds {raw_path} line count {line_count}")

    if errors:
        raise ValueError("invalid answer-span gold:\n" + "\n".join(f"- {error}" for error in errors))


def resolve_pixel(pixel, launch_dir=None):
    value = str(pixel)
    path = Path(value)
    if path.is_absolute():
        return str(path)
    if os.sep in value or len(path.parts) > 1:
        return str(((Path.cwd() if launch_dir is None else Path(launch_dir)) / path).resolve())
    return shutil.which(str(path)) or str(path)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--pixel")
    parser.add_argument("--repo")
    parser.add_argument("--split", choices=("dev", "test", "all"), default="dev")
    parser.add_argument("--out")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        sys.argv = [sys.argv[0]]
        unittest.main(module=sys.modules[__name__], exit=True)
    if not (args.pixel and args.repo):
        parser.error("--pixel and --repo are required")
    try:
        validate_fixture(args.repo)
    except ValueError as error:
        parser.error(str(error))
    pixel = resolve_pixel(args.pixel)
    prompt_bytes = PROMPTS.read_bytes()
    span_bytes = SPANS.read_bytes()
    prompts = {r["id"]: r for r in map(json.loads, prompt_bytes.splitlines()) if r.get("id")}
    gold_rows = list(map(json.loads, span_bytes.splitlines()))
    try:
        for gold in gold_rows:
            if not isinstance(gold, dict):
                raise ValueError("gold row must be an object")
            shape_of(gold)
        validate_gold(args.repo, gold_rows, prompts)
    except ValueError as error:
        parser.error(str(error))
    rows = []
    for gold in gold_rows:
        prompt = prompts[gold["id"]]
        if args.split != "all" and prompt["split"] != args.split:
            continue
        shape = shape_of(gold)
        got = run(pixel, args.repo, prompt["text"])
        brief = got["brief"]
        shown = presented_lines(brief)
        delivered = [{"path": path, "line": number, "text": text}
                     for path, lines in shown.items() for number, text in lines.items()]
        row = {"id": gold["id"], "shape": shape, "fired": bool(brief), "failed": got["failed"],
               "error": got["error"], "bytes": len(brief.encode()), "elapsed_ms": got["elapsed_ms"],
               "brief_text": brief,
               "shown_lines": delivered, "quality": quality_scores(gold["answer_spans"], shown),
               "tier": got.get("tier"), "directive": "Answer from this evidence" in brief}
        row.update(hits(gold["answer_spans"], brief))
        rows.append(row)
    groups = {"strong": [r for r in rows if r["fired"] and r["tier"] is None],
              "prose": [r for r in rows if r["tier"] in ("high", "low")]}
    shapes = sorted({row["shape"] for row in rows})
    result = {"split": args.split, "fixture_sha": FIXTURE_SHA,
              "prompts_sha256": hashlib.sha256(prompt_bytes).hexdigest(),
              "answer_spans_sha256": hashlib.sha256(span_bytes).hexdigest(),
              "parts": 1, "summary": summarize(rows),
              "quality": summarize_quality(rows), "rows": rows,
              "by_signal": {name: summarize(group) for name, group in groups.items()},
              "quality_by_shape": {shape: summarize_quality([r for r in rows if r["shape"] == shape])
                                   for shape in shapes}}
    if args.out:
        Path(args.out).write_text(json.dumps(result, indent=1))
    print(json.dumps(result["summary"]))
    for name, summary in result["by_signal"].items():
        keys = ("n", "has_read", "read@1", "read@1-3", "answered")
        print(f"  {name}: " + " ".join(f"{k}={summary[k]}" for k in keys))
    print_quality_table(result["quality"], result["quality_by_shape"])


class Tests(unittest.TestCase):
    BRIEF = "\n".join([
        "[PIXEL:BRIEF]", "defined: function f a.rs:10-20 — x", "files: b.rs:5 — t; c.rs:9",
        "answer a.rs:30:", "  30| fn g() {", "  31|     go();", "answer b.rs:2:", "  2| fn h() {",
    ])

    def test_hits_should_tell_excerpt_defined_and_pointer_apart(self):
        span = lambda path, a, b: [{"path": path, "start_line": a, "end_line": b}]
        pick = lambda got: (got["excerpt"], got["brief"], got["pointer"])
        self.assertEqual(pick(hits(span("a.rs", 31, 40), self.BRIEF)), (True, True, False))
        self.assertEqual(pick(hits(span("a.rs", 15, 16), self.BRIEF)), (False, True, False))
        self.assertEqual(pick(hits(span("c.rs", 8, 12), self.BRIEF)), (False, False, True))
        self.assertEqual(pick(hits(span("z.rs", 1, 9), self.BRIEF)), (False, False, False))

    def test_hits_should_score_the_zero_line_and_the_read_ranges(self):
        brief = "answer a.rs:30:\n  30| fn g() {\nread: a.rs:28-40 — g\nalso: b.rs:1-9 — h\nAnswer from these lines; open."
        span = lambda path, a, b: [{"path": path, "start_line": a, "end_line": b}]
        got = hits(span("a.rs", 30, 31), brief)
        self.assertTrue(got["zero"] and got["zero_ok"] and got["read1"] and got["read1_in"] and got["answered"])
        got = hits(span("b.rs", 5, 6), brief)
        self.assertTrue(not got["read1"] and got["read3"] and got["read3_in"] and not got["zero_ok"])
        self.assertEqual(read_ranges(brief), [("a.rs", 28, 40), ("b.rs", 1, 9)])

    def test_quality_scores_only_deduplicated_injected_lines(self):
        brief = "\n".join([
            "defined: function f hidden.rs:1-20 — pointer only", "files: files.rs:4 — pointer",
            "read: read.rs:1-20 — suggestion", "answer a.rs:10:", "  10| answer one",
            "  11| answer two", "### a.rs:10-12 — probe", "10| answer one", "11| answer two",
            "### noisy.rs:30 — probe", "30| unrelated line",
        ])
        gold = [{"path": "a.rs", "start_line": 10, "end_line": 11},
                {"path": "missing.rs", "start_line": 1, "end_line": 2}]
        shown = presented_lines(brief)
        scores = quality_scores(gold, shown)
        self.assertEqual(shown, {"a.rs": {10: "answer one", 11: "answer two"},
                                 "noisy.rs": {30: "unrelated line"}})
        self.assertEqual(scores["line"], {"tp": 2, "fp": 1, "fn": 2, "precision": 2 / 3,
                                          "recall": 0.5, "f0.5": 0.625})
        self.assertEqual(scores["file"], {"tp": 1, "fp": 1, "fn": 1, "precision": 0.5,
                                          "recall": 0.5, "f0.5": 0.5})

    def test_quality_parser_accepts_single_line_labeled_probe_blocks(self):
        brief = "### src/one.rs:20 — the relevant statement\n20| let answer = true;"
        self.assertEqual(presented_lines(brief), {"src/one.rs": {20: "let answer = true;"}})

    def test_failed_or_empty_briefs_cannot_score_as_hits(self):
        gold = [{"path": "a.rs", "start_line": 1, "end_line": 2}]
        scores = quality_scores(gold, {})
        self.assertEqual(scores["line"]["tp"], 0)
        self.assertEqual(scores["line"]["fn"], 2)
        self.assertEqual(scores["line"]["f0.5"], 0)
        self.assertEqual(scores["file"]["fn"], 1)
        self.assertEqual(scores["file"]["f0.5"], 0)

    def test_quality_summary_counts_failures_and_unclassified_shape(self):
        gold = [{"path": "a.rs", "start_line": 1, "end_line": 1}]
        rows = [{"shape": "unclassified", "failed": True, "bytes": 0, "elapsed_ms": 8,
                 "quality": quality_scores(gold, {})}]
        summary = summarize_quality(rows)
        self.assertEqual(summary["failed"], 1)
        self.assertEqual(summary["line"]["fn"], 1)
        self.assertEqual(summary["line"]["f0.5"], 0)
        self.assertEqual(summary["line"]["macro"], {"n": 1, "precision": 0, "recall": 0, "f0.5": 0})

    def test_shapes_are_only_accepted_from_the_annotation(self):
        self.assertEqual(shape_of({}), "unclassified")
        self.assertEqual(shape_of({"shape": "multi-hop"}), "multi-hop")
        with self.assertRaises(ValueError):
            shape_of({"shape": "guessed"})

    def test_run_counts_process_failures_as_empty_and_accepts_valid_json(self):
        for result in (subprocess.CompletedProcess([], 2, '{"brief":"must be ignored"}', ""),
                       subprocess.CompletedProcess([], 0, "not json", ""),
                       subprocess.TimeoutExpired("pixel", 60)):
            call = mock.patch("subprocess.run", side_effect=result) if isinstance(result, subprocess.TimeoutExpired) \
                else mock.patch("subprocess.run", return_value=result)
            with call as process:
                got = run("pixel", "repo", "prompt")
            self.assertTrue(got["failed"])
            self.assertEqual(got["brief"], "")
            self.assertGreaterEqual(got["elapsed_ms"], 0)
            self.assertIn("--parts", process.call_args.args[0])
        with mock.patch("subprocess.run", return_value=subprocess.CompletedProcess(
                [], 0, '{"brief":"answer a.rs:1:\\n  1| yes","tier":"high"}', "")):
            got = run("pixel", "repo", "prompt")
        self.assertFalse(got["failed"])
        self.assertEqual(got["brief"], "answer a.rs:1:\n  1| yes")
        self.assertEqual(got["tier"], "high")

    def test_fixture_mismatch_stops_before_any_pixel_invocation(self):
        with mock.patch("subprocess.run", return_value=subprocess.CompletedProcess(
                [], 0, "wrong-sha\n", "")) as process, \
                mock.patch.object(sys, "argv", ["bench-brief-spans.py", "--pixel", "pixel", "--repo", "repo"]), \
                contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            main()
        self.assertEqual(process.call_count, 1)
        self.assertEqual(process.call_args.args[0], ["git", "rev-parse", "HEAD"])

    def make_git_fixture(self, directory):
        repo = Path(directory)
        (repo / "source.rs").write_text("first line\nsecond line\n")
        subprocess.run(["git", "init", "--quiet", str(repo)], check=True)
        subprocess.run(["git", "-C", str(repo), "add", "--", "source.rs"], check=True)
        subprocess.run(["git", "-C", str(repo), "-c", "user.name=Fixture", "-c",
                        "user.email=fixture@example.com", "-c", "commit.gpgsign=false",
                        "commit", "--quiet", "-m", "fixture"], check=True)
        return repo

    def test_gold_preflight_rejects_every_bad_row_before_pixel(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = self.make_git_fixture(directory)
            (repo / "untracked.rs").write_text("not in git\n")
            rows = [
                {"id": "bg-test-missing", "shape": "symbol", "answer_spans": [
                    {"path": "missing.rs", "start_line": 1, "end_line": 1}]},
                {"id": "bg-test-untracked", "shape": "symbol", "answer_spans": [
                    {"path": "untracked.rs", "start_line": 1, "end_line": 1}]},
                {"id": "bg-test-out-of-bounds", "shape": "symbol", "answer_spans": [
                    {"path": "source.rs", "start_line": 1, "end_line": 3}]},
                {"id": "bg-test-malformed", "shape": "symbol", "answer_spans": [
                    {"path": "source.rs", "start_line": "1", "end_line": 2}]},
            ]
            prompts = directory + "/prompts.jsonl"
            spans = directory + "/answer_spans.jsonl"
            Path(prompts).write_text("\n".join(json.dumps({
                "id": row["id"], "text": "question", "split": "test",
            }) for row in rows) + "\n")
            Path(spans).write_text("\n".join(json.dumps(row) for row in rows) + "\n")
            arguments = ["bench-brief-spans.py", "--pixel", "pixel", "--repo", str(repo)]
            with mock.patch.object(sys, "argv", arguments), \
                    mock.patch.object(sys.modules[__name__], "PROMPTS", Path(prompts)), \
                    mock.patch.object(sys.modules[__name__], "SPANS", Path(spans)), \
                    mock.patch.object(sys.modules[__name__], "validate_fixture"), \
                    mock.patch.object(sys.modules[__name__], "run") as pixel, \
                    contextlib.redirect_stderr(io.StringIO()) as stderr, \
                    self.assertRaises(SystemExit) as raised:
                main()
            self.assertEqual(raised.exception.code, 2)
            pixel.assert_not_called()
            for question_id in ("bg-test-missing", "bg-test-untracked", "bg-test-out-of-bounds",
                                "bg-test-malformed"):
                self.assertIn(question_id, stderr.getvalue())
            self.assertIn("not a tracked regular source file", stderr.getvalue())
            self.assertIn("exceeds source.rs line count 2", stderr.getvalue())
            self.assertIn("positive ordered integers", stderr.getvalue())

    def test_gold_preflight_accepts_valid_ranges_and_records_input_hashes(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = self.make_git_fixture(directory)
            prompt_bytes = b'{"id":"bg-test-valid","text":"question","split":"dev"}\n'
            span_bytes = (b'{"id":"bg-test-valid","shape":"symbol","answer_spans":'
                          b'[{"path":"source.rs","start_line":1,"end_line":2}]}\n')
            prompts = Path(directory) / "prompts.jsonl"
            spans = Path(directory) / "answer_spans.jsonl"
            prompts.write_bytes(prompt_bytes)
            spans.write_bytes(span_bytes)
            output = Path(directory) / "result.json"
            arguments = ["bench-brief-spans.py", "--pixel", "pixel", "--repo", str(repo),
                         "--out", str(output)]
            with mock.patch.object(sys, "argv", arguments), \
                    mock.patch.object(sys.modules[__name__], "PROMPTS", prompts), \
                    mock.patch.object(sys.modules[__name__], "SPANS", spans), \
                    mock.patch.object(sys.modules[__name__], "validate_fixture"), \
                    mock.patch.object(sys.modules[__name__], "run", return_value={
                        "brief": "", "tier": None, "elapsed_ms": 1,
                        "failed": False, "error": None,
                    }) as pixel, \
                    contextlib.redirect_stdout(io.StringIO()):
                main()
            pixel.assert_called_once()
            result = json.loads(output.read_text())
            self.assertEqual(result["prompts_sha256"], hashlib.sha256(prompt_bytes).hexdigest())
            self.assertEqual(result["answer_spans_sha256"], hashlib.sha256(span_bytes).hexdigest())
            self.assertEqual(result["summary"]["n"], 1)

    def test_relative_binary_paths_use_the_launch_directory(self):
        self.assertEqual(resolve_pixel("target/dev-release/pixel", "/work/repo"),
                         "/work/repo/target/dev-release/pixel")
        self.assertEqual(resolve_pixel("./pixel", "/work/repo"), "/work/repo/pixel")
        with mock.patch("shutil.which", return_value="/bin/pixel"):
            self.assertEqual(resolve_pixel("pixel", "/work/repo"), "/bin/pixel")


if __name__ == "__main__":
    main()
