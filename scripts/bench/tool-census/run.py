#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Tool census runner: vanilla Claude Code sessions, no Pixel at all (issue #905).

Runs the prompts of the private ``manifest.json`` as parallel headless ``claude -p``
sessions, one throwaway clone each, and keeps the full raw ``stream-json`` of
every session so the extractor can pair each tool call with its output. The
point is to record what an agent does on its own, so nothing here helps it:
empty settings (no hook), no MCP server, no skill, and a ``pixel`` shim on
``PATH`` that prints "command not found" and exits 127 so a stray call is
visible and fails.

    run.py prepare                      # pinned clone cache (4 repos), idempotent
    run.py probe                        # isolation evidence; run before every campaign
    run.py run --run-id <id> [--concurrency 20] [--only id,id] [--skip-done]
    run.py --self-test

``manifest.json`` (the 20 census prompts) is local and git-ignored: write it yourself, or point
``PIXEL_CENSUS_MANIFEST`` at it. The tools that only replay or run questions (``brief-check.py``,
``brief-zero-calls.py``, ``probe-test.py``...) do not need it: they read the public bench
(``eval/brief-bench/``, see ``question_source.py``) and use this module for the agent launch and the clone copies.

State lives under ``$PIXEL_CENSUS_ROOT`` (default ``$TMPDIR/pixel-tool-census``, ``--root``): ``cache/``
(pinned clones), ``ws/<id>/`` (one clone per session), ``raw/<id>.stream.jsonl``,
``diffs/<id>.diff`` (edit sessions), ``runs/<run-id>.json`` (run record), and
``census.db`` (``sessions`` table, columns in the ``SCHEMA`` of ``extract.py``).

