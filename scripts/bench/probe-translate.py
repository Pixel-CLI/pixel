#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Translate the vanilla trace (issue #906): what would each tool call of a session without a brief have needed?

Plain Codex, no Pixel hook, answers the question with tool calls. Each call is evidence it did not have, and most
have a deterministic Pixel equivalent:

    sed -n a,bp file / cat / head / tail   the region (or the enclosing symbol)
    rg / grep PATTERN                      the matching lines grouped by file, or the located definition
    cat README / docs                      a documentation section
    git log / show / diff                  a history block
    --help                                 the CLI definition region

This prints, per call: its translation, whether the current brief already carries it (yes / buried at line N /
partly / no) and which probe of ``probes/`` would (run and merged for the question), or "no probe: candidate".
That table is step 2 of the probe skill (``.agents/skills/probe``) and how new generic probes are found.

    probe-translate.py <question-id> [--bin PIXEL] [--fresh N] [--questions Q.jsonl | --census-db DB] [--root ROOT]
    probe-translate.py --self-test

The question is one of the public bench (``eval/brief-bench``, default) or of a local census (``--census-db``). The
calls come from the newest recorded no-brief stream under ``<root>/zero-calls`` (``brief-check.py``'s
``recorded_traces``) and, when there is none, from the question's bench baseline (the lines and commits each call
fetched); ``--fresh N`` first runs plain Codex N more times (``brief-zero-calls.py --arm nobrief``). Only counts,
paths and line ranges are printed: recorded commands and outputs are private. Standard library only; ``python3 -I``.
"""

import argparse
import importlib.util
import json
import re
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
WORKTREE = HERE.parent
BURIED_AFTER = 80      # a block whose header is further down than this many lines of the brief reads as buried
COVERED = 0.8


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, HERE / filename)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


def kind_of(oracle, command):
    """What a call looked for: ``help`` / ``history`` / ``doc`` / ``region`` / ``search`` / ``other``."""
    if re.search(r"--help\b|\s-h\b", command):
        return "help"
    if oracle.commits or re.search(r"\bgit\s+(-C\s+\S+\s+)?(log|show|diff|blame)\b", command):
        return "history"
    sources = set().union(*oracle.files.values()) if oracle.files else set()
    read = [p for p, src in oracle.files.items() if "read" in src]
    docs = [p for p in read if p.lower().endswith((".md", ".mdx", ".rst", ".txt"))]
    if docs and len(docs) == len(read):
        return "doc"
    if "read" in sources:
        return "region"
    if "search" in sources:
        return "search"
    return "other"


TRANSLATION = {
    "help": "the CLI definition region",
    "history": "a history block (commit, message, patch)",
    "doc": "a documentation section",
    "region": "the region (or the enclosing symbol)",
    "search": "the matching lines grouped by file, or the located definition",
    "other": "no deterministic equivalent",
}


def target_of(oracle, kind):
    if kind == "history":
        return "commits " + (", ".join(sorted(oracle.commits)[:3]) or "-") + (
            f" +{len(oracle.commits) - 3}" if len(oracle.commits) > 3 else "")
    ranges = sorted({(r["file"], r["lo"], r["hi"]) for r in oracle.regions}, key=lambda r: -(r[2] - r[1]))
    if kind == "search":
        files = sorted({r[0] for r in ranges})
        return f"{len(oracle.lines)} hit line(s) in {len(files)} file(s)" + (f", e.g. {files[0]}" if files else "")
    if ranges:
        path, lo, hi = ranges[0]
        return f"{path}:{lo}-{hi}" + (f" (+{len(ranges) - 1} more)" if len(ranges) > 1 else "")
    return "-"


def status_of(oracle, regions, brief_text, kind):
    """``yes`` / ``buried at line N`` / ``partly X%`` / ``no``: is what the call fetched in the brief."""
    if kind == "history":
        got = [c for c in oracle.commits if any(h.startswith(c) for h in regions.hex)]
        return "yes" if oracle.commits and len(got) == len(oracle.commits) else (
            f"partly {len(got)}/{len(oracle.commits)}" if got else "no")
    if not oracle.lines:
        return "no"
    share = sum(1 for k in oracle.lines if k in regions.inline) / len(oracle.lines)
    if share < 0.05:
        return "no"
    if share < COVERED:
        return f"partly {share:.0%}"
    paths = {k[0] for k in oracle.lines if k in regions.inline}
    lines = brief_text.splitlines()
    first = min((i + 1 for i, line in enumerate(lines) if line.startswith("### ") and any(p in line for p in paths)),
                default=1)
    return "yes" if first <= BURIED_AFTER else f"buried at line {first}"


def covering_probes(sb, repo, pt, probes, question, call_oracle, kind, binary):
    """Probes whose merged brief quotes this call's target while the brief alone does not."""
    names = []
    for probe, merged in probes:
        regions = sb.parse_brief(merged, repo)
        if kind == "history":
            ok = bool(call_oracle.commits) and all(any(h.startswith(c) for h in regions.hex) for c in call_oracle.commits)
        else:
            ok = bool(call_oracle.lines) and sum(1 for k in call_oracle.lines if k in regions.inline) >= COVERED * len(
                call_oracle.lines)
        if ok:
            names.append(probe.name)
    return names


