#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""The public brief bench (issue #906): answer-only questions on pinned public repositories.

``eval/brief-bench/questions.jsonl`` holds one question per line, ``{id, repo, url, sha, prompt, kind, oracle,
oracle_lines}``: written for the bench (the census prompts are private and stay local), each pinned to a commit.
``baselines.jsonl`` holds the aggregates of plain Codex with no Pixel hook, recorded once: ``{id, runs, n_calls,
calls}`` where a call is ``{reads: [[path, lo, hi]], commits: [sha7]}``, the lines it fetched. ``oracle`` is the
union of those lines, the evidence a session needed. Nothing else of a session is kept: no command, no output.

    brief-bench.py check                       lint the bench (CI): count, ids, pins, fields, baselines
    brief-bench.py prepare [--only ids]        clone the pinned repositories into the cache
    brief-bench.py baseline [--only ids] [--jobs 8] [--runs 1]
                                               run plain Codex on each question, record baselines and oracles
    brief-bench.py --self-test

``baseline`` needs the runner and scorer (``scripts/tool-census``, in this repository) and a Codex login (copied to a
disposable CODEX_HOME per session, never printed). The bench is the default question source of every zero-call
tool (``scripts/bench/tool-census/question_source.py``); ``check`` also refuses a question whose prompt is in a local
census (``--census-db``, else ``<root>/census.db`` when it exists). The clones live in ``$PIXEL_BENCH_CACHE``
(default ``~/.cache/pixel-brief-bench``). Standard library only; run with ``python3 -I``.
"""

import argparse
import concurrent.futures
import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
WORKTREE = HERE.parents[1]
BENCH = WORKTREE / "eval" / "brief-bench"
CACHE = Path(os.environ.get("PIXEL_BENCH_CACHE") or Path.home() / ".cache" / "pixel-brief-bench")
MIN_QUESTIONS = 50
KINDS = ("overview", "lookup", "config", "flow", "history", "tests", "error-origin", "impact", "diagnosis")
FIELDS = ("id", "repo", "url", "sha", "prompt", "kind", "oracle", "oracle_lines")


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, HERE / filename)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


def read_jsonl(path):
    path = Path(path)
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()] if path.exists() else []


def write_jsonl(path, rows):
    Path(path).write_text("".join(json.dumps(r, ensure_ascii=False) + "\n" for r in rows))


# --------------------------------------------------------------------------- lint

def lint(questions, baselines, census_prompts=()):
    """Problems of the bench as a list of strings; empty when it is sound."""
    problems = []
    if len(questions) < MIN_QUESTIONS:
        problems.append(f"{len(questions)} questions, at least {MIN_QUESTIONS} are required")
    seen, prompts = set(), set()
    repos = {}
    for q in questions:
        qid = q.get("id", "?")
        for field in FIELDS:
            if field not in q:
                problems.append(f"{qid}: missing `{field}`")
        if qid in seen:
            problems.append(f"{qid}: duplicate id")
        seen.add(qid)
        if not re.fullmatch(r"[0-9a-f]{40}", str(q.get("sha", ""))):
            problems.append(f"{qid}: sha must be a 40-hex commit")
        if q.get("kind") not in KINDS:
            problems.append(f"{qid}: kind `{q.get('kind')}` is not one of {KINDS}")
        if not str(q.get("url", "")).startswith("https://github.com/"):
            problems.append(f"{qid}: url must be a public https://github.com/ repository")
        prompt = str(q.get("prompt", ""))
        if len(prompt) < 20:
            problems.append(f"{qid}: prompt is too short")
        if prompt in prompts:
            problems.append(f"{qid}: duplicate prompt")
        prompts.add(prompt)
        if prompt in census_prompts:
            problems.append(f"{qid}: this prompt is a private census prompt")
        if repos.setdefault(q.get("repo"), q.get("sha")) != q.get("sha"):
            problems.append(f"{qid}: {q.get('repo')} is pinned at two different commits")
        if q.get("oracle_lines") != sum(hi - lo + 1 for _p, lo, hi in q.get("oracle", [])):
            problems.append(f"{qid}: oracle_lines does not match oracle")
    if len(repos) < 7:
        problems.append(f"{len(repos)} repositories, at least 7 are required")
    by_id = {b.get("id"): b for b in baselines}
    for qid in by_id:
        if qid not in seen:
            problems.append(f"baseline for unknown question {qid}")
    for b in baselines:
        for call in b.get("calls", []):
            if set(call) - {"tool", "reads", "commits"}:
                problems.append(f"{b.get('id')}: a call holds more than tool, reads and commits (aggregates only)")
    return problems


# --------------------------------------------------------------------------- oracle from a recorded run

def merge_ranges(keys):
    """``[(path, n)]`` -> sorted ``[[path, lo, hi]]`` of contiguous lines."""
    out = []
    for path in sorted({p for p, _ in keys}):
        lines = sorted(n for p, n in keys if p == path)
        lo = prev = lines[0]
        for n in lines[1:]:
            if n != prev + 1:
                out.append([path, lo, prev])
                lo = n
            prev = n
        out.append([path, lo, prev])
    return out


MAX_CALL_RANGES, MAX_ORACLE_SINGLES = 40, 100


def compact(ranges, limit):
    """At most ``limit`` ranges: every range of three lines or more first (largest first), then single lines, in path
    order. A search over a whole repository would otherwise store thousands of one-line hits."""
    spans = sorted((r for r in ranges if r[2] - r[1] >= 2), key=lambda r: (-(r[2] - r[1]), r[0], r[1]))
    singles = [r for r in ranges if r[2] - r[1] < 2]
    kept = (spans + singles)[:limit]
    return sorted(kept, key=lambda r: (r[0], r[1]))


def aggregate(sb, repo, calls):
    """The aggregates of one run: per call its fetched ranges and commits, and the union of the lines."""
    summary, union = [], set()
    for call in calls:
        oracle = bc_module().call_oracle(sb, repo, call)
        union |= set(oracle.lines)
        summary.append({"tool": "sh", "reads": compact(merge_ranges(set(oracle.lines)), MAX_CALL_RANGES),
                        "commits": sorted(oracle.commits)})
    return summary, compact(merge_ranges(union), MAX_ORACLE_SINGLES + 60)


# --------------------------------------------------------------------------- commands

def src_dir(question):
    return CACHE / "src" / question["repo"].replace("/", "_")


def prepare(questions):
    for repo in sorted({(q["repo"], q["url"], q["sha"]) for q in questions}):
        slug, url, sha = repo
        dest = src_dir({"repo": slug})
        if not (dest / ".git").exists():
            dest.parent.mkdir(parents=True, exist_ok=True)
            subprocess.run(["git", "clone", "-q", url, str(dest)], check=True)
        have = subprocess.run(["git", "rev-parse", "HEAD"], cwd=dest, capture_output=True, text=True).stdout.strip()
        if have != sha:
            subprocess.run(["git", "checkout", "-q", "--detach", sha], cwd=dest, check=True)
        print(f"{slug} at {sha[:8]}")


def final_answer(lines):
    """The last assistant message of a ``codex exec --json`` stream."""
    answer = ""
    for line in lines:
        try:
            rec = json.loads(line)
        except ValueError:
            continue
        item = rec.get("item") or {}
        if rec.get("type") == "item.completed" and item.get("type") == "agent_message":
            answer = str(item.get("text") or "")
    return answer


def run_codex(args, questions, runs, binary=None, src=None, tag="bench"):
    """Run the agent (``args.agent``: ``codex`` or ``claude``) on every question ``runs`` times, in parallel, one clone per session.

    ``binary`` None: no Pixel hook at all (the vanilla session). Otherwise the hooks of ``pixel setup`` run that
    binary on a clone that ``activate`` has indexed. ``src(question)`` is the directory to clone from. Returns
    ``[(question id, run, record or None)]`` where a record is ``{n_calls, calls (cmd/out, private), answer,
    brief_bytes, brief_ms}``; the caller keeps aggregates only."""
    bzc = load("brief_zero_calls", "brief-zero-calls.py")
    census = bzc.load_census(args.census_dir)
    shim = census.make_shim_dir(Path(args.root))
    work = Path(args.root) / "work"
    work.mkdir(parents=True, exist_ok=True)
    src = src or src_dir
    out, lock = [], threading.Lock()

    def one(question, run):
        try:
            return one_session(question, run)
        except (OSError, subprocess.SubprocessError, ValueError) as error:
            # One failed session (a copy, an index, a login) is one missing record, not a lost baseline.
            print(f"{question['id']} run {run}: {type(error).__name__}", flush=True)
            return question["id"], run, None

    def one_session(question, run):
        ws = Path(tempfile.mkdtemp(prefix=f"{tag}-{question['id']}-{run}-", dir=str(work)))
        shutil.rmtree(ws)
        subprocess.run(["cp", "-c", "-R", str(src(question)), str(ws)], check=True)
        shutil.rmtree(ws / ".pixel", ignore_errors=True)
        home = None
        try:
            env = bzc.clean_env()
            if binary:
                bzc.activate(binary, ws, env, True)
            if getattr(args, "agent", "codex") == "claude":
                # claude -p with a per-run settings file that registers the hooks (none for the vanilla arm); the
                # login is the user's OAuth/keychain one, ANTHROPIC_API_KEY never reaches the child, and the real
                # HOME's settings are not touched.
                settings = Path(tempfile.mkdtemp(prefix="bench-claude-", dir=str(work))) / "settings.json"
                body = bzc.settings_body(binary, 3) if binary else {"hooks": {}}
                settings.write_text(json.dumps(body) + "\n")
                env = census.agent_env(os.environ, shim)
                argv = census.claude_argv(census.CLAUDE, settings, args.claude_model, args.claude_effort, 2.0)
                lines, _err, code, timed_out, _s = census.run_claude(argv, question["prompt"], ws, env, args.timeout, None)
                text = [x for _, x in lines]
                stream = bzc.parse_stream(text)
                if timed_out or code != 0 or not stream["result"]:
                    return question["id"], run, None
                answer = str((stream["result"] or {}).get("result") or "")
                calls, n_calls = bc_module().claude_calls(text), len(stream["calls"])
                shutil.rmtree(settings.parent, ignore_errors=True)
            else:
                home = Path(tempfile.mkdtemp(prefix="bench-codex-home-", dir=str(work)))
                bzc.make_codex_home(home, binary or "pixel", 3, args.model, args.effort, ws, hooks=bool(binary))
                env = dict(census.agent_env(os.environ, shim), CODEX_HOME=str(home))
                argv = bzc.codex_argv(bzc.CODEX, ws, args.model, args.effort)
                lines, _err, code, timed_out, _s = census.run_claude(argv, question["prompt"], ws, env, args.timeout, None)
                text = [x for _, x in lines]
                stream = bzc.parse_codex_stream(text)
                if timed_out or code != 0 or not stream["completed"]:
                    return question["id"], run, None
                answer, calls, n_calls = final_answer(text), bzc_calls(bzc, text), len(stream["calls"])
            fb = bzc.feedback_of(ws) if binary else {"part_bytes": 0}
            return question["id"], run, {"n_calls": n_calls, "calls": calls, "answer": answer,
                                         "brief_bytes": fb["part_bytes"],
                                         "brief_ms": bzc.brief_ms_of(ws) if binary else None, "ws": None}
        finally:
            if binary:
                subprocess.run([binary, "daemon", "stop", "."], cwd=ws, capture_output=True)
            if home is not None:
                shutil.rmtree(home, ignore_errors=True)
            shutil.rmtree(ws, ignore_errors=True)

    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
        futures = [pool.submit(one, q, r) for q in questions for r in range(runs)]
        for fut in concurrent.futures.as_completed(futures):
            qid, run, rec = fut.result()
            with lock:
                out.append((qid, run, rec))
                print(f"{qid} run {run}: " + ("failed" if rec is None else f"{rec['n_calls']} calls"), flush=True)
    return out


def oracle_of(sb, repo, record):
    """The evidence a session cited: the ``path:line`` mentions of its final answer, else the lines it read."""
    cited = sb.cited_lines(record["answer"], repo)
    if cited:
        return compact(merge_ranges(cited), MAX_ORACLE_SINGLES + 60)
    return aggregate(sb, repo, record["calls"])[1]


def baseline(args, questions):
    sb = load_score(args.census_dir)
    prepare(questions)
    results = {}
    for qid, run, rec in run_codex(args, questions, args.runs):
        results.setdefault(qid, []).append((run, rec))
    all_q = read_jsonl(BENCH / "questions.jsonl")
    base = {b["id"]: b for b in read_jsonl(BENCH / "baselines.jsonl")}
    for q in all_q:
        runs = [(r, rec) for r, rec in results.get(q["id"], []) if rec]
        if not runs:
            continue
        runs.sort(key=lambda t: t[1]["n_calls"])
        _run, pick = runs[len(runs) // 2]
        repo = sb.Repo(src_dir(q))
        summary, union = aggregate(sb, repo, pick["calls"])
        oracle = oracle_of(sb, repo, pick)
        q["oracle"], q["oracle_lines"] = oracle, sum(h - l + 1 for _p, l, h in oracle)
        base[q["id"]] = {"id": q["id"], "runs": len(runs), "n_calls": [rec["n_calls"] for _r, rec in runs],
                         "calls": summary}
    write_jsonl(BENCH / "questions.jsonl", all_q)
    write_jsonl(BENCH / "baselines.jsonl", [base[k] for k in sorted(base)])


def bc_module():
    return load("brief_check", "brief-check.py")


def bzc_calls(bzc, text):
    bc = load("brief_check", "brief-check.py")
    return bc.codex_calls(text)


def load_score(census_dir):
    bc = load("brief_check", "brief-check.py")
    return bc.load_module("score_brief", census_dir)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("command", nargs="?", choices=("check", "prepare", "baseline"))
    parser.add_argument("--only", default=None)
    parser.add_argument("--jobs", type=int, default=8)
    parser.add_argument("--runs", type=int, default=1)
    parser.add_argument("--timeout", type=int, default=900)
    parser.add_argument("--agent", choices=("codex", "claude"), default="codex")
    parser.add_argument("--model", default="gpt-5.6-terra")
    parser.add_argument("--effort", default="medium")
    parser.add_argument("--claude-model", default=None)
    parser.add_argument("--claude-effort", default=None)
    parser.add_argument("--root", default=None, help="state directory (default: $PIXEL_CENSUS_ROOT, else $TMPDIR/pixel-tool-census)")
    parser.add_argument("--census-db", default=None, help="check: a local census.db whose prompts the bench must not contain")
    parser.add_argument("--census-dir", default=None, help="scripts/tool-census (default: PIXEL_CENSUS_DIR, else beside this script)")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if args.self_test:
        return 0 if unittest.TextTestRunner(verbosity=2).run(
            unittest.defaultTestLoader.loadTestsFromTestCase(SelfTest)).wasSuccessful() else 1
    if not args.command:
        parser.error("a command")
    args.root = args.root or str(bc_module().sources(args.census_dir).default_root())
    questions = read_jsonl(BENCH / "questions.jsonl")
    if args.only:
        wanted = set(args.only.split(","))
        selected = [q for q in questions if q["id"] in wanted]
    else:
        selected = questions
    if args.command == "check":
        census = set()
        db = Path(args.census_db) if args.census_db else Path(args.root) / "census.db"
        if db.exists():
            import sqlite3
            con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
            census = {r[0] for r in con.execute("SELECT prompt FROM sessions")}
            con.close()
        problems = lint(questions, read_jsonl(BENCH / "baselines.jsonl"), census)
        for problem in problems:
            print(problem)
        print(f"{len(questions)} questions on {len({q['repo'] for q in questions})} repositories, "
              f"{len(read_jsonl(BENCH / 'baselines.jsonl'))} baselines: " + ("ok" if not problems else f"{len(problems)} problem(s)"))
        return 1 if problems else 0
    if args.command == "prepare":
        prepare(selected)
        return 0
    baseline(args, selected)
    return 0


class SelfTest(unittest.TestCase):
    def question(self, **extra):
        base = {"id": "r-01", "repo": "o/r", "url": "https://github.com/o/r.git", "sha": "a" * 40,
                "prompt": "give me an overview of the modules", "kind": "overview", "oracle": [["a.py", 1, 4]],
                "oracle_lines": 4}
        return {**base, **extra}

    def test_merge_ranges_joins_contiguous_lines_per_file(self):
        keys = {("b.py", 5), ("a.py", 3), ("a.py", 1), ("a.py", 2), ("b.py", 9)}
        self.assertEqual(merge_ranges(keys), [["a.py", 1, 3], ["b.py", 5, 5], ["b.py", 9, 9]])

    def test_compact_keeps_the_spans_and_caps_the_single_line_hits(self):
        ranges = [["a", n, n] for n in range(1, 50, 2)] + [["b", 1, 30], ["c", 5, 9]]
        kept = compact(ranges, 5)
        self.assertEqual(len(kept), 5)
        self.assertIn(["b", 1, 30], kept)
        self.assertIn(["c", 5, 9], kept)
        self.assertEqual(kept, sorted(kept, key=lambda r: (r[0], r[1])))

    def test_lint_names_each_problem_of_a_bad_bench(self):
        good = [self.question(id=f"r-{i:02d}", prompt=f"question number {i} about the code") for i in range(50)]
        good += [self.question(id=f"s{i}", repo=f"o/s{i}", prompt=f"another question {i} about the code") for i in range(6)]
        self.assertEqual(lint(good, []), [])
        bad = [self.question(sha="main", kind="vibes", url="http://x", prompt="short", oracle_lines=9)]
        text = " | ".join(lint(bad, [{"id": "ghost", "calls": [{"tool": "sh", "reads": [], "commits": [], "cmd": "ls"}]}]))
        for needle in ("at least 50", "40-hex", "kind `vibes`", "public https://github.com/", "too short",
                       "oracle_lines does not match", "unknown question ghost", "aggregates only", "at least 7"):
            self.assertIn(needle, text)

    def test_a_private_census_prompt_is_refused(self):
        rows = [self.question()]
        self.assertTrue(any("private census prompt" in p for p in lint(rows, [], {rows[0]["prompt"]})))

    def test_two_pins_for_one_repository_are_refused(self):
        rows = [self.question(), self.question(id="r-02", prompt="a second question about the code", sha="b" * 40)]
        self.assertTrue(any("pinned at two different commits" in p for p in lint(rows, [])))

    def test_the_shipped_bench_passes_its_own_lint_when_it_is_there(self):
        questions = read_jsonl(BENCH / "questions.jsonl")
        if questions:
            self.assertEqual(lint(questions, read_jsonl(BENCH / "baselines.jsonl")), [])


if __name__ == "__main__":
    sys.exit(main())