``claude_argv``, ``run_claude``, the ``ENV_KEEP`` whitelist, the thread pool and
the ``rl_five_hour`` quota guard are adapted from ``scripts/bench/ab-brief-live.py``
(same project, MIT); they are copied, not imported, because that driver loads
two sibling scripts at import and fixes its tool set to read-only. Standard
library only.
"""

import argparse
import concurrent.futures
import hashlib
import json
import os
import platform
import shlex
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))
import question_source as qs  # noqa: E402

MANIFEST = Path(os.environ.get("PIXEL_CENSUS_MANIFEST") or HERE / "manifest.json")  # private, git-ignored
DEFAULT_ROOT = qs.default_root()
# The real binary: the shell `claude` may be a wrapper function. $PIXEL_CLAUDE_BIN, else the first `claude` on PATH.
CLAUDE = Path(os.environ.get("PIXEL_CLAUDE_BIN") or shutil.which("claude") or Path.home() / ".local" / "bin" / "claude")

TOOLS = "Read,Grep,Glob,Bash,Edit,Write"
DENIED = "Agent,Task,WebFetch,WebSearch,NotebookEdit"  # every call stays attributable to the one agent
# Same idea as ab-brief-live.py: the agent inherits only these. CLAUDE_* is left out on purpose (host-session
# settings, effort, subagent model, entrypoint). ANTHROPIC_* is banned and never forwarded.
ENV_KEEP = ("HOME", "USER", "LOGNAME", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE", "TERM", "TZ",
            "__CF_USER_TEXT_ENCODING", "NODE_USE_SYSTEM_CA")
# Fixed, listed in the run record: no auto-update, no auto-memory (it would write into the real ~/.claude), a
# bash that reads no user rc file (a zsh profile could put the real pixel before the shim), shared toolchain homes.
ENV_SET = {"DISABLE_AUTOUPDATER": "1", "CLAUDE_CODE_DISABLE_AUTO_MEMORY": "1"}
SYSTEM_PATH = ["/usr/local/bin", "/opt/homebrew/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"]
REPOS = {
    "pixel": {"url": os.environ.get("PIXEL_CENSUS_PIXEL_URL") or "https://github.com/Pixel-CLI/pixel.git",
              "sha": "85bede7d9a3c4e385e0a2045241a6466efbd8cbd"},
    "excalidraw": {"url": "https://github.com/excalidraw/excalidraw.git", "branch": "master",
                   "sha": "541653c7c9d587d463a47daacff9fb30e45a52e7"},
    "requests": {"url": "https://github.com/psf/requests.git", "branch": "main",
                 "sha": "611c6162cbc4ac2020a2f91c7cfa4f3abf9bbb60"},
    "ripgrep": {"url": "https://github.com/BurntSushi/ripgrep.git", "branch": "master",
                "sha": "3fce3b5bb0236da2df6d99672afb8a719642eca7"},
}
CANARY_EVENTS = ("SessionStart", "PreToolUse", "PostToolUse", "Stop")
SESSIONS_DDL = """CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY, source TEXT, repo TEXT, sha TEXT, category TEXT,
  mode TEXT, prompt TEXT, prompt_hash TEXT, started_at TEXT, wall_s REAL, model TEXT, tokens_in INTEGER,
  tokens_out INTEGER, cost_usd REAL, final_answer TEXT, ok INTEGER, raw_path TEXT)"""


def now_iso():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def die(message):
    raise SystemExit(f"tool-census: {message}")


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def sh(argv, cwd=None, timeout=900, check=True):
    """A helper command (git, cp); a hang or a failure ends the driver naming it."""
    try:
        proc = subprocess.run([str(a) for a in argv], cwd=cwd, capture_output=True, text=True, encoding="utf-8",
                              errors="replace", check=False, timeout=timeout)
    except subprocess.TimeoutExpired:
        die(f"{shlex.join(map(str, argv))}: no exit after {timeout}s")
    if check and proc.returncode != 0:
        die(f"{shlex.join(map(str, argv))}: exit {proc.returncode}: {proc.stderr.strip()[-300:]}")
    return proc


# --------------------------------------------------------------------------
# Repositories: a pinned cache, one clone per session
# --------------------------------------------------------------------------

def git_head(path):
    return sh(["git", "rev-parse", "HEAD"], cwd=path).stdout.strip()


def prepare_cache(root):
    """Clone each repo once, pinned; returns {repo: sha}. Idempotent."""
    cache = root / "cache"
    cache.mkdir(parents=True, exist_ok=True)
    pins = {}
    for name, spec in REPOS.items():
        dest = cache / name
        if not (dest / ".git").exists():
            if name == "pixel":
                sh(["git", "clone", "-q", "--no-local", spec["url"], dest], timeout=600)
            else:
                sh(["git", "clone", "-q", "--filter=blob:none", spec["url"], dest], timeout=900)
        if name == "pixel":
            sha = spec["sha"]
            sh(["git", "checkout", "-q", "--detach", sha], cwd=dest)
            # The clone carries the live repo's later commits and branches: keep only history up to the pin, so an
            # agent running `git log --all` or `git branch -a` cannot see what came after it.
            for ref in sh(["git", "for-each-ref", "--format=%(refname)"], cwd=dest).stdout.split():
                sh(["git", "update-ref", "-d", ref], cwd=dest)
            sh(["git", "branch", "main", sha], cwd=dest)
            # No remote: the repo's own rules tell an agent to fetch the actual base, and a fetch would move the clone
            # off the pin (the first campaign's pixel-1 rebased onto the live main).
            sh(["git", "remote", "remove", "origin"], cwd=dest, check=False)
            sh(["git", "reflog", "expire", "--expire=now", "--all"], cwd=dest)
            sh(["git", "gc", "-q", "--prune=now"], cwd=dest)
            shutil.rmtree(dest / ".pixel", ignore_errors=True)
        else:
            sha = git_head(dest)
            # The manifest pins the SHA the repo was at when it was written: stay on it, do not follow the branch.
            pinned = pinned_sha(name)
            if pinned and pinned != sha:
                sh(["git", "checkout", "-q", "--detach", pinned], cwd=dest)
                sha = pinned
            if name == "excalidraw":  # a blobless clone fetches lazily; warm what a "review the last commit" needs
                sh(["git", "show", "--stat", "-p", "HEAD"], cwd=dest, check=False)
            else:  # requests and ripgrep hold every object (refetched without the filter): no remote, as for pixel
                sh(["git", "remote", "remove", "origin"], cwd=dest, check=False)
        if git_head(dest) != sha:
            die(f"{name}: HEAD {git_head(dest)} is not the pin {sha}")
        if sh(["git", "status", "--porcelain"], cwd=dest).stdout.strip():
            die(f"{name}: the cache clone is dirty")
        pins[name] = sha
    return pins


def pinned_sha(name):
    """The commit a census repository stays on: the pin in ``REPOS``, else the private manifest's."""
    if REPOS.get(name, {}).get("sha"):
        return REPOS[name]["sha"]
    if not MANIFEST.exists():
        return None
    for row in json.loads(MANIFEST.read_text(encoding="utf-8"))["prompts"]:
        if row["repo"] == name:
            return row["sha"]
    return None


def make_workspace(root, repo, session_id, sha):
    """An independent clone for one session: ``cp -c`` is an APFS copy-on-write copy, so it costs no disk."""
    dest = root / "ws" / session_id
    if dest.exists():
        shutil.rmtree(dest)
    dest.parent.mkdir(parents=True, exist_ok=True)
    qs.copy_tree(root / "cache" / repo, dest)
    if git_head(dest) != sha:
        die(f"{session_id}: workspace HEAD {git_head(dest)} is not {sha}")
    shutil.rmtree(dest / ".pixel", ignore_errors=True)
    return dest