def table(rows):
    out = ["| # | arm | looked for | translation | in the current brief | covered by |", "|---|---|---|---|---|---|"]
    for r in rows:
        out.append(f"| {r['n']} | {r['arm']} | {r['kind']}: {r['target']} | {TRANSLATION[r['kind']]} | {r['status']} | "
                   f"{', '.join(r['probes']) if r['probes'] else 'no probe: candidate' if r['status'] != 'yes' else '-'} |")
    return "\n".join(out)


def fresh_runs(args, bc, qid, count):
    """Run plain Codex ``count`` more times with no hook; the streams land in ``hyp-translate-*`` run directories."""
    stamp = int(time.time())
    for i in range(count):
        source_args = (["--census-db", args.census_db] if args.census_db else
                       ["--questions", args.questions] if args.questions else [])
        subprocess.run([sys.executable, "-I", str(HERE / "brief-zero-calls.py"), "--brief-bin", args.bin, "--agent",
                        args.agent, "--arm", "nobrief", "--only", qid, "--jobs", "1", "--root", args.root, "--run-id",
                        f"hyp-translate-{stamp}-{i}", *source_args], capture_output=True, text=True)


def traces_of(args, bc, qid, source=None):
    """``[(label, calls)]``: the newest recorded no-brief stream (else the question's bench baseline) plus every
    fresh ``hyp-translate-*`` one."""
    out = []
    recorded = bc.recorded_traces(args.root, qid).get("nobrief")
    if recorded:
        out.append((f"recorded {recorded[0]}", recorded[1]))
    elif source is not None and source.baseline_trace(qid):
        out.append(source.baseline_trace(qid))
    for path in sorted((Path(args.root) / "zero-calls").glob(f"hyp-translate-*/raw/{qid}.nobrief.stream.jsonl")):
        text = path.read_text(encoding="utf-8", errors="replace")
        if '"turn.completed"' in text:
            out.append((f"fresh {path.parents[1].name[-12:]}", bc.codex_calls(text.splitlines())))
    return out


