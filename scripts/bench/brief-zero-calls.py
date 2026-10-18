#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Zero-call test of the prompt-submit brief (issue #906).

The questions are answer-only (no edit), and the census oracle experiment (#905) showed that such a question needs
no tool call when the context the agent would have read is injected. This script runs them LIVE with Pixel's own
brief instead of the oracle and counts the tool calls of each. The default questions are the public bench
(``eval/brief-bench/questions.jsonl``: 56 questions on 7 pinned public repositories, with the plain-Codex baseline
call counts of ``baselines.jsonl``); ``--census-db`` runs the 13 answer-only questions of the private census instead,
compared with their vanilla Claude Code sessions:

    zero calls; quality unreviewed   delivered brief, nonempty answer, 0 calls; correctness needs review
    fell back to tool calls >= 1 call: the agent fetched more evidence
    brief not delivered     the hooks printed no byte to the agent: not a pass, whatever it did

``--agent`` picks the live agent: ``codex`` (the default) or ``claude``. A session is an
``(arm, question)`` pair: ``--arm brief`` registers the hooks, ``--arm nobrief`` is the same
question with no Pixel hook at all (the fair baseline), ``--arm both`` runs the two.

Per session it makes a pinned clone of the question's repository. A brief session indexes the clone with
the binary under test (``prepare-repo``, one ``search-meaning``, throwaway briefs until the output
settles, as ``score_brief.py`` does) and registers the hooks the way ``pixel setup`` writes them:

* Codex: a disposable ``CODEX_HOME`` per session holding ``config.toml``, ``hooks.json`` and a copy
  of ``auth.json`` (never printed or logged, deleted with the session; the real ``~/.codex`` is
  never written), and ``codex exec --json --ephemeral --skip-git-repo-check --ignore-user-config
  --dangerously-bypass-approvals-and-sandbox --dangerously-bypass-hook-trust -c features.hooks=true``,
  the recipe of ``eval/clis/codex.sh`` and ``scripts/bench/codex-pixel-ab.sh``. Tool calls are counted from
  the parsed JSON events (command executions, file changes, MCP calls, web searches), not by grep.
* Claude: ``claude -p`` with the census tool set and a settings file with the same hooks.

The hooks are ``run-hook task-event --provider <agent> --event prompt-submit --part K/N`` (N parts:
a host caps what one hook injects), and the ``post-tool-use``, ``stop`` and ``session-start`` hooks
that ``pixel setup`` registers too. The agent runs with a ``pixel`` that is visibly absent from
PATH, so the only Pixel it sees is the brief. What each hook part printed to the agent is read
back from the clone's ``.pixel/brief-feedback.jsonl`` (written by the hooks themselves).

    brief-zero-calls.py --brief-bin <pixel> [--agent codex|claude] [--arm brief|nobrief|both]
                        [--only id,id] [--parts 3] [--jobs 26] [--questions Q.jsonl | --census-db DB]
    brief-zero-calls.py --self-test

The answers and the transcripts are private: the raw streams stay under ``<root>/zero-calls/``, and the printed
and recorded report holds counts, tool names, bytes, seconds and an answer hash, never answer text.
In live-run mode a successful exit measures delivery and tool calls only; answer quality is unreviewed.
``--acceptance-run`` verifies frozen aggregate reports against source reviews and requires the full set to pass.
Compare paired answers with the pinned source before claiming correctness or equal answers. Census prompts are read from
``census.db`` at run time and never written anywhere.

The runner (``scripts/bench/tool-census/run.py``: clone copies, env whitelist, shim, launch, quota guard) and the
question sources (``question_source.py``) live in ``scripts/tool-census``; ``--census-dir`` or
``PIXEL_CENSUS_DIR`` point elsewhere. Standard library only; run with ``python3 -I``.
"""

import argparse
import concurrent.futures
import hashlib
import importlib.util
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
CENSUS_DIRS = tuple(Path(p) for p in (os.environ.get("PIXEL_CENSUS_DIR"), HERE / "tool-census") if p)


def codex_binary(env=os.environ, which=shutil.which, home=Path.home()):
    """``CODEX_BIN``, else the ``codex`` on ``PATH`` (a mise or Homebrew install), else ``~/.local/bin/codex``."""
    return Path(env.get("CODEX_BIN") or which("codex") or (home / ".local" / "bin" / "codex"))


CODEX = codex_binary()
CODEX_AUTH = Path.home() / ".codex" / "auth.json"
# The campaign defaults of eval/run.sh.
CODEX_MODEL = "gpt-5.6-terra"
CODEX_EFFORT = "medium"
ZERO_CALLS = "zero calls; quality unreviewed"
FELL_BACK = "fell back to tool calls"
NOT_DELIVERED = "brief not delivered"
BASELINE = "baseline"
ERROR = "error"
# Items of a `codex exec --json` stream that are not tool calls; every other item type is one.
NON_CALL_ITEMS = frozenset({"agent_message", "reasoning", "error", "todo_list", "user_message", "plan"})
HISTORY_BUDGET_S = 420
SETTLE_TRIES = 14
SETTLE_NEED = 3
LIFECYCLE = (("SessionStart", "session-start"), ("PostToolUse", "post-tool-use"), ("Stop", "stop"))


def load_census(directory):
    """The census runner module (``run.py``), from ``directory`` or ``scripts/tool-census``."""
    candidates = [Path(directory)] if directory else list(CENSUS_DIRS)
    for candidate in candidates:
        path = candidate / "run.py"
        if path.exists():
            spec = importlib.util.spec_from_file_location("census_run", path)
            module = importlib.util.module_from_spec(spec)
            sys.modules["census_run"] = module
            spec.loader.exec_module(module)
            return module
    raise SystemExit("runner (scripts/bench/tool-census/run.py) not found: pass --census-dir or set PIXEL_CENSUS_DIR")


def load_sources(directory=None):
    """``question_source.py``, the bench and census question sources."""
    if "question_source" in sys.modules:
        return sys.modules["question_source"]
    for candidate in ([Path(directory)] if directory else list(CENSUS_DIRS)):
        path = candidate / "question_source.py"
        if path.exists():
            spec = importlib.util.spec_from_file_location("question_source", path)
            module = importlib.util.module_from_spec(spec)
            sys.modules["question_source"] = module
            spec.loader.exec_module(module)
            return module
    raise SystemExit("question_source.py (scripts/tool-census) not found: pass --census-dir or set PIXEL_CENSUS_DIR")


def sh(argv, cwd=None, timeout=900, env=None):
    return subprocess.run([str(a) for a in argv], cwd=cwd, capture_output=True, text=True, timeout=timeout, env=env)


def clean_env():
    env = dict(os.environ)
    env.pop("ANTHROPIC_API_KEY", None)  # banned everywhere
    env.pop("PIXEL_BRIEF", None)        # the kill switch must not blank the brief under test
    env.pop("PIXEL_BRIEF_LOG", None)    # the feedback log is how delivery is read back
    return env


def task_event_command(binary, provider, event, part=None, parts=1):
    """The command ``pixel setup`` registers for ``event``: a prompt-submit part carries ``--part K/N`` and ends
    on ``|| true`` so it exits 0 whatever happens."""
    command = f"'{binary}' run-hook task-event --provider {provider} --event {event}"
    if event == "prompt-submit":
        if parts > 1:
            command += f" --part {part}/{parts}"
        command += " || true"
    return command


def hook_command(binary, part, parts):
    """The command ``pixel setup`` registers for UserPromptSubmit part ``part`` of ``parts`` on Claude Code."""
    return task_event_command(binary, "claude", "prompt-submit", part, parts)


def hook_entry(command):
    return {"type": "command", "command": command, "timeout": 10}


def settings_body(binary, parts):
    """Claude Code settings: one UserPromptSubmit hook per part (a single hook when ``parts`` is 1), and the
    SessionStart, PostToolUse and Stop hooks that ``pixel setup`` registers beside them."""
    commands = [task_event_command(binary, "claude", "prompt-submit", k, parts) for k in range(1, max(parts, 1) + 1)]
    hooks = {"UserPromptSubmit": [{"hooks": [hook_entry(c) for c in commands]}]}
    for host_event, name in LIFECYCLE:
        hooks[host_event] = [{"hooks": [hook_entry(task_event_command(binary, "claude", name))]}]
    return {"hooks": hooks}


def codex_hooks_body(binary, parts):
    """Codex ``hooks.json`` as ``pixel setup`` writes it: every hook its own group, the brief in ``parts`` hooks."""
    prompt = [task_event_command(binary, "codex", "prompt-submit", k, parts) for k in range(1, max(parts, 1) + 1)]
    hooks = {"UserPromptSubmit": [{"hooks": [hook_entry(c)]} for c in prompt]}
    for host_event, name in LIFECYCLE:
        hooks[host_event] = [{"hooks": [hook_entry(task_event_command(binary, "codex", name))]}]
    return {"hooks": hooks}


def codex_config_body(model, effort, ws):
    """The disposable home's ``config.toml``. ``--ignore-user-config`` skips it; the same values travel as flags,
    so the file documents the arm rather than deciding it."""
    return (f'model = "{model}"\nmodel_reasoning_effort = "{effort}"\n\n[features]\nhooks = true\n\n'
            f'[projects."{ws}"]\ntrust_level = "trusted"\n')


def make_codex_home(home, binary, parts, model, effort, ws, hooks=True, auth=None):
    """A disposable ``CODEX_HOME``: ``config.toml``, ``hooks.json`` (brief arm only) and a copy of ``auth.json``.

    The credentials are copied byte for byte, owner-only, and never read into this process's output; the real
    ``~/.codex`` is only read from."""
    auth = Path(auth) if auth else CODEX_AUTH
    if not auth.exists():
        raise SystemExit("no Codex login to copy: run `codex login` first")
    home = Path(home)
    home.mkdir(parents=True, exist_ok=True)
    home.chmod(0o700)
    (home / "config.toml").write_text(codex_config_body(model, effort, ws), encoding="utf-8")
    if hooks:
        (home / "hooks.json").write_text(json.dumps(codex_hooks_body(binary, parts), indent=1) + "\n", encoding="utf-8")
    shutil.copyfile(auth, home / "auth.json")
    (home / "auth.json").chmod(0o600)
    return home


def codex_argv(codex, ws, model, effort):
    """The proven recipe of ``eval/clis/codex.sh`` and ``scripts/bench/codex-pixel-ab.sh``; the prompt is read from stdin."""
    return [str(codex), "exec", "--json", "--ephemeral", "--skip-git-repo-check", "--ignore-user-config",
            "--dangerously-bypass-approvals-and-sandbox", "--dangerously-bypass-hook-trust",
            "-c", "features.hooks=true", "-m", model, "-c", f'model_reasoning_effort="{effort}"',
            "-C", str(ws), "-"]


def command_label(command):
    """The executable a shell command item ran (``rg``, ``sed``...): names only, the arguments are private."""
    try:
        words = shlex.split(command)
        if len(words) >= 3 and words[1] in ("-c", "-lc", "-ic"):
            words = shlex.split(words[2])
        return f"sh:{Path(words[0]).name}" if words else "sh"
    except ValueError:
        return "sh"


def parse_codex_stream(lines):
    """Tool calls (type and executable only: the arguments are private) and the outcome of one ``codex exec --json``
    session. Every item that is not a message, a reasoning step, a todo list or an error is a call, counted once."""
    calls, seen, errors, completed, failed, answer = [], set(), 0, False, False, ""
    for line in lines:
        try:
            rec = json.loads(line)
        except ValueError:
            continue
        kind = rec.get("type")
        item = rec.get("item") or {}
        if kind in ("item.started", "item.updated", "item.completed") and item.get("type") not in NON_CALL_ITEMS:
            ident = item.get("id") or f"anon-{len(calls)}"
            if ident not in seen:
                seen.add(ident)
                label = item.get("type") or "?"
                if label == "command_execution":
                    label = command_label(str(item.get("command") or ""))
                calls.append({"id": ident, "tool": label})
        elif kind == "item.completed" and item.get("type") == "agent_message":
            if item.get("phase") in (None, "final_answer"):
                answer = str(item.get("text") or "")
        elif kind == "item.completed" and item.get("type") == "error":
            errors += 1
        elif kind == "turn.completed":
            completed = True
        elif kind in ("turn.failed", "error"):
            failed = True
    return {"calls": calls, "errors": errors, "completed": completed and not failed, "answer": answer}


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
    return {"calls": calls, "hooks": hooks, "result": result,
            "answer": str((result or {}).get("result") or "")}


def feedback_of(ws):
    """What the hooks of one session printed to the agent, from the clone's ``.pixel/brief-feedback.jsonl``:
    bytes per part, how many parts ran, and the brief's own size. Paths and counts only."""
    path = Path(ws) / ".pixel" / "brief-feedback.jsonl"
    parts, briefed = {}, 0
    late = 0
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        text = ""
    for line in text.splitlines():
        try:
            rec = json.loads(line)
        except ValueError:
            continue
        if rec.get("t") == "part":
            parts[int(rec.get("part") or 0)] = parts.get(int(rec.get("part") or 0), 0) + int(rec.get("bytes") or 0)
            late += 1 if rec.get("late") else 0
        elif rec.get("t") == "prompt":
            briefed += int(rec.get("bytes") or 0)
    return {"parts_ran": len(parts), "part_bytes": sum(parts.values()), "late": late, "brief_bytes": briefed}


def brief_ms_of(ws):
    """Milliseconds the hook's brief took, from the last line of the clone's decision log (the prompt the agent
    was given is the last one the brief judged)."""
    path = Path(ws) / ".pixel" / "brief-decisions.jsonl"
    try:
        lines = [line for line in path.read_text(encoding="utf-8", errors="replace").splitlines() if line.strip()]
        return int(json.loads(lines[-1]).get("elapsed_ms")) if lines else None
    except (OSError, ValueError, TypeError):
        return None


def percentile(values, q):
    """The ``q`` quantile (0..1) of ``values`` by nearest rank; ``None`` for no values."""
    values = sorted(v for v in values if v is not None)
    if not values:
        return None
    return values[min(len(values) - 1, max(0, int(q * len(values) + 0.999999) - 1))]


def verdict(arm, n_calls, delivered, problems=(), *, answered=False):
    """Measure delivery and calls only; a nonempty answer is necessary, not evidence of correctness."""
    if problems or not answered:
        return ERROR
    if arm == "nobrief":
        return BASELINE
    if not delivered:
        return NOT_DELIVERED
    return ZERO_CALLS if n_calls == 0 else FELL_BACK


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


def short(row):
    return f"{row['n_calls']} ({row['verdict']})" if row else "-"


def render(rows, label, claude_census=None, source_kind="census"):
    """The report: aggregates only. With both arms, one line per question compares them. ``claude_census`` maps a
    question id to the calls of its reference session: the vanilla Claude Code session of the census, or the
    recorded plain-Codex baseline of a bench question (``source_kind``)."""
    claude_census = claude_census or {}
    column, reference = (("census Claude calls", "the vanilla Claude Code session of #905") if source_kind == "census"
                         else ("bench baseline calls", "the plain Codex session recorded in the bench baselines"))
    by = {}
    for row in rows:
        by.setdefault(row["id"], {})[row.get("arm", "brief")] = row
    both = any("nobrief" in arms for arms in by.values()) and any("brief" in arms for arms in by.values())
    agent = rows[0].get("agent", "claude") if rows else "?"
    lines = [f"# Zero-call test: {label}", ""]
    if both:
        lines += [f"Live agent: {agent}. Calls per question, without the brief and with it; the {column} column is "
                  f"{reference}.", "",
                  f"| question | repo | {column} | no brief calls | brief calls | brief bytes | result |",
                  "|---|---|---:|---:|---:|---:|---|"]
        for qid in sorted(by):
            base, brief = by[qid].get("nobrief"), by[qid].get("brief")
            census = claude_census.get(qid)
            lines.append(f"| {qid} | {(brief or base)['repo']} | {'-' if census is None else census} | "
                         f"{'-' if not base else base['n_calls']} | {'-' if not brief else brief['n_calls']} | "
                         f"{'-' if not brief else brief['brief_bytes']} | {brief['verdict'] if brief else '-'} |")
    else:
        lines += ["| question | repo | calls | tools | brief bytes | result |", "|---|---|---:|---|---:|---|"]
        for row in sorted(rows, key=lambda r: r["id"]):
            tools = ", ".join(f"{t} x{n}" for t, n in sorted(row["tools"].items())) or "-"
            lines.append(f"| {row['id']} | {row['repo']} | {row['n_calls']} | {tools} | {row['brief_bytes']} | "
                         f"{row['verdict']} |")
    brief_rows = [r for r in rows if r.get("arm", "brief") == "brief"]
    base_rows = [r for r in rows if r.get("arm") == "nobrief"]
    zero_calls = sum(1 for r in brief_rows if r["verdict"] == ZERO_CALLS)
    undelivered = sum(1 for r in brief_rows if r["verdict"] == NOT_DELIVERED)
    errors = sum(1 for r in rows if r["verdict"] == ERROR)
    if brief_rows:
        lines += ["", f"**{zero_calls} of {len(brief_rows)} brief sessions produced a nonempty answer with 0 tool calls**; "
                      f"{sum(1 for r in brief_rows if r['verdict'] == FELL_BACK)} {FELL_BACK}, "
                      f"{undelivered} {NOT_DELIVERED}, {errors} error."]
        lines.append("Answer quality: unreviewed. Zero calls does not establish correctness or equal answers; "
                     "review the paired private transcripts against the pinned source before claiming either.")
        lines.append(f"Brief sessions: {sum(r['n_calls'] for r in brief_rows if r['n_calls'] >= 0)} tool calls in total.")
    if base_rows:
        lines.append(f"No-brief sessions: {sum(r['n_calls'] for r in base_rows if r['n_calls'] >= 0)} tool calls in total, "
                     f"{sum(1 for r in base_rows if r['n_calls'] == 0)} with none.")
    latency = [r.get("brief_ms") for r in brief_rows]
    if any(v is not None for v in latency):
        lines.append(f"Brief latency over {sum(v is not None for v in latency)} sessions: "
                     f"p50 {percentile(latency, 0.5)} ms, p95 {percentile(latency, 0.95)} ms, "
                     f"max {max(v for v in latency if v is not None)} ms (window 3000 ms).")
    if claude_census:
        lines.append(f"{'Census (vanilla Claude Code)' if source_kind == 'census' else 'Bench baseline (plain Codex)'}: "
                     f"{sum(claude_census.get(q, 0) for q in by)} tool calls over these {len(by)} questions.")
    return "\n".join(lines) + "\n"


class Run:
    def __init__(self, args, census):
        self.args, self.census = args, census
        self.root = Path(args.root)
        self.out = self.root / "zero-calls" / args.run_id
        self.lock, self.rows = threading.Lock(), []
        self.shim = census.make_shim_dir(self.root)
        self.index_slots = threading.Semaphore(max(1, args.index_jobs))

    def session(self, sid, ws, arm, prompt, env):
        """Run the agent on ``prompt`` in ``ws``: (tool calls, outcome problems, seconds, answer evidence)."""
        census, args = self.census, self.args
        settings = None
        home = None
        raw = self.out / "raw" / f"{sid}.{arm}.stream.jsonl"
        raw.parent.mkdir(parents=True, exist_ok=True)
        try:
            if args.agent == "codex":
                home = Path(tempfile.mkdtemp(prefix="zc-codex-home-", dir=str(self.root / "work")))
                make_codex_home(home, args.brief_bin, args.parts, args.codex_model, args.codex_effort, ws,
                                hooks=arm == "brief")
                env = dict(env, CODEX_HOME=str(home))
                argv = codex_argv(CODEX, ws, args.codex_model, args.codex_effort)
            else:
                settings = self.out / "settings" / f"{sid}.{arm}.json"
                settings.parent.mkdir(parents=True, exist_ok=True)
                body = settings_body(args.brief_bin, args.parts) if arm == "brief" else {"hooks": {}}
                settings.write_text(json.dumps(body) + "\n", encoding="utf-8")
                argv = census.claude_argv(census.CLAUDE, settings, args.model, args.effort, args.budget)
            lines, _err, code, timed_out, seconds = census.run_claude(argv, prompt, ws, env, args.timeout, None)
        finally:
            if home is not None:
                shutil.rmtree(home, ignore_errors=True)  # the copied login goes with the session
        raw.write_text("".join(x if x.endswith("\n") else x + "\n" for _, x in lines), encoding="utf-8")
        text = [x for _, x in lines]
        if args.agent == "codex":
            stream = parse_codex_stream(text)
            finished = stream["completed"]
        else:
            stream = parse_stream(text)
            finished = bool(stream["result"])
        problems = []
        if timed_out or code != 0 or not finished:
            problems.append(f"session:exit{code}{'+timeout' if timed_out else ''}")
        answer = stream["answer"].strip()
        if not answer:
            problems.append("answer:empty")
        if args.agent == "claude" and ((stream["result"] or {}).get("is_error")
                                      or (stream["result"] or {}).get("subtype") != "success"):
            problems.append("session:unsuccessful-result")
        # Text stays in the private raw stream; public rows carry identity for later review only.
        evidence = {"answer_bytes": len(answer.encode("utf-8")),
                    "answer_sha256": hashlib.sha256(answer.encode("utf-8")).hexdigest() if answer else None,
                    "answer_quality": "unreviewed"}
        return stream["calls"], problems, seconds, evidence

    def unit(self, sid, repo, sha, prompt, arm, total):
        census, args = self.census, self.args
        env = clean_env()
        ws = census.make_workspace(self.root, repo, f"zc-{args.run_id}-{sid}-{arm}", sha)
        row = {"id": sid, "repo": repo, "sha": sha[:8], "agent": args.agent, "arm": arm, "n_calls": -1, "tools": {},
               "brief_bytes": 0, "verdict": ERROR, "problems": []}
        try:
            shutil.rmtree(Path(ws) / ".pixel", ignore_errors=True)
            if arm == "brief":
                with self.index_slots:
                    activate(args.brief_bin, ws, env, not args.no_history)
            calls, problems, seconds, evidence = self.session(sid, ws, arm, prompt, census.agent_env(os.environ, self.shim))
            tools = {}
            for call in calls:
                tools[call["tool"]] = tools.get(call["tool"], 0) + 1
            fb = feedback_of(ws) if arm == "brief" else {"parts_ran": 0, "part_bytes": 0, "late": 0, "brief_bytes": 0}
            if arm == "brief":
                # Paths and counts only: kept so the feedback summary can be run over the whole run.
                saved = self.out / "feedback" / f"{sid}.jsonl"
                saved.parent.mkdir(parents=True, exist_ok=True)
                try:
                    shutil.copyfile(Path(ws) / ".pixel" / "brief-feedback.jsonl", saved)
                except OSError:
                    pass
            delivered = fb["part_bytes"] > 0
            if arm == "brief" and delivered and fb["parts_ran"] != args.parts:
                problems.append(f"hooks:{fb['parts_ran']}/{args.parts}")
            row.update(evidence)
            row.update({"n_calls": len(calls), "tools": tools, "brief_bytes": fb["part_bytes"],
                        "parts_ran": fb["parts_ran"], "parts_late": fb["late"],
                        "brief_ms": brief_ms_of(ws) if arm == "brief" else None, "seconds": round(seconds, 1),
                        "problems": problems, "verdict": verdict(arm, len(calls), delivered, problems,
                                                                answered=evidence["answer_bytes"] > 0)})
        except (OSError, subprocess.SubprocessError) as error:
            row["problems"].append(type(error).__name__)
        finally:
            if arm == "brief":
                try:
                    sh([args.brief_bin, "daemon", "stop", "."], cwd=ws, timeout=60, env=env)
                except (OSError, subprocess.SubprocessError):
                    pass
            shutil.rmtree(ws, ignore_errors=True)
        with self.lock:
            self.rows.append(row)
            print(f"[{len(self.rows)}/{total}] {sid:<13} {arm:<7} calls {row['n_calls']:>2}  {row['verdict']}"
                  f"{'  bytes ' + str(row['brief_bytes']) if arm == 'brief' else ''}"
                  f"{'  ' + ','.join(row['problems']) if row['problems'] else ''}", flush=True)
        return row


def acceptance(questions, question_hash, runs, reviews):
    """Verify reviewed paired runs. Call counts alone, stale reviews and changed experiments cannot pass."""
    expected = {q["id"]: q for q in questions}
    if not expected or len(expected) != len(questions) or not runs:
        raise ValueError("acceptance needs nonempty unique questions and runs")
    review_by = {(r["run"], r["id"]): r for r in reviews}
    if len(review_by) != len(reviews):
        raise ValueError("duplicate quality review")
    experiment_keys = ("native_sha256", "wrapper_sha256", "probe_files", "harness_source_sha", "agent", "model",
                       "effort", "parts", "jobs", "index_jobs", "timeout_s", "history")
    experiment = {k: runs[0]["identity"][k] for k in experiment_keys}
    if not experiment["probe_files"] or experiment["parts"] < 1:
        raise ValueError("acceptance needs frozen probes and positive hook parts")
    by_id, names, initial = {qid: [] for qid in expected}, set(), None
    for run in runs:
        name, identity = run["name"], run["identity"]
        if name in names or identity["run"] != name:
            raise ValueError("duplicate or mismatched run identity")
        names.add(name)
        if identity["questions_sha256"] != question_hash or any(identity[k] != experiment[k] for k in experiment_keys):
            raise ValueError("question set or frozen experiment changed between runs")
        selected = set(identity.get("selected_ids", expected))
        if not selected or not selected <= expected.keys():
            raise ValueError("run contains unknown or empty question selection")
        rows = {(r["id"], r["arm"]): r for r in run["rows"]}
        if len(rows) != len(run["rows"]) or set(rows) != {(qid, arm) for qid in selected for arm in ("brief", "nobrief")}:
            raise ValueError("run must contain one complete pair per selected question")
        if initial is None:
            if selected != expected.keys():
                raise ValueError("first run must cover the entire acceptance set")
            initial = rows
        for qid in selected:
            q, review = expected[qid], review_by.get((name, qid))
            pair = [rows[qid, arm] for arm in ("brief", "nobrief")]
            if any(row["sha"] not in (q["sha"], q["sha"][:8])
                   or row["repo"] != q["repo"].replace("/", "__") for row in pair):
                raise ValueError(f"{name}/{qid}: repository pin mismatch")
            if review:
                if review["source_sha"] != q["sha"]:
                    raise ValueError(f"{name}/{qid}: stale source review")
                for arm, row in zip(("brief", "nobrief"), pair):
                    if review[arm]["answer_sha256"] != row.get("answer_sha256"):
                        raise ValueError(f"{name}/{qid}/{arm}: stale answer review")
                    if review[arm]["quality"] not in ("pass", "fail", "inconclusive"):
                        raise ValueError("unknown quality verdict")
            brief, base = pair
            protocol = all(not r["problems"] and r["n_calls"] >= 0 and r.get("answer_bytes", 0) > 0
                           and re.fullmatch(r"[0-9a-f]{64}", str(r.get("answer_sha256", ""))) for r in pair)
            delivered = (brief["brief_bytes"] > 0 and brief["parts_ran"] == identity["parts"]
                         and brief["parts_late"] == 0)
            quality = bool(review and all(review[a]["quality"] == "pass" for a in ("brief", "nobrief")))
            by_id[qid].append({"run": name, "brief_calls": brief["n_calls"], "control_calls": base["n_calls"],
                               "protocol": bool(protocol), "delivered": delivered, "quality": quality,
                               "reviewed": bool(review)})
    if any(run not in names or qid not in expected for run, qid in review_by):
        raise ValueError("quality review refers to an unloaded run or unknown question")
    cases = []
    for qid, pairs in sorted(by_id.items()):
        reasons = []
        if not all(p["protocol"] for p in pairs): reasons.append("protocol")
        if not all(p["delivered"] for p in pairs): reasons.append("delivery")
        if not all(p["reviewed"] for p in pairs): reasons.append("unreviewed")
        if not all(p["quality"] for p in pairs): reasons.append("quality")
        if any(p["brief_calls"] != 0 for p in pairs): reasons.append("nonzero")
        if len(pairs) < 3: reasons.append("fewer than three pairs")
        cases.append({"id": qid, "accepted": not reasons, "reasons": reasons, "pairs": pairs})
    accepted = sum(c["accepted"] for c in cases)
    return {"questions": len(expected), "accepted": accepted, "full_set_pass": accepted == len(expected),
            "first_run_brief_calls": sum(initial[qid, "brief"]["n_calls"] for qid in expected),
            "first_run_control_calls": sum(initial[qid, "nobrief"]["n_calls"] for qid in expected),
            "cases": cases}


def acceptance_report(args):
    """Read only aggregate reports and hash-bound source reviews; no model is run and no answer text is needed."""
    source = load_sources().open_source(args)
    if source.kind != "bench" or not args.quality_reviews:
        raise ValueError("acceptance requires public questions and --quality-reviews")
    runs = []
    for directory in args.acceptance_run:
        path = Path(directory)
        runs.append({"name": path.name, "identity": json.loads((path / "identity.json").read_text()),
                     "rows": json.loads((path / "report.json").read_text())["rows"]})
    reviews = [json.loads(line) for line in Path(args.quality_reviews).read_text().splitlines() if line.strip()]
    result = acceptance(source.rows, hashlib.sha256(source.path.read_bytes()).hexdigest(), runs, reviews)
    print(json.dumps(result, indent=2))
    return 0 if result["full_set_pass"] else 1


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--brief-bin", help="the pixel binary under test")
    parser.add_argument("--agent", choices=("codex", "claude"), default="codex", help="the live agent (default codex)")
    parser.add_argument("--arm", choices=("brief", "nobrief", "both"), default="brief",
                        help="brief: hooks registered; nobrief: the same question with no Pixel hook")
    parser.add_argument("--census-dir", default=None, help="scripts/tool-census (default: PIXEL_CENSUS_DIR, else beside this script)")
    load_sources().add_arguments(parser)
    parser.add_argument("--run-id", default=time.strftime("zc-%Y%m%d-%H%M%S"))
    parser.add_argument("--only", default=None, help="comma-separated question ids")
    parser.add_argument("--parts", type=int, default=3, help="UserPromptSubmit hooks (brief_parts)")
    parser.add_argument("--jobs", "--concurrency", dest="jobs", type=int, default=1,
                        help="sessions run in parallel, each in its own clone and output file (default 1)")
    parser.add_argument("--index-jobs", type=int, default=4,
                        help="clones indexed at once; the sessions themselves all start together (default 4)")
    parser.add_argument("--model", default=None, help="Claude model (--agent claude)")
    parser.add_argument("--effort", default=None, help="Claude effort (--agent claude)")
    parser.add_argument("--budget", type=float, default=2.0, help="Claude budget in USD (--agent claude)")
    parser.add_argument("--codex-model", default=CODEX_MODEL)
    parser.add_argument("--codex-effort", default=CODEX_EFFORT)
    parser.add_argument("--timeout", type=int, default=900)
    parser.add_argument("--no-history", action="store_true", help="skip build-index --history")
    parser.add_argument("--acceptance-run", action="append", default=[], metavar="DIR",
                        help="verify aggregate paired runs and source reviews, without running an agent; repeat per run")
    parser.add_argument("--quality-reviews", help="JSONL source reviews bound to run, question, source SHA and answer hashes")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(SelfTest)
        return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1
    if args.acceptance_run:
        try:
            return acceptance_report(args)
        except (OSError, ValueError, KeyError) as error:
            parser.error(str(error))
    if not args.brief_bin:
        parser.error("--brief-bin is required")
    args.brief_bin = str(Path(args.brief_bin).resolve())
    # Every session would otherwise fail on its own with a bare FileNotFoundError (2026-10-18: 112 of 112).
    if args.agent == "codex" and not os.access(CODEX, os.X_OK):
        parser.error(f"no Codex binary at {CODEX}: put codex on PATH or set CODEX_BIN")
    census = load_census(args.census_dir)
    source = load_sources().open_source(args)
    root = Path(args.root)
    only = set(args.only.split(",")) if args.only else None
    sessions = source.sessions(only)
    if not sessions:
        raise SystemExit(f"no answer-only {source.kind} session selected")
    arms = ("brief", "nobrief") if args.arm == "both" else (args.arm,)
    units = [(s["id"], s["repo"], s["sha"], s["prompt"], arm) for s in sessions for arm in arms]
    # Two runs starting together collide on git's index.lock: the source prepares the shared cache under a lock.
    (root / "cache").mkdir(parents=True, exist_ok=True)
    source.prepare(root, only)
    (root / "work").mkdir(parents=True, exist_ok=True)
    run = Run(args, census)
    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, args.jobs)) as pool:
        for future in concurrent.futures.as_completed(
                [pool.submit(run.unit, sid, repo, sha, prompt, arm, len(units)) for sid, repo, sha, prompt, arm in units]):
            future.result()
    rows = sorted(run.rows, key=lambda r: (r["id"], r["arm"]))
    digest = hashlib.sha256(Path(args.brief_bin).read_bytes()).hexdigest()[:12]
    version = sh([args.brief_bin, "--version"]).stdout.strip().replace("\n", " ")
    model = f"{args.codex_model}/{args.codex_effort}" if args.agent == "codex" else (args.model or "default")
    label = (f"{args.agent} ({model}), {Path(args.brief_bin).name} ({version}, sha256 {digest}), "
             f"{args.parts} hook part(s)")
    report = render(rows, label, source.reference_calls(), source.kind)
    run.out.mkdir(parents=True, exist_ok=True)
    (run.out / "report.md").write_text(report, encoding="utf-8")
    (run.out / "report.json").write_text(json.dumps({"binary": label, "rows": rows}, indent=1), encoding="utf-8")
    print("\n" + report)
    print(f"report {run.out / 'report.md'}")
    return 0 if all(r["verdict"] in (ZERO_CALLS, BASELINE) for r in rows) else 1