# --------------------------------------------------------------------------
# The agent invocation (adapted from scripts/bench/ab-brief-live.py)
# --------------------------------------------------------------------------

def claude_argv(claude, settings_path, model=None, effort=None, budget=2.0, permission="dontAsk"):
    argv = [str(claude), "-p", "--output-format", "stream-json", "--verbose", "--include-hook-events",
            "--setting-sources", "project,local", "--settings", str(settings_path),
            "--strict-mcp-config", "--disable-slash-commands", "--no-session-persistence",
            "--permission-mode", permission, "--tools", TOOLS, "--allowedTools", TOOLS,
            "--disallowedTools", DENIED, "--max-budget-usd", f"{budget:g}"]
    if model:
        argv += ["--model", model]
    if effort:
        argv += ["--effort", effort]
    return argv


def make_shim_dir(root):
    """A `pixel` that is visibly absent: any call prints "command not found" and exits 127."""
    shim = root / "work" / "shim-bin"
    shim.mkdir(parents=True, exist_ok=True)
    for name in ("pixel", "pixel-dev"):
        path = shim / name
        path.write_text(f'#!/bin/sh\necho "{name}: command not found" >&2\nexit 127\n', encoding="utf-8")
        path.chmod(0o755)
    return shim


def agent_env(base, shim_dir):
    env = {key: base[key] for key in ENV_KEEP if key in base}
    path = []
    for entry in [str(shim_dir)] + [p for p in base.get("PATH", "").split(":") if p] + SYSTEM_PATH:
        if entry not in path:
            path.append(entry)
    env["PATH"] = ":".join(path)
    env.update(ENV_SET)
    cargo, rustup = Path(base.get("HOME", "")) / ".cargo", Path(base.get("HOME", "")) / ".rustup"
    if cargo.exists():
        env["CARGO_HOME"] = str(cargo)
    if rustup.exists():
        env["RUSTUP_HOME"] = str(rustup)
    assert not any(key.startswith("ANTHROPIC_") for key in env), "ANTHROPIC_* must never reach the agent"
    return env


def with_suppress(fn):
    try:
        fn()
    except OSError:
        pass


def run_claude(argv, prompt, cwd, env, timeout, on_rate=None):
    """Run one headless session; returns (stamped stdout lines, stderr, exit code, timed out, seconds).

    ``on_rate`` is called with every ``rate_limit_event`` as it streams, so a quota guard can act mid-run."""
    started = time.monotonic()
    proc = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, text=True, encoding="utf-8", errors="replace",
                            start_new_session=True)
    timed_out = threading.Event()

    def kill():
        timed_out.set()
        with_suppress(lambda: os.killpg(proc.pid, signal.SIGKILL))

    timer = threading.Timer(timeout, kill)
    timer.start()
    errors = []
    reader = threading.Thread(target=lambda: errors.append(proc.stderr.read()), daemon=True)
    reader.start()
    try:
        proc.stdin.write(prompt)
        proc.stdin.close()
    except OSError:
        pass
    lines = []
    for line in proc.stdout:
        lines.append((time.monotonic() - started, line))
        if on_rate is not None and '"rate_limit_event"' in line:
            try:
                on_rate(json.loads(line).get("rate_limit_info") or {}, proc)
            except ValueError:
                pass
    code = proc.wait()
    timer.cancel()
    reader.join(timeout=5)
    return lines, "".join(errors), code, timed_out.is_set(), time.monotonic() - started


# --------------------------------------------------------------------------
# Reading one stream
# --------------------------------------------------------------------------

def collect(lines):
    """The facts of one ``stream-json`` run (top-level agent only: no sub-agent exists)."""
    found = {"init": {}, "result": None, "rate": {}, "hooks": [], "tool_uses": [], "tool_errors": 0, "bad": 0,
             "texts": []}
    seen = set()
    for item in lines:
        raw = item[1] if isinstance(item, tuple) else item
        if not raw.strip():
            continue
        try:
            record = json.loads(raw)
        except ValueError:
            found["bad"] += 1
            continue
        if not isinstance(record, dict):
            continue
        kind, sub = record.get("type"), record.get("subtype")
        if kind == "system" and sub == "init":
            found["init"] = record
        elif kind == "system" and sub in ("hook_started", "hook_response"):
            found["hooks"].append({"event": record.get("hook_event"), "subtype": sub, "name": record.get("hook_name")})
        elif kind == "assistant" and not record.get("parent_tool_use_id"):
            for part in (record.get("message") or {}).get("content") or []:
                if not isinstance(part, dict):
                    continue
                if part.get("type") == "tool_use" and part.get("id") not in seen:
                    seen.add(part.get("id"))
                    found["tool_uses"].append((part.get("name", ""), part.get("input") or {}))
                elif part.get("type") == "text" and part.get("text", "").strip():
                    found["texts"].append(part["text"])
        elif kind == "user":
            content = (record.get("message") or {}).get("content")
            for part in content if isinstance(content, list) else []:
                if isinstance(part, dict) and part.get("type") == "tool_result" and part.get("is_error"):
                    found["tool_errors"] += 1
        elif kind == "rate_limit_event":
            found["rate"] = record.get("rate_limit_info") or {}
        elif kind == "result":
            found["result"] = record
    return found