def translate(bc, sb, pt, repo, clone, prompt, text, meta, traces, binary):
    """Rows of the table for ``traces`` (``[(label, calls)]``) against the brief ``text`` of ``prompt`` in ``clone``:
    the probes of ``probes/`` are run and merged for the question to say which one covers a call."""
    regions = sb.parse_brief(text, repo)
    gate_open = (meta.get("gate") or "open") == "open"
    merged = []
    home = tempfile.mkdtemp(prefix="translate-home-")
    for directory in sorted(d for d in (WORKTREE / "probes").glob("*/") if not d.name.startswith(("_", "."))):
        probe = pt.Probe(directory)
        if probe.problems or not gate_open or not probe.applies(meta.get("kind"), prompt):
            continue
        request = {"prompt": prompt, "repo_root": str(clone), "brief": text or "[PIXEL:BRIEF]\n",
                   "kind": meta.get("kind"), "session_files": []}
        stdout, _, error = pt.run_probe(probe, request, binary, home)
        if not error and stdout.strip():
            merged.append((probe, pt.merge(text or "[PIXEL:BRIEF]\n", [(probe.name, probe.mode, stdout)])))
    rows = []
    for label, calls in traces:
        for n, call in enumerate(calls, 1):
            oracle = bc.call_oracle(sb, repo, call)
            kind = kind_of(oracle, bc.call_command(call))
            status = status_of(oracle, regions, text, kind)
            rows.append({"n": n, "arm": label, "kind": kind, "target": target_of(oracle, kind), "status": status,
                         "probes": [] if status == "yes" else covering_probes(sb, repo, pt, merged, None, oracle, kind,
                                                                             binary)})
    return rows


def summary_line(rows):
    open_rows = [r for r in rows if r["status"] != "yes" and not r["probes"]]
    return (f"{sum(1 for r in rows if r['status'] == 'yes')} already in the brief, "
            f"{sum(1 for r in rows if r['status'] != 'yes' and r['probes'])} covered by a probe, "
            f"{len(open_rows)} with no probe: "
            + (", ".join(f"{r['arm'].split()[0]}#{r['n']} {r['kind']}" for r in open_rows) or "-"))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("question", nargs="?")
    parser.add_argument("--bin", default=str(WORKTREE / "target/dev-release/pixel"))
    parser.add_argument("--census-dir", default=None, help="scripts/tool-census (default: PIXEL_CENSUS_DIR, else beside this script)")
    load("brief_check", "brief-check.py").sources(None).add_arguments(parser)
    parser.add_argument("--fresh", type=int, default=0, help="run the agent with no hook this many more times first")
    parser.add_argument("--agent", choices=("codex", "claude"), default=None,
                        help="the agent of --fresh runs (default: claude under Claude Code, else codex)")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if args.self_test:
        return 0 if unittest.TextTestRunner(verbosity=2).run(
            unittest.defaultTestLoader.loadTestsFromTestCase(SelfTest)).wasSuccessful() else 1
    if not args.question:
        parser.error("a question id")
    bc = load("brief_check", "brief-check.py")
    pt = load("probe_test", "probe-test.py")
    sb = bc.load_module("score_brief", args.census_dir)
    source = bc.sources(args.census_dir).open_source(args)
    sb.add_workspace_root(args.root)
    args.bin = str(Path(args.bin).resolve())
    args.agent = args.agent or bc.default_agent()
    if args.fresh:
        fresh_runs(args, bc, args.question, args.fresh)
    repo_name, prompt, clone = bc.prepare(source, Path(args.root), args.question, args.bin)
    text, meta, _ = bc.run_brief(args.bin, clone, prompt)
    traces = traces_of(args, bc, args.question, source)
    if not traces:
        print(f"# {args.question}: no recorded trace (no stream under {args.root}/zero-calls, no bench baseline); "
              "run `brief-zero-calls.py --arm nobrief` or pass --fresh N")
        return 1
    rows = translate(bc, sb, pt, sb.Repo(clone), clone, prompt, text, meta, traces, args.bin)
    print(f"# {args.question} ({meta.get('kind')}): brief {len(text.encode())} B, {len(rows)} no-brief call(s) translated\n")
    print(table(rows))
    print("\n" + summary_line(rows))
    return 0


