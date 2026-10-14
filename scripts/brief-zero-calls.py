#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Zero-call test of the prompt-submit brief (issue #906).

The tool census (#905) ran 20 prompts as vanilla Claude Code sessions; 13 of them are
answer-only questions (no edit), and the census oracle experiment showed they need no tool
call when the context the agent would have read is injected. This script runs those 13 questions
LIVE with Pixel's own brief instead of the oracle and counts the tool calls of each:

    pass                    0 calls: the brief carried everything the answer needed
    fell back to tool calls >= 1 call: something was missing from the brief

Per question it makes a pinned clone of the census repository, indexes it with the binary under
test (``prepare-repo``, one ``search-meaning``, throwaway briefs until the output settles, as
``score_brief.py`` does), registers the prompt-submit hooks the way ``pixel setup`` writes them for
Claude Code (``run-hook task-event --provider claude --event prompt-submit --part K/N``, N parts),
and runs ``claude -p`` with those hooks, the census tool set and a ``pixel`` that is visibly
absent from PATH, so the only Pixel the agent sees is the brief.

    brief-zero-calls.py --brief-bin <pixel> [--only id,id] [--parts 3] [--jobs 13]
    brief-zero-calls.py --self-test

The questions, the answers and the transcripts are private: the questions are read from
``census.db`` at run time, the raw streams stay under ``<root>/zero-calls/``, and the printed
and recorded report holds aggregates only (counts, tool names, bytes, seconds).

The census runner (``run.py``: pinned clone cache, env whitelist, shim, launch, quota guard) lives
with the census (``scripts/tool-census``); point ``--census-dir`` at it, or set
``PIXEL_CENSUS_DIR``. Standard library only; run with ``python3 -I``.
"""

import argparse
import concurrent.futures
import hashlib
import importlib.util
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import threading
import time
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
DEFAULT_ROOT = Path("/private/tmp/claude-501/census")
CENSUS_DIRS = (
    Path(os.environ.get("PIXEL_CENSUS_DIR", "/nonexistent")),
    HERE / "tool-census",
    Path("/Users/livio/Documents/pixel-wt/census/scripts/tool-census"),
)
PASS = "pass"
FELL_BACK = "fell back to tool calls"
HISTORY_BUDGET_S = 420
SETTLE_TRIES = 14
SETTLE_NEED = 3


def load_census(directory):
    """The census runner module (``run.py``), from ``directory`` or the first known place."""
    candidates = [Path(directory)] if directory else list(CENSUS_DIRS)
    for candidate in candidates:
        path = candidate / "run.py"
        if path.exists():
            spec = importlib.util.spec_from_file_location("census_run", path)
            module = importlib.util.module_from_spec(spec)
            sys.modules["census_run"] = module
            spec.loader.exec_module(module)
            return module
    raise SystemExit("census runner (scripts/tool-census/run.py) not found: pass --census-dir or set PIXEL_CENSUS_DIR")


def sh(argv, cwd=None, timeout=900, env=None):
    return subprocess.run([str(a) for a in argv], cwd=cwd, capture_output=True, text=True, timeout=timeout, env=env)


def clean_env():
    env = dict(os.environ)
    env.pop("ANTHROPIC_API_KEY", None)  # banned everywhere
    env.pop("PIXEL_BRIEF", None)        # the kill switch must not blank the brief under test
    return env


def answer_sessions(db_path, only=None):
    """``(id, repo, sha, prompt)`` of every successful answer-only census session."""
    db = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    rows = db.execute("SELECT id, repo, sha, prompt FROM sessions WHERE source='census' AND mode='answer' AND ok=1 "
                      "ORDER BY id").fetchall()
    db.close()
    return [r for r in rows if not only or r[0] in only]


def hook_command(binary, part, parts):
    """The command ``pixel setup`` registers for UserPromptSubmit part ``part`` of ``parts`` on Claude Code."""
    return (f"'{binary}' run-hook task-event --provider claude --event prompt-submit --part {part}/{parts} || true")


def settings_body(binary, parts):
    """Claude Code settings with one UserPromptSubmit hook per part (a single hook when ``parts`` is 1)."""
    if parts <= 1:
        commands = [f"'{binary}' run-hook task-event --provider claude --event prompt-submit || true"]
    else:
        commands = [hook_command(binary, k, parts) for k in range(1, parts + 1)]
    return {"hooks": {"UserPromptSubmit": [{"hooks": [{"type": "command", "command": c, "timeout": 10}
                                                      for c in commands]}]}}


def parse_stream(lines):
    """Tool calls (name only: the arguments are private) and hook events of one stream-json session."""
    calls, hooks, result = [], [], None
    for line in lines:
        try:
            rec = json.loads(line)
        except ValueError:
            continue
        kind = rec.get("type")
        if kind == "assistant" and not rec.get("parent_tool_use_id"):
            for part in (rec.get("message") or {}).get("content") or []:
                if part.get("type") == "tool_use" and part.get("id") not in {c["id"] for c in calls}:
                    calls.append({"id": part["id"], "tool": part.get("name", "?")})
        elif kind == "system" and rec.get("subtype") in ("hook_started", "hook_response"):
            hooks.append({"subtype": rec["subtype"], "event": rec.get("hook_event"), "outcome": rec.get("outcome"),
                          "chars": len(str(rec.get("stdout") or rec.get("output") or ""))})
        elif kind == "result":
            result = rec
    return {"calls": calls, "hooks": hooks, "result": result}


def verdict(n_calls):
    return PASS if n_calls == 0 else FELL_BACK


def is_promisor_clone(ws):
    """A blobless (partial) clone: it fetches the blobs it diffs from the network."""
    try:
        return "promisor" in (Path(ws) / ".git" / "config").read_text(errors="replace")
    except OSError:
        return False


def activate(binary, ws, env, history=True):
    """Index ``ws`` with the binary under test and settle the daemon, as ``score_brief.py`` does.

    ``history`` also builds the history index (``build-index --history``), which a question about
    when or by whom something changed is answered from; a user who asks such a question has it
    built, a fresh clone does not."""
    sh([binary, "prepare-repo", "."], cwd=ws, timeout=1800, env=env)
    if history and not is_promisor_clone(ws):
        # A blobless clone fetches every blob it diffs from the network: bounded, and
        # a question of history in such a repository is simply not answerable.
        try:
            sh([binary, "build-index", "--history", "."], cwd=ws, timeout=HISTORY_BUDGET_S, env=env)
        except subprocess.TimeoutExpired:
            pass
    sh([binary, "search-meaning", "how does this code work", "."], cwd=ws, timeout=1800, env=env)
    last, same = None, 0
    for _ in range(SETTLE_TRIES):
        out = sh([binary, "brief", "--metrics", "off", "where is the main entry point of this program defined", "."],
                 cwd=ws, timeout=600, env=env).stdout
        same = same + 1 if out == last else 1
        last = out
        if same >= SETTLE_NEED:
            return
        time.sleep(1)


def render(rows, label):
    lines = [f"# Zero-call test: {label}", "", "| question | repo | calls | tools | brief chars | result |",
             "|---|---|---:|---|---:|---|"]
    for row in rows:
        tools = ", ".join(f"{t} x{n}" for t, n in sorted(row["tools"].items())) or "-"
        lines.append(f"| {row['id']} | {row['repo']} | {row['n_calls']} | {tools} | {row['brief_chars']} | {row['verdict']} |")
    passed = sum(1 for r in rows if r["verdict"] == PASS)
    lines += ["", f"**{passed} of {len(rows)} answered with 0 tool calls**; "
                  f"{len(rows) - passed} {FELL_BACK}.", ""]
    return "\n".join(lines)


class Run:
    def __init__(self, args, census):
        self.args, self.census = args, census
        self.root = Path(args.root)
        self.out = self.root / "zero-calls" / args.run_id
        self.lock, self.rows = threading.Lock(), []
        self.shim = census.make_shim_dir(self.root)

    def unit(self, sid, repo, sha, prompt, total):
        census, args = self.census, self.args
        env = clean_env()
        ws = census.make_workspace(self.root, repo, f"zc-{args.run_id}-{sid}", sha)
        row = {"id": sid, "repo": repo, "sha": sha[:8], "n_calls": -1, "tools": {}, "brief_chars": 0,
               "verdict": "error", "problems": []}
        try:
            shutil.rmtree(Path(ws) / ".pixel", ignore_errors=True)
            activate(args.brief_bin, ws, env, not args.no_history)
            settings = self.out / "settings" / f"{sid}.json"
            settings.parent.mkdir(parents=True, exist_ok=True)
            settings.write_text(json.dumps(settings_body(args.brief_bin, args.parts)) + "\n", encoding="utf-8")
            argv = census.claude_argv(census.CLAUDE, settings, args.model, args.effort, args.budget)
            lines, err, code, timed_out, seconds = census.run_claude(
                argv, prompt, ws, census.agent_env(os.environ, self.shim), args.timeout, None)
            raw = self.out / "raw" / f"{sid}.stream.jsonl"
            raw.parent.mkdir(parents=True, exist_ok=True)
            raw.write_text("".join(x if x.endswith("\n") else x + "\n" for _, x in lines), encoding="utf-8")
            stream = parse_stream([x for _, x in lines])
            tools = {}
            for call in stream["calls"]:
                tools[call["tool"]] = tools.get(call["tool"], 0) + 1
            responses = [h for h in stream["hooks"] if h["subtype"] == "hook_response"]
            if len(responses) != args.parts:
                row["problems"].append(f"hooks:{len(responses)}/{args.parts}")
            if timed_out or code != 0 or not stream["result"]:
                row["problems"].append(f"session:exit{code}{'+timeout' if timed_out else ''}")
            row.update({"n_calls": len(stream["calls"]), "tools": tools,
                        "brief_chars": sum(h["chars"] for h in responses), "seconds": round(seconds, 1),
                        "verdict": verdict(len(stream["calls"])) if not row["problems"] else "error"})
        except (OSError, subprocess.SubprocessError) as error:
            row["problems"].append(type(error).__name__)
        finally:
            sh([args.brief_bin, "daemon", "stop", "."], cwd=ws, timeout=60, env=env)
            shutil.rmtree(ws, ignore_errors=True)
        with self.lock:
            self.rows.append(row)
            print(f"[{len(self.rows)}/{total}] {sid:<13} calls {row['n_calls']:>2}  {row['verdict']}"
                  f"{'  ' + ','.join(row['problems']) if row['problems'] else ''}", flush=True)
        return row


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--brief-bin", help="the pixel binary under test")
    parser.add_argument("--census-dir", default=None, help="scripts/tool-census of the census (default: PIXEL_CENSUS_DIR or known places)")
    parser.add_argument("--root", default=str(DEFAULT_ROOT), help="census root (cache/, census.db)")
    parser.add_argument("--run-id", default=time.strftime("zc-%Y%m%d-%H%M%S"))
    parser.add_argument("--only", default=None, help="comma-separated question ids")
    parser.add_argument("--parts", type=int, default=3, help="UserPromptSubmit hooks (brief_parts)")
    parser.add_argument("--jobs", "--concurrency", dest="jobs", type=int, default=1,
                        help="questions run in parallel, each in its own clone and output file (default 1)")
    parser.add_argument("--model", default=None)
    parser.add_argument("--effort", default=None)
    parser.add_argument("--budget", type=float, default=2.0)
    parser.add_argument("--timeout", type=int, default=900)
    parser.add_argument("--no-history", action="store_true", help="skip build-index --history")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(SelfTest)
        return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1
    if not args.brief_bin:
        parser.error("--brief-bin is required")
    args.brief_bin = str(Path(args.brief_bin).resolve())
    census = load_census(args.census_dir)
    root = Path(args.root)
    sessions = answer_sessions(root / "census.db", set(args.only.split(",")) if args.only else None)
    if not sessions:
        raise SystemExit("no answer-only census session selected")
    census.prepare_cache(root)
    run = Run(args, census)
    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, args.jobs)) as pool:
        for future in concurrent.futures.as_completed(
                [pool.submit(run.unit, sid, repo, sha, prompt, len(sessions)) for sid, repo, sha, prompt in sessions]):
            future.result()
    rows = sorted(run.rows, key=lambda r: r["id"])
    digest = hashlib.sha256(Path(args.brief_bin).read_bytes()).hexdigest()[:12]
    version = sh([args.brief_bin, "--version"]).stdout.strip().replace("\n", " ")
    label = f"{Path(args.brief_bin).name} ({version}, sha256 {digest}), {args.parts} hook part(s)"
    report = render(rows, label)
    run.out.mkdir(parents=True, exist_ok=True)
    (run.out / "report.md").write_text(report, encoding="utf-8")
    (run.out / "report.json").write_text(json.dumps({"binary": label, "rows": rows}, indent=1), encoding="utf-8")
    print("\n" + report)
    print(f"report {run.out / 'report.md'}")
    return 0 if all(r["verdict"] == PASS for r in rows) else 1


class SelfTest(unittest.TestCase):
    def test_settings_register_one_hook_per_part_the_way_setup_writes_them(self):
        body = settings_body("/x/pixel", 3)
        commands = [h["command"] for h in body["hooks"]["UserPromptSubmit"][0]["hooks"]]
        self.assertEqual(len(commands), 3)
        self.assertIn("--part 2/3", commands[1])
        self.assertTrue(all(c.startswith("'/x/pixel' run-hook task-event --provider claude --event prompt-submit")
                            and c.endswith("|| true") for c in commands))
        single = settings_body("/x/pixel", 1)["hooks"]["UserPromptSubmit"][0]["hooks"]
        self.assertEqual(len(single), 1)
        self.assertNotIn("--part", single[0]["command"])

    def test_stream_counts_each_tool_call_once_and_ignores_subagent_calls(self):
        lines = [
            json.dumps({"type": "system", "subtype": "hook_response", "hook_event": "UserPromptSubmit",
                        "outcome": "success", "stdout": "x" * 40}),
            json.dumps({"type": "assistant", "message": {"content": [{"type": "text", "text": "hi"}]}}),
            json.dumps({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "a", "name": "Read"}]}}),
            json.dumps({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "a", "name": "Read"}]}}),
            json.dumps({"type": "assistant", "parent_tool_use_id": "z",
                        "message": {"content": [{"type": "tool_use", "id": "b", "name": "Grep"}]}}),
            "not json",
            json.dumps({"type": "result", "subtype": "success"}),
        ]
        stream = parse_stream(lines)
        self.assertEqual([c["tool"] for c in stream["calls"]], ["Read"])
        self.assertEqual(stream["hooks"][0]["chars"], 40)
        self.assertEqual(stream["result"]["subtype"], "success")

    def test_verdict_and_report(self):
        self.assertEqual(verdict(0), PASS)
        self.assertEqual(verdict(3), FELL_BACK)
        rows = [{"id": "a-1", "repo": "a", "n_calls": 0, "tools": {}, "brief_chars": 900, "verdict": PASS},
                {"id": "a-2", "repo": "a", "n_calls": 2, "tools": {"Read": 1, "Grep": 1}, "brief_chars": 90,
                 "verdict": FELL_BACK}]
        text = render(rows, "t")
        self.assertIn("| a-2 | a | 2 | Grep x1, Read x1 | 90 | fell back to tool calls |", text)
        self.assertIn("**1 of 2 answered with 0 tool calls**; 1 fell back to tool calls.", text)

    def test_answer_sessions_reads_only_ok_answer_rows(self):
        import tempfile
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "census.db"
            db = sqlite3.connect(path)
            db.execute("CREATE TABLE sessions(id TEXT, source TEXT, repo TEXT, sha TEXT, mode TEXT, prompt TEXT, ok INT)")
            db.executemany("INSERT INTO sessions VALUES (?,?,?,?,?,?,?)", [
                ("a-1", "census", "a", "s1", "answer", "q1", 1), ("a-2", "census", "a", "s2", "edit", "q2", 1),
                ("a-3", "census", "a", "s3", "answer", "q3", 0), ("a-4", "history", "a", "s4", "answer", "q4", 1),
                ("a-5", "census", "b", "s5", "answer", "q5", 1)])
            db.commit()
            db.close()
            self.assertEqual([r[0] for r in answer_sessions(path)], ["a-1", "a-5"])
            self.assertEqual([r[0] for r in answer_sessions(path, {"a-5"})], ["a-5"])

    def test_a_blobless_clone_is_recognised_by_its_promisor_remote(self):
        import tempfile
        with tempfile.TemporaryDirectory() as tmp:
            git = Path(tmp) / ".git"
            git.mkdir()
            self.assertFalse(is_promisor_clone(tmp))
            (git / "config").write_text("[core]\n\tbare = false\n")
            self.assertFalse(is_promisor_clone(tmp))
            (git / "config").write_text("[remote \"origin\"]\n\tpromisor = true\n\tpartialclonefilter = blob:none\n")
            self.assertTrue(is_promisor_clone(tmp))

    def test_the_api_key_never_reaches_a_child(self):
        os.environ["ANTHROPIC_API_KEY"] = "x"
        try:
            self.assertNotIn("ANTHROPIC_API_KEY", clean_env())
        finally:
            del os.environ["ANTHROPIC_API_KEY"]


if __name__ == "__main__":
    sys.exit(main())