def five_hour(rate):
    return ((rate.get("unifiedWindows") or {}).get("five_hour") or {}).get("utilization")


def summarize(lines, exit_code=0, timed_out=False):
    """One row of metrics plus the reasons (``problems``) a run cannot count."""
    run = collect(lines)
    result, init = run["result"], run["init"]
    usage = (result or {}).get("usage") or {}
    answer = (result or {}).get("result") if isinstance((result or {}).get("result"), str) else ""
    if not answer and run["texts"]:
        answer = run["texts"][-1]
    by_tool = {}
    for name, _ in run["tool_uses"]:
        by_tool[name] = by_tool.get(name, 0) + 1
    row = {
        "model": init.get("model"), "claude_version": init.get("claude_code_version"),
        "tools_listed": sorted(init.get("tools") or []), "mcp_servers": len(init.get("mcp_servers") or []),
        "skills": len(init.get("skills") or []), "api_key_source": init.get("apiKeySource"),
        "tool_calls": len(run["tool_uses"]), "by_tool": by_tool, "tool_errors": run["tool_errors"],
        "hook_events": len(run["hooks"]), "num_turns": (result or {}).get("num_turns"),
        "terminal_reason": (result or {}).get("terminal_reason"),
        "denials": len((result or {}).get("permission_denials") or []),
        "tokens_in": usage.get("input_tokens", 0) + usage.get("cache_creation_input_tokens", 0)
        + usage.get("cache_read_input_tokens", 0),
        "tokens_in_fresh": usage.get("input_tokens", 0), "cache_creation": usage.get("cache_creation_input_tokens", 0),
        "cache_read": usage.get("cache_read_input_tokens", 0), "tokens_out": usage.get("output_tokens", 0),
        "cost_usd": (result or {}).get("total_cost_usd"), "api_ms": (result or {}).get("duration_api_ms"),
        "rl_status": run["rate"].get("status"), "rl_five_hour": five_hour(run["rate"]), "bad_lines": run["bad"],
        "answer": answer,
    }
    problems = []
    if result is None:
        problems.append("no-result")
    else:
        if result.get("is_error") or result.get("subtype") != "success":
            problems.append(f"result:{result.get('subtype')}")
        if result.get("terminal_reason") not in (None, "completed"):
            problems.append(f"terminal:{result.get('terminal_reason')}")
    if exit_code != 0:
        problems.append(f"exit:{exit_code}")
    if timed_out:
        problems.append("timeout")
    if row["rl_status"] not in (None, "allowed", "allowed_warning"):
        problems.append(f"rate-limit:{row['rl_status']}")
    if row["hook_events"]:
        problems.append(f"hooks:{row['hook_events']}")
    if row["mcp_servers"] or row["skills"]:
        problems.append("not-isolated")
    if not answer.strip():
        problems.append("empty-answer")
    row["problems"] = problems
    return row


# --------------------------------------------------------------------------
# Quota
# --------------------------------------------------------------------------

def quota(root, shim_dir):
    """Five-hour utilisation right now, from a one-token session (about $0.01)."""
    cwd = root / "work"
    cwd.mkdir(parents=True, exist_ok=True)
    settings = write_settings(root)
    argv = [str(CLAUDE), "-p", "--output-format", "stream-json", "--verbose", "--setting-sources", "project,local",
            "--settings", str(settings), "--no-session-persistence", "--tools", "", "--max-budget-usd", "0.5"]
    lines, _, _, _, _ = run_claude(argv, "reply: ok", cwd, agent_env(os.environ, shim_dir), 120)
    return five_hour(collect(lines)["rate"])


def write_settings(root, canary=False):
    """The only settings of a session: empty. The canary variant has hooks, to prove hook events would show up."""
    work = root / "work"
    work.mkdir(parents=True, exist_ok=True)
    body = {}
    if canary:
        body = {"hooks": {event: [{"hooks": [{"type": "command", "command": "echo '{}'", "timeout": 5}]}]
                          for event in CANARY_EVENTS}}
    path = work / ("settings-canary.json" if canary else "settings-empty.json")
    path.write_text(json.dumps(body) + "\n", encoding="utf-8")
    return path