class SelfTest(unittest.TestCase):
    class Oracle:
        def __init__(self, lines=(), commits=(), files=None, regions=()):
            self.lines = {k: "definition" for k in lines}
            self.commits = set(commits)
            self.files = files or {}
            self.regions = [{"file": f, "lo": lo, "hi": hi} for f, lo, hi in regions]

    class Regions:
        def __init__(self, inline=(), hex_=()):
            self.inline = set(inline)
            self.hex = set(hex_)

    def test_a_call_is_classified_by_what_it_looked_for(self):
        read = self.Oracle(lines=[("a.rs", 1)], files={"a.rs": {"read"}}, regions=[("a.rs", 1, 1)])
        self.assertEqual(kind_of(read, "sed -n 1,1p a.rs"), "region")
        doc = self.Oracle(lines=[("README.md", 1)], files={"README.md": {"read"}})
        self.assertEqual(kind_of(doc, "sed -n 1,9p README.md"), "doc")
        search = self.Oracle(lines=[("a.rs", 3)], files={"a.rs": {"search"}})
        self.assertEqual(kind_of(search, "rg -n foo ."), "search")
        self.assertEqual(kind_of(self.Oracle(commits=["abc1234"]), "git show abc1234"), "history")
        self.assertEqual(kind_of(self.Oracle(), "rg --help | head"), "help")
        self.assertEqual(kind_of(self.Oracle(), "yarn test"), "other")

    def test_the_status_says_yes_buried_partly_or_no(self):
        lines = [("a.rs", n) for n in range(1, 11)]
        oracle = self.Oracle(lines=lines)
        head = "\n".join(["[PIXEL:BRIEF]"] + ["### a.rs:1-10 — x"] + ["1| x"] * 5)
        self.assertEqual(status_of(oracle, self.Regions(inline=lines), head, "region"), "yes")
        deep = "\n".join(["x"] * 120 + ["### a.rs:1-10 — x"])
        self.assertEqual(status_of(oracle, self.Regions(inline=lines), deep, "region"), "buried at line 121")
        self.assertEqual(status_of(oracle, self.Regions(inline=lines[:5]), head, "region"), "partly 50%")
        self.assertEqual(status_of(oracle, self.Regions(), head, "region"), "no")
        commits = self.Oracle(commits=["abc1234", "def5678"])
        self.assertEqual(status_of(commits, self.Regions(hex_={"abc1234ffff"}), "", "history"), "partly 1/2")

    def test_the_table_names_the_probe_or_a_candidate(self):
        rows = [{"n": 1, "arm": "recorded r", "kind": "doc", "target": "README.md:50-110", "status": "no", "probes": []},
                {"n": 2, "arm": "recorded r", "kind": "history", "target": "commits abc", "status": "no",
                 "probes": ["history-fallback"]},
                {"n": 3, "arm": "recorded r", "kind": "region", "target": "a.rs:1-9", "status": "yes", "probes": []}]
        text = table(rows)
        self.assertIn("a documentation section | no | no probe: candidate", text)
        self.assertIn("| no | history-fallback |", text)
        self.assertIn("| yes | - |", text)

    def test_a_bench_baseline_trace_is_translated_without_any_recorded_stream(self):
        bc, pt = load("brief_check", "brief-check.py"), load("probe_test", "probe-test.py")
        sb = bc.load_module("score_brief", None)
        with tempfile.TemporaryDirectory() as tmp:
            questions, _sha = bc.sources().fixture_bench(tmp)
            source = bc.sources().BenchSource(questions)
            args = type("Args", (), {"root": tmp, "census_db": None, "questions": None})()
            traces = traces_of(args, bc, "t-01", source)
            self.assertEqual([label for label, _ in traces], ["bench baseline (1 run)"])
            clone = Path(tmp) / "origin"
            brief = "[PIXEL:BRIEF]\n### src/lib.py:5-9 — lines\n" + "".join(f"{n}| line_{n} = {n}\n" for n in range(5, 10))
            rows = translate(bc, sb, pt, sb.Repo(clone), clone, "where is line_7", brief, {"gate": "closed"}, traces, "pixel")
            self.assertEqual([(r["kind"], r["target"], r["status"]) for r in rows],
                             [("region", "src/lib.py:5-9", "yes"), ("other", "-", "no")])
            self.assertTrue(rows[1]["status"] != "yes" and rows[1]["probes"] == [])
            self.assertEqual(traces_of(type("Args", (), {"root": tmp})(), bc, "t-02", source), [])


if __name__ == "__main__":
    sys.exit(main())