class SelfTest(unittest.TestCase):
    def test_codex_binary_is_codex_bin_then_the_one_on_path_then_local_bin(self):
        home = Path("/h")
        self.assertEqual(codex_binary({"CODEX_BIN": "/opt/codex"}, lambda _n: "/p/codex", home), Path("/opt/codex"))
        self.assertEqual(codex_binary({}, lambda _n: "/p/codex", home), Path("/p/codex"))
        self.assertEqual(codex_binary({}, lambda _n: None, home), Path("/h/.local/bin/codex"))

    def test_a_missing_codex_stops_the_run_before_any_session(self):
        with tempfile.TemporaryDirectory() as tmp, patch(f"{__name__}.CODEX", Path(tmp) / "no-codex"), \
                patch(f"{__name__}.load_census") as census:
            with self.assertRaises(SystemExit) as stop:
                main(["--brief-bin", sys.executable, "--root", tmp])
        self.assertEqual(stop.exception.code, 2)
        census.assert_not_called()

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

    def test_codex_hooks_are_one_group_per_hook_with_the_lifecycle_beside_the_brief(self):
        body = codex_hooks_body("/x/pixel", 3)["hooks"]
        prompt = [g["hooks"][0]["command"] for g in body["UserPromptSubmit"]]
        self.assertEqual(len(prompt), 3)
        self.assertTrue(all(len(g["hooks"]) == 1 for g in body["UserPromptSubmit"]))
        self.assertEqual(prompt[2], "'/x/pixel' run-hook task-event --provider codex --event prompt-submit "
                                    "--part 3/3 || true")
        self.assertEqual(body["PostToolUse"][0]["hooks"][0]["command"],
                         "'/x/pixel' run-hook task-event --provider codex --event post-tool-use")
        self.assertEqual(sorted(body), ["PostToolUse", "SessionStart", "Stop", "UserPromptSubmit"])
        single = codex_hooks_body("/x/pixel", 1)["hooks"]["UserPromptSubmit"]
        self.assertEqual(len(single), 1)
        self.assertNotIn("--part", single[0]["hooks"][0]["command"])

    def test_the_codex_recipe_is_the_proven_one_and_reads_the_prompt_from_stdin(self):
        argv = codex_argv("/bin/codex", "/ws", "m", "medium")
        for flag in ("exec", "--json", "--ephemeral", "--skip-git-repo-check", "--ignore-user-config",
                     "--dangerously-bypass-approvals-and-sandbox", "--dangerously-bypass-hook-trust"):
            self.assertIn(flag, argv)
        self.assertEqual(argv[argv.index("-c") + 1], "features.hooks=true")
        self.assertEqual(argv[-3:], ["-C", "/ws", "-"])

    def test_a_disposable_codex_home_holds_a_private_copy_of_the_login_and_never_the_real_one(self):
        with tempfile.TemporaryDirectory() as tmp:
            auth = Path(tmp) / "auth.json"
            auth.write_text('{"marker": "a-login"}')
            home = Path(tmp) / "home"
            make_codex_home(home, "/x/pixel", 3, "m", "medium", "/ws", hooks=True, auth=auth)
            self.assertEqual((home / "auth.json").read_text(), auth.read_text())
            self.assertEqual((home / "auth.json").stat().st_mode & 0o777, 0o600)
            self.assertEqual(home.stat().st_mode & 0o777, 0o700)
            self.assertNotEqual(home.resolve(), (Path.home() / ".codex").resolve())
            config = (home / "config.toml").read_text()
            self.assertIn('model = "m"', config)
            self.assertIn("hooks = true", config)
            self.assertNotIn("a-login", config + (home / "hooks.json").read_text())
            baseline = Path(tmp) / "baseline"
            make_codex_home(baseline, "/x/pixel", 3, "m", "medium", "/ws", hooks=False, auth=auth)
            self.assertFalse((baseline / "hooks.json").exists())
            with self.assertRaises(SystemExit):
                make_codex_home(Path(tmp) / "none", "/x/pixel", 3, "m", "medium", "/ws", auth=Path(tmp) / "absent")

    def test_codex_calls_are_counted_from_the_parsed_events_once_each(self):
        def event(kind, item):
            return json.dumps({"type": kind, "item": item})
        lines = [
            json.dumps({"type": "thread.started", "thread_id": "t"}),
            event("item.completed", {"id": "e0", "type": "error", "message": "a warning that mentions command_execution"}),
            event("item.completed", {"id": "m0", "type": "agent_message", "text": "item.started command_execution"}),
            event("item.started", {"id": "c1", "type": "command_execution", "command": "/bin/zsh -lc 'rg -n foo .'"}),
            event("item.completed", {"id": "c1", "type": "command_execution", "command": "/bin/zsh -lc 'rg -n foo .'"}),
            event("item.started", {"id": "c2", "type": "command_execution", "command": "sed -n 1,5p a.rs"}),
            event("item.completed", {"id": "p1", "type": "file_change", "changes": []}),
            event("item.completed", {"id": "k1", "type": "mcp_tool_call", "server": "s", "tool": "t"}),
            event("item.completed", {"id": "r1", "type": "reasoning", "text": "..."}),
            event("item.updated", {"id": "t1", "type": "todo_list", "items": []}),
            "not json",
            json.dumps({"type": "turn.completed", "usage": {}}),
        ]
        stream = parse_codex_stream(lines)
        self.assertEqual([c["tool"] for c in stream["calls"]], ["sh:rg", "sh:sed", "file_change", "mcp_tool_call"])
        self.assertEqual(stream["errors"], 1)
        self.assertTrue(stream["completed"])
        self.assertFalse(parse_codex_stream([json.dumps({"type": "turn.failed"})])["completed"])
        self.assertEqual(parse_codex_stream([])["calls"], [])

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

    def test_a_brief_that_did_not_reach_the_agent_is_never_a_zero_call_pass(self):
        self.assertEqual(verdict("brief", 0, True, answered=True), ZERO_CALLS)
        self.assertEqual(verdict("brief", 3, True, answered=True), FELL_BACK)
        self.assertEqual(verdict("brief", 0, False, answered=True), NOT_DELIVERED)
        self.assertEqual(verdict("brief", 5, False, answered=True), NOT_DELIVERED)
        self.assertEqual(verdict("nobrief", 0, False, answered=True), BASELINE)
        self.assertEqual(verdict("brief", 0, True, ["session:exit1"], answered=True), ERROR)

    def test_empty_answers_cannot_qualify_either_arm_even_with_successful_delivery(self):
        for arm in ("brief", "nobrief"):
            self.assertEqual(verdict(arm, 0, True), ERROR)
            self.assertEqual(verdict(arm, 0, True, answered=False), ERROR)

    def test_session_requires_an_answer_and_keeps_its_text_out_of_public_evidence(self):
        for agent in ("codex", "claude"):
            for answer in ("", "   ", "A private answer é"):
                with self.subTest(agent=agent, answer=answer), tempfile.TemporaryDirectory() as tmp:
                    if agent == "codex":
                        records = [{"type": "item.completed", "item": {"type": "agent_message", "text": answer,
                                                                        "phase": "final_answer"}},
                                   {"type": "turn.completed"}]
                    else:
                        records = [{"type": "result", "subtype": "success", "result": answer}]
                    run = Run.__new__(Run)
                    run.root = run.out = Path(tmp)
                    (run.root / "work").mkdir()
                    run.args = SimpleNamespace(agent=agent, brief_bin="/pixel", parts=3, codex_model="m",
                                               codex_effort="medium", model=None, effort=None, budget=1, timeout=1)
                    run.census = SimpleNamespace(CLAUDE="/claude", claude_argv=lambda *args: [],
                                                 run_claude=lambda *args: ([(0, json.dumps(r)) for r in records],
                                                                          "", 0, False, 1))
                    with patch(__name__ + ".make_codex_home"):
                        calls, problems, _, evidence = run.session("q", tmp, "brief", "prompt", {})
                    self.assertEqual(calls, [])
                    self.assertEqual(problems, [] if answer.strip() else ["answer:empty"])
                    self.assertEqual(evidence, {"answer_bytes": len(answer.strip().encode("utf-8")),
                                               "answer_sha256": hashlib.sha256(answer.strip().encode("utf-8")).hexdigest()
                                               if answer.strip() else None, "answer_quality": "unreviewed"})
                    self.assertNotIn("A private answer", json.dumps(evidence))

    def test_claude_error_result_cannot_be_a_zero_call_success_even_if_it_has_text(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = Run.__new__(Run)
            run.root = run.out = Path(tmp)
            run.args = SimpleNamespace(agent="claude", brief_bin="/pixel", parts=3,
                                       model=None, effort=None, budget=1, timeout=1)
            record = {"type": "result", "subtype": "error_max_budget_usd", "is_error": True,
                      "result": "Budget exhausted"}
            run.census = SimpleNamespace(CLAUDE="/claude", claude_argv=lambda *args: [],
                                         run_claude=lambda *args: ([(0, json.dumps(record))], "", 0, False, 1))
            _, problems, _, evidence = run.session("q", tmp, "brief", "prompt", {})
            self.assertEqual(problems, ["session:unsuccessful-result"])
            self.assertEqual(verdict("brief", 0, True, problems, answered=evidence["answer_bytes"] > 0), ERROR)

    def test_codex_commentary_is_not_an_answer_and_only_completed_messages_count(self):
        records = [{"type": "item.completed", "item": {"type": "agent_message", "text": "Looking now",
                                                        "phase": "commentary"}},
                   {"type": "item.started", "item": {"type": "agent_message", "text": "unfinished"}},
                   {"type": "turn.completed"}]
        self.assertEqual(parse_codex_stream(map(json.dumps, records))["answer"], "")
        records.insert(-1, {"type": "item.completed", "item": {"type": "agent_message", "text": "Final",
                                                               "phase": "final_answer"}})
        self.assertEqual(parse_codex_stream(map(json.dumps, records))["answer"], "Final")

    def test_feedback_reads_back_the_bytes_each_part_printed(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(feedback_of(tmp), {"parts_ran": 0, "part_bytes": 0, "late": 0, "brief_bytes": 0})
            log = Path(tmp) / ".pixel" / "brief-feedback.jsonl"
            log.parent.mkdir()
            log.write_text("\n".join([
                json.dumps({"t": "part", "part": 1, "of": 3, "bytes": 9796, "late": False}),
                json.dumps({"t": "part", "part": 2, "of": 3, "bytes": 5783, "late": False}),
                json.dumps({"t": "part", "part": 3, "of": 3, "bytes": 0, "late": True}),
                json.dumps({"t": "prompt", "bytes": 15579}),
                "not json", json.dumps({"t": "tool", "bytes": 99})]) + "\n")
            self.assertEqual(feedback_of(tmp), {"parts_ran": 3, "part_bytes": 15579, "late": 1, "brief_bytes": 15579})

    def test_report_compares_the_arms_per_question_with_the_census_count(self):
        def row(qid, arm, calls, brief_bytes, result):
            return {"id": qid, "repo": "a", "agent": "codex", "arm": arm, "n_calls": calls, "tools": {},
                    "brief_bytes": brief_bytes, "verdict": result}
        rows = [row("a-1", "nobrief", 4, 0, BASELINE), row("a-1", "brief", 0, 900, ZERO_CALLS),
                row("a-2", "nobrief", 2, 0, BASELINE), row("a-2", "brief", 1, 90, FELL_BACK),
                row("a-3", "nobrief", 0, 0, BASELINE), row("a-3", "brief", 0, 0, NOT_DELIVERED)]
        text = render(rows, "t", {"a-1": 7, "a-2": 3, "a-3": 5})
        self.assertIn("| a-1 | a | 7 | 4 | 0 | 900 | zero calls; quality unreviewed |", text)
        self.assertIn("| a-2 | a | 3 | 2 | 1 | 90 | fell back to tool calls |", text)
        self.assertIn("| a-3 | a | 5 | 0 | 0 | 0 | brief not delivered |", text)
        self.assertIn("**1 of 3 brief sessions produced a nonempty answer with 0 tool calls**; 1 fell back to tool calls, "
                      "1 brief not delivered, 0 error.", text)
        self.assertIn("Answer quality: unreviewed. Zero calls does not establish correctness or equal answers", text)
        self.assertIn("No-brief sessions: 6 tool calls in total, 1 with none.", text)
        self.assertIn("Census (vanilla Claude Code): 15 tool calls over these 3 questions.", text)

    def test_latency_is_read_from_the_last_decision_and_reported_as_percentiles(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertIsNone(brief_ms_of(tmp))
            log = Path(tmp) / ".pixel" / "brief-decisions.jsonl"
            log.parent.mkdir()
            log.write_text(json.dumps({"elapsed_ms": 900}) + "\n" + json.dumps({"elapsed_ms": 210}) + "\n")
            self.assertEqual(brief_ms_of(tmp), 210)
            log.write_text("not json\n")
            self.assertIsNone(brief_ms_of(tmp))
        values = list(range(1, 21))
        self.assertEqual(percentile(values, 0.5), 10)
        self.assertEqual(percentile(values, 0.95), 19)
        self.assertEqual(percentile([7], 0.95), 7)
        self.assertIsNone(percentile([None], 0.95))
        rows = [{"id": f"a-{n}", "repo": "a", "arm": "brief", "n_calls": 0, "tools": {}, "brief_bytes": 10,
                 "verdict": ZERO_CALLS, "brief_ms": ms} for n, ms in enumerate([100, 200, 300, 700])]
        self.assertIn("Brief latency over 4 sessions: p50 200 ms, p95 700 ms, max 700 ms", render(rows, "t"))

    def test_single_arm_report_lists_tools(self):
        rows = [{"id": "a-2", "repo": "a", "arm": "brief", "n_calls": 2, "tools": {"sh:rg": 1, "sh:sed": 1},
                 "brief_bytes": 90, "verdict": FELL_BACK}]
        self.assertIn("| a-2 | a | 2 | sh:rg x1, sh:sed x1 | 90 | fell back to tool calls |", render(rows, "t"))

    def test_the_bench_report_names_the_baseline_column_and_the_census_one_the_census(self):
        rows = [{"id": "b-1", "repo": "o__r", "agent": "codex", "arm": arm, "n_calls": n, "tools": {},
                 "brief_bytes": 5, "verdict": v} for arm, n, v in (("nobrief", 6, BASELINE), ("brief", 0, ZERO_CALLS))]
        bench = render(rows, "t", {"b-1": 5}, "bench")
        self.assertIn("| question | repo | bench baseline calls | no brief calls |", bench)
        self.assertIn("| b-1 | o__r | 5 | 6 | 0 | 5 | zero calls; quality unreviewed |", bench)
        self.assertIn("Bench baseline (plain Codex): 5 tool calls over these 1 questions.", bench)
        self.assertIn("census Claude calls", render(rows, "t", {"b-1": 5}))

    def test_the_default_source_is_the_public_bench_and_needs_no_census(self):
        parser = argparse.ArgumentParser()
        load_sources().add_arguments(parser)
        args = parser.parse_args([])
        source = load_sources().open_source(args)
        self.assertEqual(source.kind, "bench")
        self.assertGreaterEqual(len(source.sessions()), 50)
        self.assertEqual(len(source.sessions({"pixel-01"})), 1)

    def acceptance_fixture(self):
        questions = [{"id": "q", "repo": "o/r", "sha": "a" * 40}]
        experiment = {k: "fixed" for k in ("native_sha256", "wrapper_sha256", "harness_source_sha", "agent",
                                           "model", "effort", "jobs", "index_jobs", "timeout_s", "history")}
        experiment.update(probe_files={"probe": "hash"}, parts=3, questions_sha256="questions")
        runs, reviews = [], []
        for n in range(3):
            name = f"r{n}"
            rows = [{"id": "q", "arm": arm, "repo": "o__r", "sha": "a" * 8, "problems": [], "n_calls": 0,
                     "answer_bytes": 10, "answer_sha256": ("1" if arm == "brief" else "2") * 64, "brief_bytes": 100, "parts_ran": 3, "parts_late": 0}
                    for arm in ("brief", "nobrief")]
            runs.append({"name": name, "identity": dict(experiment, run=name), "rows": rows})
            reviews.append({"id": "q", "run": name, "source_sha": "a" * 40,
                            **{a: {"quality": "pass", "answer_sha256": ("1" if a == "brief" else "2") * 64}
                               for a in ("brief", "nobrief")}})
        return questions, runs, reviews

    def test_acceptance_needs_three_complete_quality_passing_pairs(self):
        q, runs, reviews = self.acceptance_fixture()
        self.assertTrue(acceptance(q, "questions", runs, reviews)["full_set_pass"])
        self.assertFalse(acceptance(q, "questions", runs[:1], reviews[:1])["full_set_pass"])
        for arm in ("brief", "nobrief"):
            q, runs, reviews = self.acceptance_fixture()
            reviews[0][arm]["quality"] = "fail"
            self.assertEqual(acceptance(q, "questions", runs, reviews)["accepted"], 0)
        self.assertEqual(acceptance(q, "questions", runs, [])["accepted"], 0)

    def test_acceptance_rejects_stale_reviews_and_changed_experiments(self):
        for field, value in (("source_sha", "b" * 40), ("answer_sha256", "stale")):
            q, runs, reviews = self.acceptance_fixture()
            if field == "source_sha": reviews[0][field] = value
            else: reviews[0]["brief"][field] = value
            with self.assertRaisesRegex(ValueError, "stale"):
                acceptance(q, "questions", runs, reviews)
        q, runs, reviews = self.acceptance_fixture()
        runs[1]["identity"]["probe_files"] = {"changed": "hash"}
        with self.assertRaisesRegex(ValueError, "experiment changed"):
            acceptance(q, "questions", runs, reviews)
        with self.assertRaisesRegex(ValueError, "question set"):
            acceptance(q, "different questions", runs[:1], reviews[:1])

    def test_acceptance_counts_delivery_protocol_and_nonzero_failures(self):
        for field, value in (("parts_ran", 2), ("parts_late", 1), ("brief_bytes", 0),
                             ("n_calls", 1), ("problems", ["session:exit1"]), ("answer_bytes", 0)):
            q, runs, reviews = self.acceptance_fixture()
            runs[1]["rows"][0][field] = value
            with self.subTest(field=field):
                self.assertEqual(acceptance(q, "questions", runs, reviews)["accepted"], 0)

    def test_acceptance_requires_a_successful_control_and_real_answer_identity(self):
        for arm_index in (0, 1):
            for field, value in (("problems", ["session:exit1"]), ("answer_bytes", 0)):
                q, runs, reviews = self.acceptance_fixture()
                runs[1]["rows"][arm_index][field] = value
                self.assertEqual(acceptance(q, "questions", runs, reviews)["accepted"], 0)
        q, runs, reviews = self.acceptance_fixture()
        runs[0]["rows"][0]["answer_sha256"] = "not-a-digest"
        reviews[0]["brief"]["answer_sha256"] = "not-a-digest"
        self.assertEqual(acceptance(q, "questions", runs, reviews)["accepted"], 0)

    def test_acceptance_checks_the_row_pin_and_does_not_reuse_a_review_twice(self):
        q, runs, reviews = self.acceptance_fixture()
        runs[1]["rows"][0]["sha"] = "a"
        with self.assertRaisesRegex(ValueError, "pin mismatch"):
            acceptance(q, "questions", runs, reviews)
        q, runs, reviews = self.acceptance_fixture()
        reviews.append(dict(reviews[0]))
        with self.assertRaisesRegex(ValueError, "duplicate quality review"):
            acceptance(q, "questions", runs, reviews)

    def test_acceptance_cannot_pass_an_empty_partial_or_duplicate_campaign(self):
        q, runs, reviews = self.acceptance_fixture()
        with self.assertRaises(ValueError): acceptance([], "questions", runs, reviews)
        for mutate in (lambda rs: rs[0]["rows"].pop(),
                       lambda rs: rs[0]["rows"].append(dict(rs[0]["rows"][0])),
                       lambda rs: rs.append(dict(rs[0]))):
            q, runs, reviews = self.acceptance_fixture()
            mutate(runs)
            with self.assertRaises(ValueError): acceptance(q, "questions", runs, reviews)

    def test_a_blobless_clone_is_recognised_by_its_promisor_remote(self):
        with tempfile.TemporaryDirectory() as tmp:
            git = Path(tmp) / ".git"
            git.mkdir()
            self.assertFalse(is_promisor_clone(tmp))
            (git / "config").write_text("[core]\n\tbare = false\n")
            self.assertFalse(is_promisor_clone(tmp))
            (git / "config").write_text("[remote \"origin\"]\n\tpromisor = true\n\tpartialclonefilter = blob:none\n")
            self.assertTrue(is_promisor_clone(tmp))

    def test_the_api_key_never_reaches_a_child_and_the_log_switches_stay_out(self):
        os.environ["ANTHROPIC_API_KEY"] = "x"
        os.environ["PIXEL_BRIEF_LOG"] = "0"
        try:
            env = clean_env()
            self.assertNotIn("ANTHROPIC_API_KEY", env)
            self.assertNotIn("PIXEL_BRIEF_LOG", env)
        finally:
            del os.environ["ANTHROPIC_API_KEY"]
            del os.environ["PIXEL_BRIEF_LOG"]


if __name__ == "__main__":
    sys.exit(main())