# --------------------------------------------------------------------------
# probe
# --------------------------------------------------------------------------

PROBE_PROMPT = (
    "Do exactly these steps with tools, then answer. 1) Bash: run `command -v pixel; pixel --version; echo exit=$?`. "
    "2) Write a file named probe-write.txt containing the word ok. 3) Edit probe-write.txt replacing ok with done. "
    "4) Bash: `git status --short | head -5`. 5) Read the first line of CLAUDE.md. Final answer: one line per step "
    "with what you saw, then 'RTK-INSTRUCTIONS: yes' if any instruction in your context mentions a command prefix "
    "named rtk or tells you to use Bun instead of npm, otherwise 'RTK-INSTRUCTIONS: no'.")


def probe_session(root, shim_dir, settings, cwd, prompt, label):
    argv = claude_argv(CLAUDE, settings, budget=1.0)
    lines, err, code, timed_out, seconds = run_claude(argv, prompt, cwd, agent_env(os.environ, shim_dir), 400)
    run = collect(lines)
    summary = summarize(lines, code, timed_out)
    calls = [{"tool": n, "input": i} for n, i in run["tool_uses"]]
    tool_text = []
    for item in lines:  # the Bash tool results, as the model saw them
        try:
            record = json.loads(item[1])
        except ValueError:
            continue
        if record.get("type") == "user":
            for part in (record.get("message") or {}).get("content") or []:
                if isinstance(part, dict) and part.get("type") == "tool_result":
                    content = part.get("content")
                    tool_text.append(content if isinstance(content, str) else json.dumps(content))
    return {"label": label, "wall_s": round(seconds, 1), "stderr_tail": err[-200:], "summary": {
        k: v for k, v in summary.items() if k != "answer"}, "answer": summary["answer"], "calls": calls,
        "hook_events": run["hooks"], "tool_results": tool_text, "stream_lines": len(lines)}


def cmd_probe(args):
    root = Path(args.root)
    pins = prepare_cache(root)
    shim = make_shim_dir(root)
    ws = make_workspace(root, "excalidraw", "probe-excalidraw", pins["excalidraw"])
    findings, checks = [], []
    found = probe_session(root, shim, write_settings(root), ws, PROBE_PROMPT, "isolation")
    findings.append(found)
    text = "\n".join(found["tool_results"])
    s = found["summary"]
    tools_ok = s["tools_listed"] == sorted(TOOLS.split(","))
    checks += [
        ("init lists exactly Read,Grep,Glob,Bash,Edit,Write; no MCP server, no skill", tools_ok and not s["mcp_servers"] and not s["skills"]),
        ("no hook event in the stream (--include-hook-events, empty settings)", not found["hook_events"]),
        ("`command -v pixel` resolves to the shim", str(shim / "pixel") in text),
        ("the shim prints 'command not found' and exits 127", "pixel: command not found" in text and "exit=127" in text),
        ("Write and Edit work in the clone (probe-write.txt = done)",
         (ws / "probe-write.txt").exists() and (ws / "probe-write.txt").read_text().strip() == "done"),
        ("Bash reads the clone (git status lists probe-write.txt)", "probe-write.txt" in text),
        ("project CLAUDE.md is loaded (the clone's own) and read", "CLAUDE.md" in text),
        ("no user instruction leaks in (no rtk/Bun rule in context)", "RTK-INSTRUCTIONS: no" in found["answer"]),
        ("model is the CLI default (no --model flag)", bool(s["model"])),
        ("not API-key auth", s["api_key_source"] in (None, "none")),
        ("session completed", not s["problems"]),
    ]
    canary_ws = make_workspace(root, "requests", "probe-canary", pins["requests"])
    canary = probe_session(root, shim, write_settings(root, canary=True), canary_ws,
                           "Run `echo hi` with Bash and reply with its output.", "canary")
    seen = {h["event"] for h in canary["hook_events"]}
    findings.append(canary)
    checks.append(("sensitivity: canary SessionStart/PreToolUse/PostToolUse/Stop hooks DO appear in the stream",
                   set(CANARY_EVENTS) <= seen))
    for name in ("probe-excalidraw", "probe-canary"):
        shutil.rmtree(root / "ws" / name, ignore_errors=True)
    payload = {"at": now_iso(), "claude": str(CLAUDE), "claude_sha256": sha256_file(CLAUDE),
               "checks": [{"check": n, "ok": ok} for n, ok in checks], "findings": findings,
               "shim": str(shim / "pixel"), "env_kept": list(ENV_KEEP), "env_set": ENV_SET}
    out = root / "probes.json"
    out.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    for name, ok in checks:
        print(f"{'PASS' if ok else 'FAIL'}  {name}")
    print(f"evidence: {out}")
    return 0 if all(ok for _, ok in checks) else 1


# --------------------------------------------------------------------------
# run
# --------------------------------------------------------------------------

def load_manifest():
    if not MANIFEST.exists():
        die(f"{MANIFEST}: the census manifest is private and not in the repository; the tools that only replay "
            "questions use the public bench (eval/brief-bench/)")
    data = json.loads(MANIFEST.read_text(encoding="utf-8"))
    rows = data["prompts"]
    ids = [r["id"] for r in rows]
    if len(set(ids)) != len(ids):
        die("duplicate ids in the manifest")
    for r in rows:
        for key in ("id", "repo", "sha", "category", "mode", "prompt", "expected"):
            if not r.get(key):
                die(f"{r.get('id')}: missing {key}")
        if r["repo"] not in REPOS or r["mode"] not in ("answer", "edit"):
            die(f"{r['id']}: bad repo or mode")
    return rows


def open_db(root):
    # census.db is shared with the extractor lane (it owns `calls`): no journal-mode change, wait on its locks.
    db = sqlite3.connect(root / "census.db", timeout=60)
    db.execute(SESSIONS_DDL)
    return db


def clone_diff(ws, sha):
    """Everything that differs from the pinned commit, as one patch: commits the agent made, edits it left in the
    working tree and new files (an agent that follows the repo's own rules may commit, so HEAD alone is not enough)."""
    sh(["git", "add", "-N", "."], cwd=ws, check=False)
    return sh(["git", "diff", "--no-color", sha], cwd=ws, check=False).stdout


class Campaign:
    def __init__(self, args, root, rows, shim):
        self.args, self.root, self.rows, self.shim = args, Path(root), rows, shim
        self.lock = threading.Lock()
        self.stop = threading.Event()
        self.stop_reason = None
        self.done = 0
        self.records = []
        self.settings = write_settings(self.root)

    def halt(self, reason, proc=None):
        if not self.stop.is_set():
            self.stop.set()
            self.stop_reason = reason
            print(f"STOP: {reason}", flush=True)

    def on_rate(self, rate, proc):
        util = five_hour(rate)
        if util is None:
            return
        if util >= self.args.max_utilization:
            self.halt(f"five-hour utilisation {util} >= {self.args.max_utilization}: no further launch")
        if util >= self.args.kill_utilization:  # a hard ceiling for the sessions still running
            self.halt(f"five-hour utilisation {util} >= {self.args.kill_utilization}: killing the running sessions")
            with_suppress(lambda: os.killpg(proc.pid, signal.SIGKILL))

    def unit(self, row, index, total):
        if self.stop.is_set():
            return None
        root, args = self.root, self.args
        ws = make_workspace(root, row["repo"], row["id"], row["sha"])
        argv = claude_argv(CLAUDE, self.settings, args.model, args.effort, args.budget)
        started = now_iso()
        lines, err, code, timed_out, seconds = run_claude(argv, row["prompt"], ws, agent_env(os.environ, self.shim),
                                                          args.timeout, self.on_rate)
        raw = root / "raw" / f"{row['id']}.stream.jsonl"
        raw.parent.mkdir(parents=True, exist_ok=True)
        raw.write_text("".join(line if line.endswith("\n") else line + "\n" for _, line in lines), encoding="utf-8")
        metrics = summarize(lines, code, timed_out)
        diff = clone_diff(ws, row["sha"])
        # the work must stay on the pinned commit: a session whose HEAD no longer descends from it read other code
        left_pin = sh(["git", "merge-base", "--is-ancestor", row["sha"], "HEAD"], cwd=ws, check=False).returncode != 0
        commits_made = len(sh(["git", "rev-list", f"{row['sha']}..HEAD"], cwd=ws, check=False).stdout.split())
        if left_pin:
            metrics["problems"].append("left-pin")
        diff_path = None
        if row["mode"] == "edit":
            diff_path = root / "diffs" / f"{row['id']}.diff"
            diff_path.parent.mkdir(parents=True, exist_ok=True)
            diff_path.write_text(diff, encoding="utf-8")
        record = {"run_id": args.run_id, "id": row["id"], "repo": row["repo"], "category": row["category"],
                  "mode": row["mode"], "started": started, "wall_s": round(seconds, 2), "ok": not metrics["problems"],
                  "stderr_tail": err[-200:] if err.strip() else "", "diff_bytes": len(diff.encode("utf-8")),
                  "answer_mode_dirty": bool(diff.strip()) and row["mode"] == "answer", "left_pin": left_pin,
                  "commits_made": commits_made, "final_head": git_head(ws), **metrics}
        answer = record.pop("answer")
        with self.lock:
            db = open_db(self.root)  # a connection belongs to the thread that made it
            db.execute(
                "INSERT OR REPLACE INTO sessions VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                (row["id"], "census", row["repo"], row["sha"], row["category"], row["mode"], row["prompt"],
                 hashlib.sha256(row["prompt"].encode("utf-8")).hexdigest(), started, round(seconds, 2),
                 metrics["model"], metrics["tokens_in"], metrics["tokens_out"], metrics["cost_usd"], answer,
                 int(record["ok"]), str(raw)))
            db.commit()
            db.close()
            self.records.append(record)
            self.done += 1
            print(f"[{self.done}/{total}] {row['id']:<8} {row['repo']:<10} {row['mode']:<6} "
                  f"{'ok ' if record['ok'] else 'BAD'} calls {metrics['tool_calls']:>3} {seconds:>6.1f}s "
                  f"${metrics['cost_usd'] or 0:.2f} 5h={metrics['rl_five_hour']} {','.join(metrics['problems'])}",
                  flush=True)
        if metrics["rl_five_hour"] is not None and metrics["rl_five_hour"] >= args.max_utilization:
            self.halt(f"five-hour utilisation {metrics['rl_five_hour']} >= {args.max_utilization}")
        return record


def cmd_run(args):
    root = Path(args.root)
    rows = load_manifest()
    if args.only:
        wanted = set(args.only.split(","))
        rows = [r for r in rows if r["id"] in wanted]
    db = open_db(root)
    if args.skip_done:
        done = {r[0] for r in db.execute("SELECT id FROM sessions WHERE ok=1 AND source='census'")}
        rows = [r for r in rows if r["id"] not in done]
    db.close()
    if not rows:
        die("nothing to run")
    pins = prepare_cache(root)
    for r in rows:
        if pins[r["repo"]] != r["sha"]:
            die(f"{r['id']}: manifest pin {r['sha']} is not the cache's {pins[r['repo']]}")
    shim = make_shim_dir(root)
    before = quota(root, shim)
    print(f"five-hour utilisation before: {before}", flush=True)
    if before is None:
        die("cannot read the five-hour utilisation: refusing to run unguarded")
    if before > args.start_ceiling:
        die(f"five-hour utilisation {before} > {args.start_ceiling}: aborting before any launch")
    (root / "runs").mkdir(exist_ok=True)
    started = now_iso()
    campaign = Campaign(args, root, rows, shim)
    t0 = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.concurrency) as pool:
        futures = [pool.submit(campaign.unit, r, i, len(rows)) for i, r in enumerate(rows, 1)]
        for future in concurrent.futures.as_completed(futures):
            future.result()
    wall = time.monotonic() - t0
    after = quota(root, shim)
    ran = sorted(campaign.records, key=lambda r: r["id"])
    record = {
        "run_id": args.run_id, "started": started, "wall_s": round(wall, 1), "five_hour_before": before,
        "five_hour_after": after, "stop_reason": campaign.stop_reason, "concurrency": args.concurrency,
        "not_launched": sorted(set(r["id"] for r in rows) - set(r["id"] for r in ran)),
        "cost_usd": round(sum(r["cost_usd"] or 0 for r in ran), 4), "ok": sum(1 for r in ran if r["ok"]),
        "failed": [r["id"] for r in ran if not r["ok"]],
        "claude": {"path": str(CLAUDE), "sha256": sha256_file(CLAUDE), "version": next(
            (r["claude_version"] for r in ran if r["claude_version"]), None)},
        "model_flag": args.model, "effort_flag": args.effort, "model_seen": sorted({r["model"] for r in ran if r["model"]}),
        "budget_usd": args.budget, "timeout_s": args.timeout, "max_utilization": args.max_utilization,
        "argv": claude_argv("<claude>", "<empty-settings>", args.model, args.effort, args.budget),
        "env_kept": list(ENV_KEEP), "env_set": ENV_SET, "pins": pins,
        "machine": {"platform": platform.platform(), "cpus": os.cpu_count(), "python": platform.python_version(),
                    "load_end": [round(x, 2) for x in os.getloadavg()]},
        "sessions": ran,
    }
    out = root / "runs" / f"{args.run_id}.json"
    out.write_text(json.dumps(record, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(f"run record: {out}")
    print(f"ok {record['ok']}/{len(ran)}  cost ${record['cost_usd']:.2f}  5h {before} -> {after}  wall {wall:.0f}s")
    return 0 if not record["failed"] and not campaign.stop_reason else 1


# --------------------------------------------------------------------------
# self-test
# --------------------------------------------------------------------------

def _stream(*records):
    return [json.dumps(r) + "\n" for r in records]


class SelfTest(unittest.TestCase):
    def test_summarize_counts_calls_tokens_and_answer(self):
        lines = _stream(
            {"type": "system", "subtype": "init", "model": "m", "tools": ["Bash", "Read"], "mcp_servers": [], "skills": []},
            {"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "a", "name": "Grep", "input": {}}]}},
            {"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "a", "name": "Grep", "input": {}}]}},
            {"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "b", "name": "Read", "input": {}}]}},
            {"type": "user", "message": {"content": [{"type": "tool_result", "is_error": True}]}},
            {"type": "rate_limit_event", "rate_limit_info": {"status": "allowed", "unifiedWindows": {
                "five_hour": {"utilization": 0.5}}}},
            {"type": "result", "subtype": "success", "is_error": False, "result": "answer", "total_cost_usd": 0.5,
             "usage": {"input_tokens": 1, "cache_creation_input_tokens": 2, "cache_read_input_tokens": 3,
                       "output_tokens": 4}})
        row = summarize(lines)
        self.assertEqual((row["tool_calls"], row["by_tool"], row["tool_errors"]), (2, {"Grep": 1, "Read": 1}, 1))
        self.assertEqual((row["tokens_in"], row["tokens_out"], row["rl_five_hour"], row["answer"]), (6, 4, 0.5, "answer"))
        self.assertEqual(row["problems"], [])

    def test_hook_event_and_timeout_are_problems(self):
        lines = _stream({"type": "system", "subtype": "hook_started", "hook_event": "Stop"},
                        {"type": "result", "subtype": "success", "result": "x"})
        row = summarize(lines, exit_code=1, timed_out=True)
        self.assertEqual(row["problems"], ["exit:1", "timeout", "hooks:1"])

    def test_agent_env_forwards_no_claude_or_anthropic(self):
        env = agent_env({"HOME": "/h", "PATH": "/x:/usr/bin", "ANTHROPIC_API_KEY": "k", "CLAUDE_CODE_SESSION_ID": "s"},
                        Path("/shim"))
        self.assertTrue(env["PATH"].startswith("/shim:/x:"))
        self.assertFalse([k for k in env if k.startswith(("ANTHROPIC", "CLAUDE_CODE_SESSION"))])

    def test_argv_denies_delegation_and_allows_edits(self):
        argv = claude_argv("c", "s")
        self.assertIn("Agent,Task,WebFetch,WebSearch,NotebookEdit", argv)
        self.assertIn("Read,Grep,Glob,Bash,Edit,Write", argv)
        self.assertIn("--strict-mcp-config", argv)

    def test_manifest_shape(self):
        if not MANIFEST.exists():
            self.skipTest("no manifest")
        rows = load_manifest()
        self.assertEqual(len(rows), 20)
        by = {}
        for r in rows:
            by[r["category"]] = by.get(r["category"], 0) + 1
        self.assertEqual(by, {"change-location": 5, "diagnosis": 3, "configuration": 2, "overview": 2, "review": 1,
                              "lookup": 2, "impact": 1, "flow": 1, "tests": 1, "history": 1, "error-origin": 1})
        self.assertEqual({repo: sum(1 for r in rows if r["repo"] == repo) for repo in REPOS},
                         {"pixel": 5, "excalidraw": 5, "requests": 5, "ripgrep": 5})


def main():
    if "--self-test" in sys.argv:
        sys.argv.remove("--self-test")
        unittest.main(argv=[sys.argv[0]])
        return
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--root", default=str(DEFAULT_ROOT))
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("prepare", help="build the pinned clone cache")
    sub.add_parser("probe", help="isolation evidence")
    run = sub.add_parser("run", help="the campaign")
    run.add_argument("--run-id", required=True)
    run.add_argument("--concurrency", type=int, default=20)
    run.add_argument("--only", help="comma-separated session ids")
    run.add_argument("--skip-done", action="store_true", help="skip ids that already have an ok row")
    run.add_argument("--model", default=None, help="default: the CLI default, recorded")
    run.add_argument("--effort", default=None)
    run.add_argument("--budget", type=float, default=2.0, help="--max-budget-usd per session")
    run.add_argument("--timeout", type=int, default=1500, help="seconds per session")
    run.add_argument("--max-utilization", type=float, default=0.9, help="stop launching at this five-hour utilisation")
    run.add_argument("--kill-utilization", type=float, default=0.97, help="kill running sessions at this utilisation")
    run.add_argument("--start-ceiling", type=float, default=0.75, help="abort when the run starts above this")
    args = parser.parse_args()
    if args.command == "prepare":
        print(json.dumps(prepare_cache(Path(args.root)), indent=2))
        return 0
    if args.command == "probe":
        return cmd_probe(args)
    return cmd_run(args)


if __name__ == "__main__":
    sys.exit(main())
