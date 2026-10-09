#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Live A/B of the prompt-submit brief on a real Claude Code agent (issue #883).

The brief (`pixel run-hook task-event --event prompt-submit`) exists so that an
agent searches the repository less before it answers. This driver measures
that on the real `claude` binary, headless, with three arms on the same
prompts:

* ``off``  the hook runs with ``PIXEL_BRIEF=0``: no brief, everything else equal;
* ``old``  the brief of the baseline binary (the side called ``old``);
* ``new``  the brief of the candidate binary (the side called ``new``).

Every arm gets the same tools (Read, Grep, Glob, Bash: read-only questions),
the same settings file apart from the binary path of the one hook it defines,
a scrubbed environment and an indexed checkout of the labelled commit. A
binary is copied into the work directory and hashed once (``setup``); nothing
rebuilds it while a campaign runs, and ``run`` re-hashes it at the end.

    ab-brief-live.py setup --side old --binary <pixel>        # copy, hash, clone, index, daemon, warm
    ab-brief-live.py select --split test                       # the prompt rows a run would use
    ab-brief-live.py probe                                     # isolation and delivery evidence
    ab-brief-live.py run --run-id <id> --arms off,old,new      # the campaign
    ab-brief-live.py report --run-id <id> [--receipt out.json] # tables, contrasts, criteria

The raw `stream-json` of every run stays under ``eval/arena-results/<run-id>/``
(gitignored); ``report --receipt`` writes the compact redacted record that may
be committed. The protocol, flags and limits are in ``docs/bench/brief-ab.md``.
``--self-test`` runs the parser and statistics tests. Standard library only.
"""

import argparse
import concurrent.futures
import hashlib
import importlib.util
import json
import math
import os
import platform
import random
import re
import shlex
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from datetime import datetime, timezone
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent
REPO = SCRIPTS.parent


def load_sibling(name):
    """A sibling script as a module (their names carry a hyphen)."""
    spec = importlib.util.spec_from_file_location(name.replace("-", "_"), SCRIPTS / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


EXC = load_sibling("excavation-count")
GATE = load_sibling("bench-brief-gate")

FIXTURE_SHA = GATE.FIXTURE_SHA
BRIEF_TAG = GATE.BRIEF_TAG
DEFAULT_SET = GATE.DEFAULT_SET
DEFAULT_WORK = Path(tempfile.gettempdir()) / "pixel-brief-ab"
DEFAULT_RESULTS = REPO / "eval" / "arena-results"

# arm -> (side whose binary, fixture and daemon it uses, whether the brief is on)
ARMS = {"off": ("old", False), "old": ("old", True), "new": ("new", True)}
SIDES = ("old", "new")
TOOLS = "Read,Grep,Glob,Bash"
DENIED = "Edit,Write,NotebookEdit,Agent,Task"
HOOK_ARGS = "run-hook task-event --provider claude --event prompt-submit"
# The only variables the agent process inherits: the rest of a shell carries
# host-session settings (effort, subagent model, entrypoint), secrets and
# wrappers that would differ between a person's terminal and a CI runner.
ENV_KEEP = ("HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE", "TERM", "TZ",
            "__CF_USER_TEXT_ENCODING", "NODE_USE_SYSTEM_CA")
SELECT_SEED = "brief-ab-883"
OFF_KINDS = ("other-repo", "generic-code", "chat", "ops")
# Off-topic prompts that ask for an action are never run: the agent has Bash.
MUTATING = re.compile(r"\b(pull|push|rebase|deploy|release|tag|install|restart|comment|create|merge|"
                      r"checkout|switch|branch|cut|bump|delete|remove|rm|reset|force|fix)\b", re.IGNORECASE)
WARM_PROMPTS = ("how does the daemon notice files that changed while it was still starting up",
                "where is IDLE_TIMEOUT used")  # a dev row and an identifier: never test rows
PROBE_PROMPT = ("where is IDLE_TIMEOUT used? Do not use any tools. Reply with exactly one line: copy "
                "verbatim the line that starts with 'anchors:' from any [PIXEL:BRIEF] block that is in "
                "your context for this message, or reply NONE if no such block is there.")
PROBE_TOOL_PROMPT = "where is IDLE_TIMEOUT used"
CANARY_EVENTS = ("SessionStart", "PreToolUse", "PostToolUse", "Stop")
CONFIDENCE = re.compile(r"^confidence:\s*(\w+)", re.MULTILINE)
PATH_TOKEN = re.compile(r"[\w@+./-]+")
NONINFERIORITY = 0.05  # a citation-rate drop below this counts as equal
OFFTOPIC_EXTRA = 0.5   # extra native calls on off-topic prompts tolerated by the cost check


# --------------------------------------------------------------------------
# Small helpers
# --------------------------------------------------------------------------

def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def now_iso():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def die(message):
    raise SystemExit(f"ab-brief-live: {message}")


def run_cmd(argv, **kwargs):
    return subprocess.run(argv, capture_output=True, text=True, encoding="utf-8", errors="replace", **kwargs)


def ollaya_warm():
    with socket.socket() as probe:
        probe.settimeout(0.5)
        try:
            probe.connect(GATE.OLLAYA_ADDR)
            return True
        except OSError:
            return False


def mean(values):
    return round(statistics.mean(values), 3) if values else None


def median(values):
    return round(statistics.median(values), 3) if values else None


def p90(values):
    if not values:
        return None
    ordered = sorted(values)
    return ordered[max(0, math.ceil(0.9 * len(ordered)) - 1)]


def bootstrap_ci(deltas, seed=883, draws=5000):
    """95% percentile interval of the mean of per-prompt deltas, prompts resampled."""
    if len(deltas) < 2:
        return None
    rng = random.Random(seed)
    means = sorted(statistics.mean(rng.choices(deltas, k=len(deltas))) for _ in range(draws))
    return [round(means[int(0.025 * draws)], 3), round(means[int(0.975 * draws) - 1], 3)]


# --------------------------------------------------------------------------
# Prompt selection
# --------------------------------------------------------------------------

def load_set(path=DEFAULT_SET):
    path = Path(path)
    rows = [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]
    return rows, sha256_file(path)


def rank_key(seed, row_id):
    return hashlib.sha256(f"{seed}:{row_id}".encode()).hexdigest()


def select_prompts(rows, split="test", n_on=16, n_off=4, seed=SELECT_SEED):
    """English plain-language on-topic rows with expected files, and off-topic rows one kind at a time.

    The order is the SHA-256 of ``seed:id``, so the pick is neither the author's
    taste nor the order of the file. Off-topic rows that ask for an action are
    skipped (``MUTATING``).
    """
    english = [r for r in rows if r["split"] == split and r["lang"] == "en"]
    on = sorted((r for r in english if r["on_topic"] and r["kind"] == "plain" and r.get("expected_files")),
                key=lambda r: rank_key(seed, r["id"]))[:n_on]
    pools = {kind: sorted((r for r in english if not r["on_topic"] and r["kind"] == kind
                           and not MUTATING.search(r["text"])), key=lambda r: rank_key(seed, r["id"]))
             for kind in OFF_KINDS}
    off, cursor = [], 0
    while len(off) < n_off and any(pools.values()):
        kind = OFF_KINDS[cursor % len(OFF_KINDS)]
        cursor += 1
        if pools[kind]:
            off.append(pools[kind].pop(0))
    return on, off


def schedule(prompt_ids, arms, reps, seed):
    """Units ``(prompt id, rep, arm)``: repetition by repetition, prompts and arms shuffled in each."""
    rng = random.Random(seed)
    units = []
    for rep in range(1, reps + 1):
        order = list(prompt_ids)
        rng.shuffle(order)
        for prompt_id in order:
            shuffled = list(arms)
            rng.shuffle(shuffled)
            units.extend((prompt_id, rep, arm) for arm in shuffled)
    return units


# --------------------------------------------------------------------------
# The agent invocation
# --------------------------------------------------------------------------

def hook_command(binary):
    return f"{shlex.quote(str(binary))} {HOOK_ARGS}"


def arm_settings(binary, canary=False):
    """The only settings of an arm: one UserPromptSubmit hook (canary hooks for the probe)."""
    hooks = {"UserPromptSubmit": [{"hooks": [{"type": "command", "command": hook_command(binary), "timeout": 10}]}]}
    if canary:
        for event in CANARY_EVENTS:
            hooks[event] = [{"hooks": [{"type": "command", "command": "echo '{}'", "timeout": 5}]}]
    return {"hooks": hooks}


def claude_argv(claude, settings_path, model=None, effort=None, budget=2.0):
    argv = [str(claude), "-p", "--output-format", "stream-json", "--verbose", "--include-hook-events",
            "--setting-sources", "project,local", "--settings", str(settings_path),
            "--strict-mcp-config", "--disable-slash-commands", "--no-session-persistence",
            "--permission-mode", "dontAsk", "--tools", TOOLS, "--allowedTools", TOOLS,
            "--disallowedTools", DENIED, "--max-budget-usd", f"{budget:g}"]
    if model:
        argv += ["--model", model]
    if effort:
        argv += ["--effort", effort]
    return argv


def agent_env(base, bin_dir, brief_on):
    env = {key: base[key] for key in ENV_KEEP if key in base}
    path = []
    for entry in [str(bin_dir)] + [p for p in base.get("PATH", "").split(":") if p]:
        if entry not in path:
            path.append(entry)
    env["PATH"] = ":".join(path)
    env["DISABLE_AUTOUPDATER"] = "1"
    env["PIXEL_DAEMON_AUTO_START"] = "0"
    if not brief_on:
        env["PIXEL_BRIEF"] = "0"
    return env


def run_claude(argv, prompt, cwd, env, timeout):
    """Run one headless session; returns (stamped stdout lines, stderr, exit code, timed out, seconds)."""
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
    lines = [(time.monotonic() - started, line) for line in proc.stdout]
    code = proc.wait()
    timer.cancel()
    reader.join(timeout=5)
    return lines, "".join(errors), code, timed_out.is_set(), time.monotonic() - started


def with_suppress(fn):
    try:
        fn()
    except OSError:
        pass


# --------------------------------------------------------------------------
# Reading one stream
# --------------------------------------------------------------------------

def path_roots(fixture):
    fixture = str(fixture)
    roots = {fixture}
    if fixture.startswith("/private/"):
        roots.add(fixture[len("/private"):])
    else:
        roots.add("/private" + fixture)
    return sorted(roots)


def normalise_token(token, roots):
    token = token.rstrip(".,;:-/")
    for root in roots:
        if token.startswith(root + "/"):
            token = token[len(root) + 1:]
    return token[2:] if token.startswith("./") else token


def cited_paths(answer, expected, roots):
    """Expected files the answer names: exact path after normalisation, and basename only."""
    wanted = set(expected)
    tokens = {normalise_token(token, roots) for token in PATH_TOKEN.findall(answer or "")}
    exact = sorted(wanted & tokens)
    names = {os.path.basename(path) for path in wanted}
    loose = sorted({token for token in tokens if os.path.basename(token) in names and "." in token})
    return exact, bool(loose)


def parse_confidence(text):
    match = CONFIDENCE.search(text or "")
    return match.group(1).lower() if match else None


def hook_context(stdout):
    """``additionalContext`` of a hook's JSON stdout, or None; the text is never pattern-matched."""
    try:
        payload = json.loads(stdout or "")
    except (TypeError, ValueError):
        return None
    if not isinstance(payload, dict):
        return None
    context = (payload.get("hookSpecificOutput") or {}).get("additionalContext")
    return context if isinstance(context, str) else None


def collect(stamped):
    """Events, hooks and result of one `stream-json` run. ``stamped`` is ``[(seconds since start, raw line)]``."""
    events, times, seen = [], [], set()
    found = {"init": {}, "result": None, "rate": {}, "bad": 0, "hooks": [], "tool_errors": 0}
    started = {}
    for stamp, raw in stamped:
        try:
            record = json.loads(raw)
        except ValueError:
            found["bad"] += 1
            continue
        if not isinstance(record, dict) or record.get("parent_tool_use_id"):
            continue
        kind, sub = record.get("type"), record.get("subtype")
        if kind == "system" and sub == "init":
            found["init"] = record
        elif kind == "system" and sub == "hook_started":
            started[record.get("hook_id")] = stamp
        elif kind == "system" and sub == "hook_response":
            found["hooks"].append({"event": record.get("hook_event"), "exit": record.get("exit_code"),
                                   "outcome": record.get("outcome"), "stdout": record.get("stdout"),
                                   "ms": round(1000 * (stamp - started.pop(record.get("hook_id"), stamp)))})
        elif kind == "assistant":
            for part in (record.get("message") or {}).get("content") or []:
                if not isinstance(part, dict):
                    continue
                if part.get("type") == "text" and part.get("text", "").strip():
                    events.append(("text", part["text"]))
                    times.append(stamp)
                elif part.get("type") == "tool_use" and part.get("id") not in seen:
                    seen.add(part.get("id"))
                    events.append(("tool", part.get("name", ""), part.get("input") or {}))
                    times.append(stamp)
        elif kind == "user":
            content = (record.get("message") or {}).get("content")
            for part in content if isinstance(content, list) else []:
                if isinstance(part, dict) and part.get("type") == "tool_result" and part.get("is_error"):
                    found["tool_errors"] += 1
        elif kind == "rate_limit_event":
            found["rate"] = record.get("rate_limit_info") or {}
        elif kind == "result":
            found["result"] = record
    return {**found, "events": events, "times": times, "unanswered_hooks": len(started)}


def analyse(stamped, expected=(), roots=(), brief_on=True):
    """Metrics of one `stream-json` run."""
    run = collect(stamped)
    events, times, init, result, rate = run["events"], run["times"], run["init"], run["result"], run["rate"]
    hooks, tool_errors, bad, started = run["hooks"], run["tool_errors"], run["bad"], range(run["unanswered_hooks"])
    counted = EXC.count_turn(events)
    headline, end = counted["headline"], counted["at"]["end"]
    by_bucket, tools = {}, 0
    for event in events:
        if event[0] == "tool":
            tools += 1
            bucket = EXC.classify_tool(event[1], event[2])[0]
            by_bucket[bucket] = by_bucket.get(bucket, 0) + 1
    answer_index = next((i for i in range(len(events) - 1, -1, -1) if events[i][0] == "text"
                         and not any(e[0] == "tool" for e in events[i + 1:])), None)
    first_tool = next((times[i] for i, e in enumerate(events) if e[0] == "tool"), None)
    prompt_hooks = [h for h in hooks if h["event"] == "UserPromptSubmit"]
    context = hook_context(prompt_hooks[0]["stdout"]) if prompt_hooks else None
    fired = bool(context and context.lstrip().startswith(BRIEF_TAG))
    block = GATE.parse_brief(context if fired else "")
    brief_paths = GATE.mentioned_paths(block)
    usage = (result or {}).get("usage") or {}
    answer = (result or {}).get("result") if isinstance((result or {}).get("result"), str) else ""
    if not answer:
        answer = next((e[1] for e in reversed(events) if e[0] == "text"), "")
    exact, loose = cited_paths(answer, expected, roots) if expected else ([], False)
    row = {
        "model": init.get("model"), "tools_listed": sorted(init.get("tools") or []),
        "mcp_servers": len(init.get("mcp_servers") or []), "skills": len(init.get("skills") or []),
        "api_key_source": init.get("apiKeySource"), "claude_version": init.get("claude_code_version"),
        "hooks": [{k: h[k] for k in ("event", "exit", "outcome", "ms")} for h in hooks],
        "hook_unanswered": len(started),
        "brief_fired": fired, "brief_bytes": len(context.encode("utf-8")) if fired else 0,
        "brief_tier": parse_confidence(context) if fired else None,
        "brief_answered": block["answered"], "brief_ops": block["ops"], "brief_partial": block["partial"],
        "brief_paths": len(brief_paths),
        "brief_names_expected": bool(expected and set(expected) & set(brief_paths[:GATE.FILES_AT])),
        "native": headline["native"], "native_end": end["native"], "pixel": end["pixel"],
        "delegated": end["delegated"], "headline_stop": counted["headline_stop"],
        "tools_total": tools, "by_bucket": by_bucket, "by_tool": end["by_tool"], "tool_errors": tool_errors,
        "num_turns": (result or {}).get("num_turns"), "stop_reason": (result or {}).get("stop_reason"),
        "terminal_reason": (result or {}).get("terminal_reason"),
        "denials": len((result or {}).get("permission_denials") or []),
        "input_tokens": usage.get("input_tokens", 0), "cache_creation": usage.get("cache_creation_input_tokens", 0),
        "cache_read": usage.get("cache_read_input_tokens", 0), "output_tokens": usage.get("output_tokens", 0),
        "cost_usd": (result or {}).get("total_cost_usd"), "api_ms": (result or {}).get("duration_api_ms"),
        "t_first_tool_s": round(first_tool, 2) if first_tool is not None else None,
        "t_answer_s": round(times[answer_index], 2) if answer_index is not None else None,
        "answer_chars": len(answer), "expected_n": len(expected), "cited": exact,
        "cites_expected": (bool(exact) if expected else None), "cites_basename": loose if expected else None,
        "rl_status": rate.get("status"),
        "rl_five_hour": ((rate.get("unifiedWindows") or {}).get("five_hour") or {}).get("utilization"),
        "rl_seven_day": ((rate.get("unifiedWindows") or {}).get("seven_day") or {}).get("utilization"),
        "bad_lines": bad,
    }
    row["input_total"] = row["input_tokens"] + row["cache_creation"] + row["cache_read"]
    row["problems"] = problems_of(row, result, brief_on)
    row["answer_text"] = answer  # callers drop it before a row is stored
    return row


def problems_of(row, result, brief_on):
    """Why a run cannot count; an empty list means it is what its arm claims."""
    problems = []
    if result is None:
        problems.append("no-result")
    else:
        if result.get("is_error") or result.get("subtype") != "success":
            problems.append(f"result:{result.get('subtype')}")
        if result.get("terminal_reason") not in (None, "completed"):
            problems.append(f"terminal:{result.get('terminal_reason')}")
    if row["rl_status"] not in (None, "allowed", "allowed_warning"):
        problems.append(f"rate-limit:{row['rl_status']}")
    events = [h["event"] for h in row["hooks"]]
    if events != ["UserPromptSubmit"]:
        problems.append(f"hooks:{','.join(map(str, events)) or 'none'}")  # a user-level hook leaked in, or ours did not run
    elif row["hooks"][0]["exit"] != 0:
        problems.append("prompt-hook-exit")
    if row["hook_unanswered"]:
        problems.append("hook-unanswered")
    if row["tools_listed"] != sorted(TOOLS.split(",")):
        problems.append("tools")
    if row["mcp_servers"] or row["skills"]:
        problems.append("extensions")
    if not brief_on and row["brief_fired"]:
        problems.append("off-arm-fired")
    return problems


# --------------------------------------------------------------------------
# Searching versus reading (a read of a file the brief named is targeted)
# --------------------------------------------------------------------------

SEARCH_VERBS = {"grep", "egrep", "fgrep", "rg", "ag", "ack", "find", "fd", "fdfind", "ls", "tree"}
READ_VERBS = {"cat", "bat", "head", "tail", "less", "more", "nl"}
GIT_SEARCH, GIT_READ = {"grep", "ls-files"}, {"cat-file"}  # excavation-count's GIT_EXPLORE
NUMERIC_ARG = re.compile(r"^[\d,]+[a-z]?$")
REF_KEYS = ("ref_search", "ref_reads_brief", "ref_reads_other", "ref_reads", "ref_reads_expected", "ref_reads_unexpected",
            "ref_first_read_idx", "ref_reached", "output_tokens", "num_turns")


def pipeline_kind(stages, depth=0):
    """``(kind, paths)`` of one pipeline: only its first command explores, any stage can write."""
    first = EXC.strip_wrappers(stages[0])
    if not first:
        return "other", []
    if any(EXC.writes_a_file(EXC.strip_wrappers(stage)) for stage in stages):
        return "edit", []
    base = os.path.basename(first[0])
    if base in ("sh", "bash", "zsh") and "-c" in first and depth < 2:
        inner = first[first.index("-c") + 1:][:1]
        return bash_kind(inner[0], depth + 1) if inner else ("other", [])
    if base in EXC.PIXEL_BINARIES:
        return "pixel", []
    args = [t for t in first[1:] if not t.startswith("-") and not NUMERIC_ARG.match(t)]
    paths = [t for t in args if "/" in t or "." in t]
    if base == "sed":
        quiet = any("n" in f[1:] for f in first[1:] if f.startswith("-") and not f.startswith("--"))
        return ("read", paths) if quiet or "--quiet" in first or "--silent" in first else ("other", [])
    if base == "git":
        sub = next((t for t in first[1:] if not t.startswith("-")), "")
        return ("search", []) if sub in GIT_SEARCH else ("read", paths) if sub in GIT_READ else ("other", [])
    if base in SEARCH_VERBS:
        return "search", []
    return ("read", paths) if base in READ_VERBS else ("other", [])


def bash_kind(command, depth=0):
    """One Bash call is one call: edit, then search, then read, then pixel, whichever it holds first."""
    pipelines = EXC.split_pipelines(command or "")
    if pipelines is None:
        pipelines = [[(command or "").split()]]
    kinds = [pipeline_kind(pipeline, depth) for pipeline in pipelines if pipeline]
    for wanted in ("edit", "search", "read", "pixel"):
        hits = [k for k in kinds if k[0] == wanted]
        if hits:
            return wanted, ([p for k in hits for p in k[1]] if wanted == "read" else [])
    return "other", []


def tool_kind(name, tool_input, roots):
    """``(kind, repo-relative paths read)`` of one tool call."""
    lowered = (name or "").lower()
    tool_input = tool_input if isinstance(tool_input, dict) else {}
    if lowered in ("grep", "glob"):
        return "search", set()
    if lowered == "read":
        path = tool_input.get("file_path") or tool_input.get("path") or ""
        return "read", ({normalise_token(path, roots)} if path else set())
    if lowered == "bash":
        kind, paths = bash_kind(tool_input.get("command") or "")
        return kind, {normalise_token(path, roots) for path in paths}
    return EXC.classify_tool(name, tool_input)[0], set()


def stop_index(events):
    """Position of the first edit or of the closing answer: the calls before it count (excavation-count's rule)."""
    answer_at = None
    for position in range(len(events) - 1, -1, -1):
        if events[position][0] == "tool":
            break
        if events[position][0] == "text":
            answer_at = position
    for position, event in enumerate(events):
        if (event[0] == "tool" and EXC.classify_tool(event[1], event[2])[0] == "edit") or position == answer_at:
            return position
    return len(events)


def brief_named_paths(context):
    """Paths a brief names, from the hook's ``additionalContext``: the parsed sections and every path-shaped
    token outside the ``excluded`` line (a token that is not a path never equals a path that was read)."""
    if not context or not context.lstrip().startswith(BRIEF_TAG):
        return set()
    named = set(GATE.mentioned_paths(GATE.parse_brief(context)))
    for line in context.splitlines():
        if not line.startswith("excluded"):
            named |= {normalise_token(token, ()) for token in PATH_TOKEN.findall(line) if "/" in token or "." in token}
    return named


def refine(events, context, expected, roots):
    """Searches, reads (of brief-named files and others) and the first read of an expected file, before the stop."""
    named, wanted = brief_named_paths(context), set(expected)
    out = {"ref_search": 0, "ref_reads_brief": 0, "ref_reads_other": 0, "ref_reads_expected": 0, "ref_calls": 0,
           "ref_first_read_idx": None, "ref_first_touch_idx": None, "ref_brief_named": len(named)}
    for event in events[:stop_index(events)]:
        if event[0] != "tool":
            continue
        out["ref_calls"] += 1
        kind, paths = tool_kind(event[1], event[2], roots)
        if kind == "search":
            out["ref_search"] += 1
        elif kind == "read":
            out["ref_reads_brief" if paths & named else "ref_reads_other"] += 1
            if paths & wanted:
                out["ref_reads_expected"] += 1
                out["ref_first_read_idx"] = out["ref_first_read_idx"] or out["ref_calls"]
        if wanted and out["ref_first_touch_idx"] is None:
            blob = json.dumps(event[2], sort_keys=True)
            if any(path in blob for path in wanted):
                out["ref_first_touch_idx"] = out["ref_calls"]
    out["ref_reads"] = out["ref_reads_brief"] + out["ref_reads_other"]
    out["ref_reads_unexpected"] = out["ref_reads"] - out["ref_reads_expected"] if wanted else None
    out["ref_reached"] = int(out["ref_first_read_idx"] is not None) if wanted else None
    return out


def refine_file(path, expected, roots):
    """``refine`` over a saved raw stream."""
    lines = [(0.0, line) for line in Path(path).read_text(encoding="utf-8", errors="replace").splitlines() if line.strip()]
    run = collect(lines)
    first = next((h for h in run["hooks"] if h["event"] == "UserPromptSubmit"), None)
    return refine(run["events"], hook_context(first["stdout"]) if first else None, expected, roots)


# --------------------------------------------------------------------------
# Sides: binary, fixture, daemon
# --------------------------------------------------------------------------

class Work:
    def __init__(self, root):
        self.root = Path(root)

    def bin_dir(self, side):
        return self.root / "bin" / side

    def binary(self, side):
        return self.bin_dir(side) / "pixel"

    def fixture(self, side):
        return self.root / side / "pixel"

    @property
    def sides_file(self):
        return self.root / "sides.json"

    def sides(self):
        return json.loads(self.sides_file.read_text()) if self.sides_file.exists() else {}

    def save_side(self, side, info):
        sides = self.sides()
        sides[side] = info
        self.root.mkdir(parents=True, exist_ok=True)
        self.sides_file.write_text(json.dumps(sides, indent=2, sort_keys=True) + "\n")


def side_env(work, side, brief_on=True):
    return agent_env(os.environ, work.bin_dir(side), brief_on)


def git(fixture, *args):
    return run_cmd(["git", "-C", str(fixture), *args])


def fixture_state(fixture):
    head = git(fixture, "rev-parse", "HEAD").stdout.strip()
    tree = git(fixture, "rev-parse", "HEAD^{tree}").stdout.strip()
    dirty = git(fixture, "status", "--porcelain", "--untracked-files=all").stdout.strip()
    return head, tree, dirty


def daemon_running(work, side):
    done = run_cmd([str(work.binary(side)), "daemon", "status", str(work.fixture(side))], env=side_env(work, side))
    return done.returncode == 0 and "daemon running" in done.stdout


def ensure_daemon(work, side):
    if daemon_running(work, side):
        return False
    done = run_cmd([str(work.binary(side)), "daemon", "start", str(work.fixture(side))], env=side_env(work, side))
    if done.returncode != 0 or not daemon_running(work, side):
        die(f"daemon of side {side} did not start: {done.stdout[-300:]} {done.stderr[-300:]}")
    return True


def hook_call(work, side, prompt, session="warm"):
    payload = json.dumps({"session_id": session, "prompt": prompt, "cwd": str(work.fixture(side)),
                          "hook_event_name": "UserPromptSubmit"})
    started = time.monotonic()
    done = run_cmd([str(work.binary(side)), *HOOK_ARGS.split()], input=payload, cwd=work.fixture(side),
                   env=side_env(work, side))
    return hook_context(done.stdout), round(time.monotonic() - started, 3)


def warm_brief(work, side, tries=10):
    """Call the hook until every warm-up prompt answers the same, complete brief twice in a row."""
    log = []
    for prompt in WARM_PROMPTS:
        previous, stable = object(), 0
        for attempt in range(1, tries + 1):
            context, seconds = hook_call(work, side, prompt)
            block = GATE.parse_brief(context or "")
            complete = not block["fired"] or (block["answered"] == block["ops"] and not block["partial"])
            stable = stable + 1 if complete and context == previous else 0
            previous = context
            log.append({"prompt": prompt[:40], "attempt": attempt, "fired": block["fired"],
                        "answered": block["answered"], "ops": block["ops"], "seconds": seconds})
            if stable >= 2:
                break
        else:
            die(f"side {side}: the brief did not settle in {tries} calls for {prompt!r}: {log[-3:]}")
    return log


def cmd_setup(args):
    work = Work(args.work)
    side = args.side
    source = Path(args.binary).resolve()
    destination = work.binary(side)
    if destination.exists() and sha256_file(destination) != sha256_file(source) and not args.force:
        die(f"{destination} exists with another hash; --force replaces it (never during a run)")
    destination.parent.mkdir(parents=True, exist_ok=True)
    if not destination.exists() or sha256_file(destination) != sha256_file(source):
        shutil.copy2(source, destination)
    digest = sha256_file(destination)
    version = run_cmd([str(destination), "--version"]).stdout
    commit = next((line.split(":", 1)[1].strip() for line in version.splitlines() if line.startswith("commit:")), "")
    if args.expect_sha and not commit.startswith(args.expect_sha):
        die(f"binary reports commit {commit!r}, expected {args.expect_sha!r}")
    fixture = work.fixture(side)
    if not fixture.exists():
        fixture.parent.mkdir(parents=True, exist_ok=True)
        source_repo = args.source or str(REPO)
        for argv in (["git", "clone", "-q", "--no-checkout", source_repo, str(fixture)],
                     ["git", "-C", str(fixture), "checkout", "-q", "--detach", FIXTURE_SHA],
                     ["git", "-C", str(fixture), "remote", "remove", "origin"]):
            done = run_cmd(argv)
            if done.returncode != 0:
                die(f"{' '.join(argv)}: {done.stderr.strip()}")
    head, tree, dirty = fixture_state(fixture)
    if head != FIXTURE_SHA or dirty:
        die(f"fixture {fixture} is at {head} (dirty: {bool(dirty)}), not clean {FIXTURE_SHA}")
    if (fixture / "eval" / "brief-gate").exists():
        die(f"{fixture} holds eval/brief-gate: the prompt set would index itself")
    done = run_cmd([str(destination), "prepare-repo", str(fixture), "--metrics", "off"], env=side_env(work, side))
    if done.returncode != 0:
        die(f"prepare-repo failed: {done.stdout[-400:]} {done.stderr[-400:]}")
    ensure_daemon(work, side)
    warmup = warm_brief(work, side)
    work.save_side(side, {"binary_sha256": digest, "version": version.strip().splitlines()[0], "commit": commit,
                          "fixture": str(fixture), "fixture_head": head, "fixture_tree": tree,
                          "warm_calls": len(warmup), "set_up": now_iso()})
    print(f"side {side}: {destination} sha256 {digest}\n  commit {commit}; fixture {head} tree {tree}; "
          f"daemon running; brief settled after {len(warmup)} warm-up calls")
    return 0


# --------------------------------------------------------------------------
# select / probe
# --------------------------------------------------------------------------

def cmd_select(args):
    rows, digest = load_set(args.set)
    on, off = select_prompts(rows, args.split, args.n_on, args.n_off, args.select_seed)
    print(f"prompt set sha256 {digest}; split {args.split}; seed {args.select_seed}")
    for label, group in (("on-topic", on), ("off-topic", off)):
        print(f"\n{label} ({len(group)})")
        for row in group:
            print(f"  {row['id']}  {row['kind']:<12}  {row['text'][:90]}")
            if row.get("expected_files"):
                print(f"      expected: {', '.join(row['expected_files'])}")
    return 0


def probe_session(args, work, side, brief_on, settings, prompt, label):
    argv = claude_argv(args.claude, settings, args.model, args.effort, args.budget)
    lines, err, code, timed_out, seconds = run_claude(argv, prompt, work.fixture(side),
                                                      side_env(work, side, brief_on), args.timeout)
    row = analyse(lines, brief_on=brief_on)
    return {"label": label, "exit": code, "timed_out": timed_out, "seconds": round(seconds, 1),
            "reply": row["answer_text"][:300], "hooks": row["hooks"], "brief_fired": row["brief_fired"],
            "tools": row["tools_listed"], "mcp_servers": row["mcp_servers"], "skills": row["skills"],
            "model": row["model"], "api_key_source": row["api_key_source"], "problems": row["problems"],
            "cost_usd": row["cost_usd"], "stderr": err[-200:]}


def cmd_probe(args):
    """Isolation and delivery evidence: user hooks stay out, ours runs, the brief reaches the model."""
    work = Work(args.work)
    out = Path(args.results) / args.run_id
    out.mkdir(parents=True, exist_ok=True)
    findings, checks = [], []
    for arm in args.arms.split(","):
        side, brief_on = ARMS[arm]
        settings = out / f"settings-{arm}.json"
        settings.write_text(json.dumps(arm_settings(work.binary(side))))
        found = probe_session(args, work, side, brief_on, settings, PROBE_PROMPT, f"delivery:{arm}")
        findings.append(found)
        reply = found["reply"].strip()
        ok = reply.startswith("anchors:") if brief_on else reply == "NONE"
        checks.append((f"delivery {arm}: the model {'quotes' if brief_on else 'sees no'} brief", ok and found["brief_fired"] == brief_on))
        checks.append((f"isolation {arm}: only our UserPromptSubmit hook ran", [h["event"] for h in found["hooks"]] == ["UserPromptSubmit"]))
        checks.append((f"isolation {arm}: tools {TOOLS}, no MCP server, no skill",
                       found["tools"] == sorted(TOOLS.split(",")) and not found["mcp_servers"] and not found["skills"]))
    canary = out / "settings-canary.json"
    side = ARMS[args.arms.split(",")[0]][0]
    canary.write_text(json.dumps(arm_settings(work.binary(side), canary=True)))
    found = probe_session(args, work, side, True, canary, PROBE_TOOL_PROMPT, "canary")
    seen = {h["event"] for h in found["hooks"]}
    findings.append(found)
    checks.append(("sensitivity: canary SessionStart/PreToolUse/PostToolUse/Stop hooks appear in the stream",
                   set(CANARY_EVENTS) | {"UserPromptSubmit"} <= seen))
    payload = {"at": now_iso(), "claude": str(args.claude), "findings": findings,
               "checks": [{"check": name, "ok": ok} for name, ok in checks]}
    (out / "probes.json").write_text(json.dumps(payload, indent=2) + "\n")
    for name, ok in checks:
        print(f"{'PASS' if ok else 'FAIL'}  {name}")
    return 0 if all(ok for _, ok in checks) else 1


# --------------------------------------------------------------------------
# run
# --------------------------------------------------------------------------

def redact(value, replacements):
    if isinstance(value, str):
        for needle, label in replacements:
            value = value.replace(needle, label)
        return value
    if isinstance(value, list):
        return [redact(item, replacements) for item in value]
    if isinstance(value, dict):
        return {key: redact(item, replacements) for key, item in value.items()}
    return value


def read_rows(path):
    """Run rows by ``(arm, prompt, rep)``; a rerun replaces the earlier row."""
    rows = {}
    if Path(path).exists():
        for line in Path(path).read_text(encoding="utf-8").splitlines():
            if line.strip():
                row = json.loads(line)
                rows[(row["arm"], row["prompt_id"], row["rep"])] = row
    return list(rows.values())


class Campaign:
    def __init__(self, args, work, prompts, out):
        self.args, self.work, self.prompts, self.out = args, work, prompts, out
        self.rows_path = out / "runs.jsonl"
        self.lock = threading.Lock()
        self.daemon_lock = threading.Lock()
        self.stop = threading.Event()
        self.stop_reason = None
        self.done = 0
        self.known_dirty = set()

    def settings(self, arm):
        return self.out / "settings" / f"{arm}.json"

    def unit(self, prompt_id, rep, arm, index, total):
        if self.stop.is_set():
            return None
        side, brief_on = ARMS[arm]
        row = self.prompts[prompt_id]
        args, work = self.args, self.work
        with self.daemon_lock:
            restarted = ensure_daemon(work, side)
        load = os.getloadavg()[0]
        started = now_iso()
        argv = claude_argv(args.claude, self.settings(arm), args.model, args.effort, args.budget)
        lines, err, code, timed_out, seconds = run_claude(argv, row["text"], work.fixture(side),
                                                          side_env(work, side, brief_on), args.timeout)
        raw = self.out / "raw" / arm / f"{prompt_id}-r{rep}.stream.jsonl"
        raw.parent.mkdir(parents=True, exist_ok=True)
        raw.write_text("".join(line if line.endswith("\n") else line + "\n" for _, line in lines), encoding="utf-8")
        expected = row.get("expected_files") or []
        metrics = analyse(lines, expected, path_roots(work.fixture(side)), brief_on)
        metrics.pop("answer_text")
        if code != 0:
            metrics["problems"].append(f"exit:{code}")
        if timed_out:
            metrics["problems"].append("timeout")
        with self.lock:  # a file an agent left behind flags the run that first sees it, not every later one
            fresh = set(fixture_state(work.fixture(side))[2].splitlines()) - self.known_dirty
            self.known_dirty |= fresh
        if fresh:
            metrics["problems"].append("fixture-dirty")
        record = {"run_id": args.run_id, "arm": arm, "side": side, "prompt_id": prompt_id, "kind": row["kind"],
                  "on_topic": row["on_topic"], "rep": rep, "index": index, "started": started,
                  "wall_s": round(seconds, 2), "load1": round(load, 2), "daemon_restarted": restarted,
                  "ok": not metrics["problems"], "stderr_tail": err[-200:] if err.strip() else "", **metrics}
        with self.lock:
            with open(self.rows_path, "a", encoding="utf-8") as handle:
                handle.write(json.dumps(record, sort_keys=True) + "\n")
            self.done += 1
            utilisation = record["rl_five_hour"]
            if utilisation is not None and utilisation >= args.max_utilization and not self.stop.is_set():
                self.stop.set()
                self.stop_reason = f"five-hour utilisation {utilisation} >= {args.max_utilization}"
            print(f"[{self.done}/{total}] {arm:<3} {prompt_id} r{rep} "
                  f"{'ok ' if record['ok'] else 'BAD'} native {record['native']:>2} tools {record['tools_total']:>2} "
                  f"brief {record['brief_tier'] or ('y' if record['brief_fired'] else '-'):<4} "
                  f"{record['wall_s']:>5.1f}s ${record['cost_usd'] or 0:.2f} "
                  f"{','.join(record['problems'])}", flush=True)
        return record


def identity(args, work, arms, prompt_rows, set_sha, claude_version, claude_hash):
    sides = {side: work.sides().get(side) for side in sorted({ARMS[a][0] for a in arms})}
    return {"run_id": args.run_id, "protocol": "docs/bench/brief-ab.md", "script": "scripts/ab-brief-live.py",
            "script_sha256": sha256_file(Path(__file__)), "started": now_iso(),
            "repo_head": git(REPO, "rev-parse", "HEAD").stdout.strip(),
            "repo_dirty": bool(git(REPO, "status", "--porcelain", "--", "scripts/ab-brief-live.py").stdout.strip()),
            "arms": {
                arm: {"side": ARMS[arm][0], "brief": ARMS[arm][1]} for arm in arms},
            "sides": sides, "fixture_sha": FIXTURE_SHA, "prompt_set_sha256": set_sha,
            "prompts": {"on_topic": [r["id"] for r in prompt_rows if r["on_topic"]],
                        "off_topic": [r["id"] for r in prompt_rows if not r["on_topic"]]},
            "reps": args.reps, "seed": args.seed, "select_seed": args.select_seed, "split": args.split,
            "concurrency": args.concurrency, "model_flag": args.model, "effort_flag": args.effort,
            "budget_usd": args.budget, "timeout_s": args.timeout, "max_utilization": args.max_utilization,
            "claude": {"path": str(args.claude), "version": claude_version, "sha256": claude_hash},
            "argv": claude_argv("<claude>", "<settings>", args.model, args.effort, args.budget),
            "hook": hook_command("<binary-of-the-side>"), "env_kept": list(ENV_KEEP),
            "env_set": ["PATH (side bin first)", "DISABLE_AUTOUPDATER=1", "PIXEL_DAEMON_AUTO_START=0",
                        "PIXEL_BRIEF=0 (off arm only)"],
            "machine": {"platform": platform.platform(), "cpus": os.cpu_count(), "python": platform.python_version(),
                        "load_start": [round(x, 2) for x in os.getloadavg()], "ollaya_warm": ollaya_warm()}}


def cmd_run(args):
    work = Work(args.work)
    arms = args.arms.split(",")
    for arm in arms:
        if arm not in ARMS:
            die(f"unknown arm {arm!r}")
    sides = work.sides()
    for side in sorted({ARMS[a][0] for a in arms}):
        if side not in sides:
            die(f"side {side} is not set up: run `setup --side {side} --binary <pixel>` first")
        if sha256_file(work.binary(side)) != sides[side]["binary_sha256"]:
            die(f"binary of side {side} no longer matches its recorded hash")
        head, _, dirty = fixture_state(work.fixture(side))
        if head != FIXTURE_SHA or dirty:
            die(f"fixture of side {side} is not clean at {FIXTURE_SHA}")
        ensure_daemon(work, side)
        warm_brief(work, side)
    rows, set_sha = load_set(args.set)
    by_id = {row["id"]: row for row in rows}
    if args.prompts:
        chosen = [by_id[p] for p in args.prompts.split(",")]
    else:
        on, off = select_prompts(rows, args.split, args.n_on, args.n_off, args.select_seed)
        chosen = on + off
    prompts = {row["id"]: row for row in chosen}
    units = schedule([row["id"] for row in chosen], arms, args.reps, args.seed)
    out = Path(args.results) / args.run_id
    claude_real = Path(args.claude).resolve()
    claude_version = run_cmd([str(args.claude), "--version"]).stdout.strip()
    meta = identity(args, work, arms, chosen, set_sha, claude_version, sha256_file(claude_real))
    if args.plan:
        print(json.dumps({"units": len(units), "first": units[:6], "identity": meta}, indent=2))
        return 0
    campaign = Campaign(args, work, prompts, out)
    (out / "settings").mkdir(parents=True, exist_ok=True)
    for arm in arms:
        campaign.settings(arm).write_text(json.dumps(arm_settings(work.binary(ARMS[arm][0]))))
    manifest = out / "manifest.json"
    if not manifest.exists():
        manifest.write_text(json.dumps(meta, indent=2, sort_keys=True) + "\n")
    done = {(r["arm"], r["prompt_id"], r["rep"]) for r in read_rows(out / "runs.jsonl")
            if r["ok"] or not args.retry_failed}
    todo = [(i, u) for i, u in enumerate(units) if (u[2], u[0], u[1]) not in done]
    print(f"run {args.run_id}: {len(todo)} of {len(units)} units to run, concurrency {args.concurrency}, "
          f"arms {arms}, {len(chosen)} prompts x {args.reps} reps", flush=True)
    started = time.time()
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.concurrency) as pool:
        futures = [pool.submit(campaign.unit, u[0], u[1], u[2], i, len(units)) for i, u in todo]
        for future in futures:
            future.result()
    end = {"ended": now_iso(), "elapsed_s": round(time.time() - started), "load_end": [round(x, 2) for x in os.getloadavg()],
           "binary_hashes_end": {s: sha256_file(work.binary(s)) for s in sorted({ARMS[a][0] for a in arms})},
           "fixtures_end": {s: fixture_state(work.fixture(s)) for s in sorted({ARMS[a][0] for a in arms})},
           "stopped": campaign.stop_reason}
    stored = json.loads(manifest.read_text())
    stored.setdefault("ends", []).append(end)
    manifest.write_text(json.dumps(stored, indent=2, sort_keys=True) + "\n")
    changed = [s for s, h in end["binary_hashes_end"].items() if h != sides[s]["binary_sha256"]]
    if changed:
        print(f"WARNING: binary of side(s) {changed} changed during the run", file=sys.stderr)
    if campaign.stop_reason:
        print(f"stopped early: {campaign.stop_reason}; rerun with the same --run-id to resume", file=sys.stderr)
        return 3
    return 0


# --------------------------------------------------------------------------
# report
# --------------------------------------------------------------------------

RECEIPT_COLUMNS = ("arm", "prompt_id", "rep", "index", "on_topic", "kind", "ok", "problems", "wall_s", "load1", "native",
                   "native_end", "tools_total", "pixel", "delegated", "input_total", "output_tokens", "cost_usd",
                   "num_turns", "t_first_tool_s", "t_answer_s", "brief_fired", "brief_tier", "brief_bytes",
                   "brief_answered", "brief_ops", "brief_partial", "brief_paths", "brief_names_expected",
                   "cites_expected", "cites_basename", "cited", "model", "rl_five_hour", "daemon_restarted", "started",
                   "ref_search", "ref_reads_brief", "ref_reads_other", "ref_reads_expected", "ref_calls",
                   "ref_first_read_idx", "ref_first_touch_idx", "ref_brief_named", "ref_reached", "ref_reads_unexpected")
KEYS = ("native", "tools_total", "pixel", "input_total", "output_tokens", "wall_s", "cost_usd")


def value_of(row, key):
    value = row.get(key)
    if key == "cites_expected":
        return None if value is None else float(value)
    return value


def summary(rows):
    out = {"runs": len(rows), "prompts": len({r["prompt_id"] for r in rows}),
           "fired": round(sum(1 for r in rows if r["brief_fired"]) / len(rows), 3) if rows else None}
    for key in KEYS:
        values = [r[key] for r in rows if r.get(key) is not None]
        out[key] = {"mean": mean(values), "median": median(values), "p90": p90(values)}
    cites = [float(r["cites_expected"]) for r in rows if r["cites_expected"] is not None]
    out["cites"] = {"rate": mean(cites), "n": len(cites)}
    return out


def prompt_means(rows, key, ids):
    grouped = {}
    for row in rows:
        if row["prompt_id"] in ids and value_of(row, key) is not None:
            grouped.setdefault(row["prompt_id"], []).append(value_of(row, key))
    return {pid: statistics.mean(values) for pid, values in grouped.items()}


def contrast(rows_a, rows_b, key, ids):
    """Per-prompt mean difference ``b - a`` over prompts both arms ran."""
    a, b = prompt_means(rows_a, key, ids), prompt_means(rows_b, key, ids)
    common = sorted(set(a) & set(b))
    deltas = [b[p] - a[p] for p in common]
    return {"prompts": len(common), "mean_delta": mean(deltas), "ci95": bootstrap_ci(deltas),
            "lower": sum(1 for d in deltas if d < -1e-9), "equal": sum(1 for d in deltas if abs(d) <= 1e-9),
            "higher": sum(1 for d in deltas if d > 1e-9)}


def new_groups(valid):
    """The `new` sessions by group: on/off-topic by the brief's tier, and on-topic by whether it names an expected file."""
    new_valid = [r for r in valid if r["arm"] == "new"]
    tier_of = lambda r: (r["brief_tier"] or "untiered") if r["brief_fired"] else "no brief"
    groups = []
    for group, flag in (("on-topic", True), ("off-topic", False)):
        in_group = [r for r in new_valid if r["on_topic"] == flag]
        groups += [(f"{group}: {tier}", [r for r in in_group if tier_of(r) == tier])
                   for tier in sorted({tier_of(r) for r in in_group})]
    fired = [r for r in new_valid if r["on_topic"] and r["brief_fired"]]
    groups += [("on-topic: brief names an expected file", [r for r in fired if r["brief_names_expected"]]),
               ("on-topic: brief fired, names none", [r for r in fired if not r["brief_names_expected"]])]
    return [(name, subset) for name, subset in groups if subset]


def refined_summary(rows):
    out = {"runs": len(rows)}
    for key in REF_KEYS:
        values = [r[key] for r in rows if r.get(key) is not None]
        out[key] = {"mean": mean(values), "median": median(values), "n": len(values)}
    return out


def refined_report(valid, arms, groups):
    """Searches, brief-named and other reads, first read of an expected file, output tokens and turns."""
    out = {"arms": {}, "tiers": {}, "contrasts": []}
    for arm in arms:
        for name, ids in groups.items():
            subset = [r for r in valid if r["arm"] == arm and r["prompt_id"] in ids]
            if subset:
                out["arms"][f"{arm}/{name}"] = refined_summary(subset)
    for name, subset in new_groups(valid) if "new" in arms else []:
        ids = {r["prompt_id"] for r in subset}
        baseline = [r for r in valid if r["arm"] == "off" and r["prompt_id"] in ids]
        out["tiers"][name] = {"new": refined_summary(subset),
                              "off_same_prompts": refined_summary(baseline) if baseline else None}
    for first, second in (("off", "new"), ("off", "old"), ("old", "new")):
        if first not in arms or second not in arms:
            continue
        a_rows = [r for r in valid if r["arm"] == first]
        b_rows = [r for r in valid if r["arm"] == second]
        picks = dict(groups)
        fired_ids = {r["prompt_id"] for r in b_rows if r["brief_fired"]} & groups["on-topic"]
        picks["on-topic, brief fired in the to-arm"] = fired_ids
        picks["on-topic, brief never fired in the to-arm"] = {r["prompt_id"] for r in b_rows} & groups["on-topic"] - fired_ids
        for name, ids in picks.items():
            if ids:
                entry = {"from": first, "to": second, "group": name}
                for key in REF_KEYS:
                    entry[key] = contrast(a_rows, b_rows, key, ids)
                out["contrasts"].append(entry)
    return out


def replicate_sd(valid, ids):
    """Pooled standard deviation of ``native`` between repetitions of one prompt in one arm."""
    cells = {}
    for row in valid:
        if row["prompt_id"] in ids:
            cells.setdefault((row["arm"], row["prompt_id"]), []).append(row["native"])
    variances = [statistics.variance(values) for values in cells.values() if len(values) > 1]
    return round(math.sqrt(statistics.mean(variances)), 2) if variances else None


def brief_code(row):
    return (row["brief_tier"] or "y") if row["brief_fired"] else "-"


def per_prompt(valid, arms):
    table = []
    for pid in sorted({r["prompt_id"] for r in valid}):
        entry = {"prompt_id": pid, "on_topic": next(r["on_topic"] for r in valid if r["prompt_id"] == pid)}
        for arm in arms:
            runs = sorted((r for r in valid if r["prompt_id"] == pid and r["arm"] == arm), key=lambda r: r["rep"])
            entry[arm] = {"native": [r["native"] for r in runs], "brief": [brief_code(r) for r in runs],
                          "cites": "".join("-" if r["cites_expected"] is None else "Y" if r["cites_expected"] else "n"
                                           for r in runs)}
        table.append(entry)
    return sorted(table, key=lambda e: (not e["on_topic"], e["prompt_id"]))


def build_report(rows, manifest=None):
    valid = [r for r in rows if r["ok"]]
    arms = [a for a in ARMS if any(r["arm"] == a for r in rows)]
    on_ids = {r["prompt_id"] for r in rows if r["on_topic"]}
    off_ids = {r["prompt_id"] for r in rows if not r["on_topic"]}
    groups = {"on-topic": on_ids, "off-topic": off_ids}
    report = {"runs": len(rows), "valid": len(valid), "invalid": [
        {"arm": r["arm"], "prompt_id": r["prompt_id"], "rep": r["rep"], "problems": r["problems"]}
        for r in rows if not r["ok"]], "arms": {}, "tiers": {}, "contrasts": [], "criteria": []}
    for arm in arms:
        for name, ids in groups.items():
            subset = [r for r in valid if r["arm"] == arm and r["prompt_id"] in ids]
            if subset:
                report["arms"][f"{arm}/{name}"] = summary(subset)
    for first, second in (("off", "old"), ("off", "new"), ("old", "new")):
        if first not in arms or second not in arms:
            continue
        a_rows = [r for r in valid if r["arm"] == first]
        b_rows = [r for r in valid if r["arm"] == second]
        for name, ids in groups.items():
            if not ids:
                continue
            entry = {"from": first, "to": second, "group": name}
            for key in ("native", "tools_total", "input_total", "wall_s", "cost_usd", "cites_expected"):
                entry[key] = contrast(a_rows, b_rows, key, ids)
            report["contrasts"].append(entry)
        fired_ids = {r["prompt_id"] for r in b_rows if r["brief_fired"] and r["prompt_id"] in on_ids}
        silent_ids = {r["prompt_id"] for r in b_rows if r["prompt_id"] in on_ids} - fired_ids
        for label, ids in (("on-topic, brief fired in the to-arm", fired_ids),
                           ("on-topic, brief never fired in the to-arm", silent_ids)):
            if ids:
                entry = {"from": first, "to": second, "group": label}
                for key in ("native", "tools_total", "input_total", "wall_s", "cost_usd", "cites_expected"):
                    entry[key] = contrast(a_rows, b_rows, key, ids)
                report["contrasts"].append(entry)
    for name, subset in new_groups(valid) if "new" in arms else []:
        ids = {r["prompt_id"] for r in subset}
        baseline = [r for r in valid if r["arm"] == "off" and r["prompt_id"] in ids]
        report["tiers"][name] = {"new": summary(subset), "off_same_prompts": summary(baseline) if baseline else None}
    for first, second in (("off", "new"), ("old", "new"), ("off", "old")):
        on = next((c for c in report["contrasts"] if c["from"] == first and c["to"] == second and c["group"] == "on-topic"), None)
        off = next((c for c in report["contrasts"] if c["from"] == first and c["to"] == second and c["group"] == "off-topic"), None)
        if not on:
            continue
        native, cites = on["native"], on["cites_expected"]
        ci = native["ci95"]
        report["criteria"].append({
            "pair": f"{second} vs {first}",
            "searches_less": bool(native["mean_delta"] is not None and native["mean_delta"] < 0 and ci and ci[1] < 0),
            "native_delta": native["mean_delta"], "native_ci95": ci,
            "answers_not_worse": bool(cites["mean_delta"] is not None and cites["mean_delta"] >= -NONINFERIORITY),
            "cites_delta": cites["mean_delta"],
            "off_topic_cost_ok": (None if not off or off["native"]["mean_delta"] is None
                                  else off["native"]["mean_delta"] <= OFFTOPIC_EXTRA),
            "off_topic_native_delta": off["native"]["mean_delta"] if off else None})
    if valid and all("ref_search" in r for r in valid):
        report["refined"] = refined_report(valid, arms, groups)
        report["refined"]["check"] = {"sessions": len(valid), "search_plus_reads_differs_from_native": sum(
            1 for r in valid if r["ref_search"] + r["ref_reads"] != r["native"])}
    report["noise"] = {name: replicate_sd(valid, ids) for name, ids in groups.items()}
    report["per_prompt"] = per_prompt(valid, arms)
    if rows:
        report["utilisation"] = {"first_five_hour": next((r["rl_five_hour"] for r in sorted(rows, key=lambda r: r["index"])
                                                          if r["rl_five_hour"] is not None), None),
                                 "last_five_hour": next((r["rl_five_hour"] for r in sorted(rows, key=lambda r: -r["index"])
                                                         if r["rl_five_hour"] is not None), None),
                                 "models": sorted({r["model"] for r in rows if r["model"]}),
                                 "total_cost_usd": round(sum(r["cost_usd"] or 0 for r in rows), 2),
                                 "load1_mean": mean([r["load1"] for r in rows])}
    return report


def fmt(value, digits=2):
    if value is None:
        return "n/a"
    if not isinstance(value, float):
        return str(value)
    text = f"{value:.{digits}f}"
    return text.rstrip("0").rstrip(".") if "." in text else text


def signed(value, digits=2):
    return "n/a" if value is None else f"{value:+.{digits}f}"


def delta_cell(entry, digits=2):
    if entry["mean_delta"] is None:
        return "n/a"
    ci = f" [{signed(entry['ci95'][0], digits)}, {signed(entry['ci95'][1], digits)}]" if entry["ci95"] else ""
    return f"{signed(entry['mean_delta'], digits)}{ci}"


def pair_cell(new, off, key, digits=2):
    return f"{fmt(new[key]['mean'], digits)} / {fmt(off[key]['mean'], digits) if off else 'n/a'}"


def render_refined(ref):
    lines = ["", "Refined split: searches, and reads of the files the brief named against other reads (before the first answer; "
             "SEARCH = Grep, Glob, Bash rg/grep/find/ls/fd/git grep; a read counts as brief-named when its file is one the "
             "session's own `[PIXEL:BRIEF]` names; `off` sessions have no brief, so all their reads are other):", "",
             "| arm / group | sessions | search | reads of brief-named | other reads | reads | reads of expected | "
             "reads of non-expected | reached expected | first read of expected (mean / median, reached) | output tok | "
             "turns mean / median |", "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- | ---: | --- |"]
    for name, s in ref["arms"].items():
        lines.append(f"| {name} | {s['runs']} | {fmt(s['ref_search']['mean'])} | {fmt(s['ref_reads_brief']['mean'])} | "
                     f"{fmt(s['ref_reads_other']['mean'])} | {fmt(s['ref_reads']['mean'])} | {fmt(s['ref_reads_expected']['mean'])} | "
                     f"{fmt(s['ref_reads_unexpected']['mean'])} | {fmt(s['ref_reached']['mean'])} (n {s['ref_reached']['n']}) | "
                     f"{fmt(s['ref_first_read_idx']['mean'])} / {fmt(s['ref_first_read_idx']['median'])} | "
                     f"{fmt(s['output_tokens']['mean'], 0)} | {fmt(s['num_turns']['mean'])} / {fmt(s['num_turns']['median'])} |")
    lines += ["", "`new` sessions by group, against `off` on the same prompts (each cell `new / off`, means):", "",
              "| `new` group | sessions | search | reads of brief-named | other reads | reads | reads of non-expected | "
              "reached expected | first read of expected | output tok | turns |",
              "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"]
    for name, entry in ref["tiers"].items():
        new, off = entry["new"], entry["off_same_prompts"]
        lines.append(f"| {name} | {new['runs']} | {pair_cell(new, off, 'ref_search')} | {pair_cell(new, off, 'ref_reads_brief')} | "
                     f"{pair_cell(new, off, 'ref_reads_other')} | {pair_cell(new, off, 'ref_reads')} | "
                     f"{pair_cell(new, off, 'ref_reads_unexpected')} | {pair_cell(new, off, 'ref_reached')} | {pair_cell(new, off, 'ref_first_read_idx')} | "
                     f"{pair_cell(new, off, 'output_tokens', 0)} | {pair_cell(new, off, 'num_turns')} |")
    lines += ["", "Paired by prompt (mean of a prompt's sessions per arm; `to` minus `from`; 95% bootstrap interval over prompts; "
              "the first-read and reached columns use the prompts where both arms have the value):", "",
              "| from to | group | prompts | search | reads of brief-named | other reads | reads | reads of non-expected | "
              "first read of expected | reached expected | output tok | turns |",
              "| --- | --- | ---: | --- | --- | --- | --- | --- | --- | --- | --- | --- |"]
    for c in ref["contrasts"]:
        lines.append(f"| {c['from']} to {c['to']} | {c['group']} | {c['ref_search']['prompts']} | {delta_cell(c['ref_search'])} | "
                     f"{delta_cell(c['ref_reads_brief'])} | {delta_cell(c['ref_reads_other'])} | {delta_cell(c['ref_reads'])} | "
                     f"{delta_cell(c['ref_reads_unexpected'])} | {delta_cell(c['ref_first_read_idx'])} (n {c['ref_first_read_idx']['prompts']}) | "
                     f"{delta_cell(c['ref_reached'])} | {delta_cell(c['output_tokens'], 0)} | {delta_cell(c['num_turns'])} |")
    check = ref["check"]
    lines += ["", f"Check: search + reads equals the earlier `native` in {check['sessions'] - check['search_plus_reads_differs_from_native']} "
              f"of {check['sessions']} sessions."]
    return lines


def render(report):
    lines = [f"runs {report['runs']}, valid {report['valid']}, invalid {len(report['invalid'])}"]
    for bad in report["invalid"]:
        lines.append(f"  invalid: {bad['arm']} {bad['prompt_id']} r{bad['rep']}: {', '.join(bad['problems'])}")
    lines += ["", "| arm / group | runs | prompts | brief fired | native mean | native median | native p90 | tools mean | "
              "pixel mean | input tok mean | output tok mean | wall s mean | wall s median | cost mean | cites expected |",
              "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"]
    for name, s in report["arms"].items():
        lines.append(f"| {name} | {s['runs']} | {s['prompts']} | {fmt(s['fired'])} | {fmt(s['native']['mean'])} | "
                     f"{fmt(s['native']['median'])} | {fmt(s['native']['p90'])} | {fmt(s['tools_total']['mean'])} | "
                     f"{fmt(s['pixel']['mean'])} | {fmt(s['input_total']['mean'], 0)} | {fmt(s['output_tokens']['mean'], 0)} | "
                     f"{fmt(s['wall_s']['mean'], 1)} | {fmt(s['wall_s']['median'], 1)} | {fmt(s['cost_usd']['mean'])} | "
                     f"{fmt(s['cites']['rate'])} (n {s['cites']['n']}) |")
    if report["tiers"]:
        lines += ["", "NEW runs by the brief's tier (and by whether it names an expected file), against OFF on the same prompts:", "",
                  "| NEW group | runs | native mean | native median | off native mean | off native median | tools mean | "
                  "input tok | output tok | wall s | cites | off cites |",
                  "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"]
        for tier, entry in report["tiers"].items():
            new, off = entry["new"], entry["off_same_prompts"]
            lines.append(f"| {tier} | {new['runs']} | {fmt(new['native']['mean'])} | {fmt(new['native']['median'])} | "
                         f"{fmt(off['native']['mean']) if off else 'n/a'} | {fmt(off['native']['median']) if off else 'n/a'} | "
                         f"{fmt(new['tools_total']['mean'])} | {fmt(new['input_total']['mean'], 0)} | "
                         f"{fmt(new['output_tokens']['mean'], 0)} | {fmt(new['wall_s']['mean'], 1)} | "
                         f"{fmt(new['cites']['rate'])} | {fmt(off['cites']['rate']) if off else 'n/a'} |")
    lines += ["", "Paired by prompt (mean of the prompt's valid runs; delta = to - from; 95% bootstrap interval of the mean delta):", "",
              "| from -> to | group | prompts | native delta [95%] | lower / equal / higher | tools delta | input tok delta | "
              "wall s delta | cost delta | cites delta |", "| --- | --- | ---: | --- | --- | ---: | ---: | ---: | ---: | ---: |"]
    for c in report["contrasts"]:
        n = c["native"]
        ci = f" [{fmt(n['ci95'][0])}, {fmt(n['ci95'][1])}]" if n["ci95"] else ""
        lines.append(f"| {c['from']} -> {c['to']} | {c['group']} | {n['prompts']} | {fmt(n['mean_delta'])}{ci} | "
                     f"{n['lower']} / {n['equal']} / {n['higher']} | {fmt(c['tools_total']['mean_delta'])} | "
                     f"{fmt(c['input_total']['mean_delta'], 0)} | {fmt(c['wall_s']['mean_delta'], 1)} | "
                     f"{fmt(c['cost_usd']['mean_delta'])} | {fmt(c['cites_expected']['mean_delta'])} |")
    if report["criteria"]:
        lines += ["", f"Criteria (docs/bench/brief-ab.md): searches less = on-topic native delta < 0 with the 95% interval below 0; "
                  f"answers not worse = citation-rate delta >= -{NONINFERIORITY}; off-topic cost = native delta <= +{OFFTOPIC_EXTRA}.", ""]
        for c in report["criteria"]:
            lines.append(f"- {c['pair']}: searches less {c['searches_less']} (delta {fmt(c['native_delta'])}, "
                         f"CI {c['native_ci95']}); answers not worse {c['answers_not_worse']} (cites delta "
                         f"{fmt(c['cites_delta'])}); off-topic cost ok {c['off_topic_cost_ok']} "
                         f"(delta {fmt(c['off_topic_native_delta'])})")
    if report.get("refined"):
        lines += render_refined(report["refined"])
    arms = [a for a in ARMS if report["per_prompt"] and a in report["per_prompt"][0]]
    if report["per_prompt"]:
        lines += ["", "Per prompt (`native` of each repetition; brief: tier, `y` fired without a tier, `-` silent; "
                  "cites: Y named an expected file, n did not, - no expected file):", "",
                  "| prompt | " + " | ".join(f"{a} native" for a in arms) + " | " + " | ".join(f"{a} brief" for a in arms if a != "off")
                  + " | cites " + "/".join(arms) + " |", "| --- |" + " ---: |" * len(arms) + " --- |" * (len(arms) - 1) + " --- |"]
        for e in report["per_prompt"]:
            lines.append(f"| {e['prompt_id']}{'' if e['on_topic'] else ' (off)'} | "
                         + " | ".join(",".join(map(str, e[a]["native"])) for a in arms) + " | "
                         + " | ".join(",".join(e[a]["brief"]) for a in arms if a != "off") + " | "
                         + "/".join(e[a]["cites"] for a in arms) + " |")
    noise = report.get("noise")
    if noise:
        lines += ["", f"Run-to-run standard deviation of `native` between repetitions of the same prompt and arm: "
                  f"on-topic {noise['on-topic']}, off-topic {noise['off-topic']}."]
    use = report.get("utilisation")
    if use:
        lines += ["", f"models {use['models']}; total list cost ${use['total_cost_usd']}; five-hour utilisation "
                  f"{use['first_five_hour']} -> {use['last_five_hour']}; mean 1-minute load at launch {use['load1_mean']}"]
    return "\n".join(lines)


def attach_refined(rows, out, manifest, work, set_path):
    """Add the ``ref_*`` metrics of each row from its saved raw stream (nothing is run)."""
    expected = {row["id"]: row.get("expected_files") or [] for row in load_set(set_path)[0]}
    for row in rows:
        raw = out / "raw" / row["arm"] / f"{row['prompt_id']}-r{row['rep']}.stream.jsonl"
        if not raw.exists():
            continue
        fixture = ((manifest.get("sides") or {}).get(row["side"]) or {}).get("fixture") or work.fixture(row["side"])
        row.update(refine_file(raw, expected[row["prompt_id"]], path_roots(fixture)))


def cmd_report(args):
    out = Path(args.results) / args.run_id
    rows = read_rows(out / "runs.jsonl")
    if not rows:
        die(f"no runs in {out / 'runs.jsonl'}")
    manifest = json.loads((out / "manifest.json").read_text()) if (out / "manifest.json").exists() else {}
    attach_refined(rows, out, manifest, Work(args.work), args.set)
    report = build_report(rows, manifest)
    print(render(report))
    if args.receipt:
        replacements = [(str(Path(args.work).resolve()), "$WORK"), (str(Path(args.work)), "$WORK"),
                        ("/private" + str(Path(args.work)), "$WORK"), (str(REPO), "$REPO"), (str(Path.home()), "~")]
        slim = [{**{k: r.get(k) for k in RECEIPT_COLUMNS},
                 "hook_ms": next((h["ms"] for h in r["hooks"] if h["event"] == "UserPromptSubmit"), None)}
                for r in sorted(rows, key=lambda r: r["index"])]
        receipt = redact({"manifest": manifest, "report": report, "runs": slim}, replacements)
        body = ",\n".join("  " + json.dumps(run, sort_keys=True, separators=(",", ":")) for run in receipt["runs"])
        parts = [' "manifest": ' + json.dumps(receipt["manifest"], indent=1, sort_keys=True).replace("\n", "\n "),
                 ' "report": ' + json.dumps(receipt["report"], sort_keys=True, separators=(",", ":")),
                 ' "runs": [\n' + body + "\n ]"]
        Path(args.receipt).parent.mkdir(parents=True, exist_ok=True)
        Path(args.receipt).write_text("{\n" + ",\n".join(parts) + "\n}\n")
        print(f"\nreceipt: {args.receipt} ({Path(args.receipt).stat().st_size} bytes, no transcripts or answers)")
    return 0


# --------------------------------------------------------------------------
# Tests
# --------------------------------------------------------------------------

def stream(*records):
    return [(0.1 * i, json.dumps(r)) for i, r in enumerate(records)]


def init_record(tools=TOOLS.split(",")):
    return {"type": "system", "subtype": "init", "tools": tools, "mcp_servers": [], "skills": [],
            "model": "claude-test", "apiKeySource": "none", "claude_code_version": "9.9"}


def hook_pair(event, stdout, hook_id="h1", exit_code=0):
    return [{"type": "system", "subtype": "hook_started", "hook_id": hook_id, "hook_event": event},
            {"type": "system", "subtype": "hook_response", "hook_id": hook_id, "hook_event": event,
             "stdout": stdout, "exit_code": exit_code, "outcome": "success"}]


def tool_use(ident, name, **inputs):
    return {"type": "assistant", "message": {"content": [{"type": "tool_use", "id": ident, "name": name, "input": inputs}]}}


def text(value):
    return {"type": "assistant", "message": {"content": [{"type": "text", "text": value}]}}


BRIEF_OUT = json.dumps({"hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": (
    "[PIXEL:BRIEF]\nkind: lookup\nfiles: crates/a/src/lib.rs:3 — x\nconfidence: high — 4/4 key terms\n"
    "coverage: 2/2 ops answered\nAnswer from this evidence.")}})


class SelfTest(unittest.TestCase):
    def good_stream(self, hook_stdout="{}", extra=()):
        return stream(init_record(), *hook_pair("UserPromptSubmit", hook_stdout), tool_use("t1", "Grep", pattern="x"),
                      tool_use("t2", "Bash", command="rg -n foo crates"), tool_use("t3", "Bash", command="cargo --version"),
                      text("see crates/a/src/lib.rs:3 and ./docs/x.md."),
                      {"type": "rate_limit_event", "rate_limit_info": {"status": "allowed", "unifiedWindows": {
                          "five_hour": {"utilization": 0.5}, "seven_day": {"utilization": 0.6}}}},
                      *extra,
                      {"type": "result", "subtype": "success", "is_error": False, "num_turns": 3,
                       "total_cost_usd": 0.5, "terminal_reason": "completed", "permission_denials": [],
                       "result": "see crates/a/src/lib.rs:3 and ./docs/x.md.",
                       "usage": {"input_tokens": 4, "cache_creation_input_tokens": 100, "cache_read_input_tokens": 50,
                                 "output_tokens": 7}})

    def test_off_arm_run_counts_native_calls_and_citations(self):
        row = analyse(self.good_stream(), ["crates/a/src/lib.rs"], ["/tmp/x"], brief_on=False)
        self.assertEqual((row["native"], row["tools_total"], row["brief_fired"], row["problems"]), (2, 3, False, []))
        self.assertEqual((row["cites_expected"], row["cited"], row["input_total"], row["rl_five_hour"]),
                         (True, ["crates/a/src/lib.rs"], 154, 0.5))
        self.assertEqual(row["by_bucket"], {"explore": 2, "other": 1})

    def test_brief_is_read_from_the_hook_json_and_carries_its_tier(self):
        row = analyse(self.good_stream(BRIEF_OUT), ["crates/a/src/lib.rs"], [], brief_on=True)
        self.assertTrue(row["brief_fired"])
        self.assertEqual((row["brief_tier"], row["brief_answered"], row["brief_ops"], row["brief_names_expected"]),
                         ("high", 2, 2, True))
        self.assertEqual(row["problems"], [])

    def test_brief_text_outside_the_hook_does_not_count(self):
        records = stream(init_record(), *hook_pair("UserPromptSubmit", "{}"), text("[PIXEL:BRIEF] quoted"),
                         {"type": "result", "subtype": "success", "result": "x", "usage": {}})
        self.assertFalse(analyse(records, brief_on=True)["brief_fired"])

    def test_off_arm_that_fires_and_foreign_hooks_are_invalid(self):
        fired = analyse(self.good_stream(BRIEF_OUT), brief_on=False)
        self.assertIn("off-arm-fired", fired["problems"])
        foreign = analyse(stream(init_record(), *hook_pair("SessionStart", "{}", "h0"),
                                 *hook_pair("UserPromptSubmit", "{}", "h1"),
                                 {"type": "result", "subtype": "success", "result": "x", "usage": {}}))
        self.assertTrue(any(p.startswith("hooks:SessionStart") for p in foreign["problems"]))

    def test_errors_and_extra_tools_are_invalid(self):
        bad = analyse(stream(init_record(TOOLS.split(",") + ["WebFetch"]), *hook_pair("UserPromptSubmit", "{}"),
                             {"type": "result", "subtype": "error_max_budget_usd", "is_error": True, "result": "", "usage": {}}))
        self.assertIn("tools", bad["problems"])
        self.assertIn("result:error_max_budget_usd", bad["problems"])
        self.assertIn("no-result", analyse(stream(init_record(), *hook_pair("UserPromptSubmit", "{}")))["problems"])

    def test_subagent_records_and_duplicate_tool_ids_are_ignored(self):
        records = stream(init_record(), *hook_pair("UserPromptSubmit", "{}"), tool_use("t1", "Grep", pattern="x"),
                         tool_use("t1", "Grep", pattern="x"),
                         {**tool_use("t9", "Grep", pattern="y"), "parent_tool_use_id": "p"}, text("done"),
                         {"type": "result", "subtype": "success", "result": "done", "usage": {}})
        self.assertEqual(analyse(records)["tools_total"], 1)

    def test_citations_are_exact_paths_after_normalisation(self):
        roots = path_roots("/private/tmp/w/old/pixel")
        expected = ["crates/pixel-daemon/src/daemon.rs", ".github/workflows/board-sync.yml"]
        answer = ("`/tmp/w/old/pixel/crates/pixel-daemon/src/daemon.rs:29`, (.github/workflows/board-sync.yml), "
                  "and pixel-daemon/src/daemon.rs")
        exact, loose = cited_paths(answer, expected, roots)
        self.assertEqual(exact, sorted(expected))
        self.assertEqual(cited_paths("only daemon.rs is relevant", expected, roots), ([], True))
        self.assertEqual(cited_paths("crates/pixel-daemon/src/daemon.rs.bak", expected, roots), ([], False))
        self.assertEqual(cited_paths("", expected, roots), ([], False))

    def test_selection_is_deterministic_plain_and_never_mutating(self):
        rows, _ = load_set()
        on, off = select_prompts(rows)
        self.assertEqual((len(on), len(off)), (16, 4))
        self.assertTrue(all(r["kind"] == "plain" and r["on_topic"] and r["expected_files"] and r["split"] == "test"
                            and r["lang"] == "en" for r in on))
        self.assertTrue(all(not r["on_topic"] and not MUTATING.search(r["text"]) for r in off))
        self.assertEqual({r["kind"] for r in off}, set(OFF_KINDS))
        self.assertEqual([r["id"] for r in on], [r["id"] for r in select_prompts(rows)[0]])
        self.assertNotEqual([r["id"] for r in on], [r["id"] for r in select_prompts(rows, seed="other")[0]])

    def test_schedule_balances_arms_and_keeps_repetitions_together(self):
        units = schedule(["a", "b", "c"], ["off", "old", "new"], 2, 7)
        self.assertEqual(len(units), 18)
        self.assertEqual({u[1] for u in units[:9]}, {1})
        self.assertEqual(units, schedule(["a", "b", "c"], ["off", "old", "new"], 2, 7))
        for rep in (1, 2):
            for prompt in "abc":
                self.assertEqual(sorted(u[2] for u in units if u[0] == prompt and u[1] == rep), ["new", "off", "old"])

    def test_environment_is_scrubbed_and_only_the_off_arm_turns_the_brief_off(self):
        base = {"HOME": "/h", "PATH": "/a:/b:/a", "ANTHROPIC_API_KEY": "k", "CLAUDE_EFFORT": "xhigh", "BASH_ENV": "/x",
                "PIXEL_BRIEF": "1", "OUTLINE_API_KEY": "s"}
        on = agent_env(base, "/side", True)
        off = agent_env(base, "/side", False)
        self.assertEqual(sorted(on), ["DISABLE_AUTOUPDATER", "HOME", "PATH", "PIXEL_DAEMON_AUTO_START"])
        self.assertEqual(on["PATH"], "/side:/a:/b")
        self.assertEqual({k: v for k, v in off.items() if k not in on}, {"PIXEL_BRIEF": "0"})

    def test_agent_command_isolates_settings_and_tools(self):
        argv = claude_argv("claude", "s.json", "m", "high", 2)
        pairs = dict(zip(argv, argv[1:]))
        self.assertEqual((pairs["--setting-sources"], pairs["--tools"], pairs["--permission-mode"]),
                         ("project,local", TOOLS, "dontAsk"))
        for flag in ("--strict-mcp-config", "--disable-slash-commands", "--no-session-persistence", "--include-hook-events"):
            self.assertIn(flag, argv)
        self.assertNotIn("--bare", argv)
        settings = arm_settings("/b dir/pixel")
        self.assertEqual(list(settings["hooks"]), ["UserPromptSubmit"])
        self.assertEqual(shlex.split(settings["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"])[:2],
                         ["/b dir/pixel", "run-hook"])

    def test_report_pairs_by_prompt_and_applies_the_criteria(self):
        rows, index = [], 0
        for prompt, base in (("p1", 6), ("p2", 8), ("p3", 5), ("p4", 7)):
            for arm, drop in (("off", 0), ("new", 3)):
                for rep in (1, 2):
                    index += 1
                    rows.append({"arm": arm, "prompt_id": prompt, "rep": rep, "index": index, "ok": True, "problems": [],
                                 "on_topic": True, "native": base - drop + rep % 2, "tools_total": base, "pixel": 0,
                                 "input_total": 100, "output_tokens": 5, "wall_s": 10.0, "cost_usd": 0.1,
                                 "brief_fired": arm == "new", "brief_tier": "high" if arm == "new" else None,
                                 "brief_names_expected": arm == "new",
                                 "cites_expected": True, "rl_five_hour": 0.5, "model": "m", "load1": 3.0})
        rows.append({**rows[0], "prompt_id": "q1", "on_topic": False, "cites_expected": None, "index": 99})
        report = build_report(rows)
        contrast_row = next(c for c in report["contrasts"] if c["to"] == "new" and c["group"] == "on-topic")
        self.assertEqual((contrast_row["native"]["prompts"], contrast_row["native"]["mean_delta"]), (4, -3))
        self.assertEqual(contrast_row["native"]["lower"], 4)
        self.assertTrue(report["criteria"][0]["searches_less"] and report["criteria"][0]["answers_not_worse"])
        self.assertIn("on-topic: high", report["tiers"])
        self.assertEqual(report["per_prompt"][0]["new"]["brief"], ["high", "high"])
        self.assertEqual(report["noise"]["on-topic"], 0.71)
        self.assertIn("native mean", render(report))

    def test_bash_calls_are_split_into_searches_and_reads(self):
        roots = ["/private/w/fx"]
        cases = [("Grep", {"pattern": "x"}, ("search", set())), ("Glob", {"pattern": "*.rs"}, ("search", set())),
                 ("Bash", {"command": "rg -n foo crates | head"}, ("search", set())),
                 ("Bash", {"command": "ls -la crates/pixel"}, ("search", set())),
                 ("Bash", {"command": "git grep -n foo"}, ("search", set())),
                 ("Bash", {"command": "rg foo && cat crates/a.rs"}, ("search", set())),
                 ("Bash", {"command": "cat crates/a.rs"}, ("read", {"crates/a.rs"})),
                 ("Bash", {"command": "sed -n '1,50p' ./crates/a.rs"}, ("read", {"crates/a.rs"})),
                 ("Bash", {"command": "head -n 20 /private/w/fx/crates/a.rs"}, ("read", {"crates/a.rs"})),
                 ("Bash", {"command": "cd crates && cat a.rs b.rs"}, ("read", {"a.rs", "b.rs"})),
                 ("Bash", {"command": "cargo test | grep FAIL"}, ("other", set())),
                 ("Bash", {"command": "echo hi > out.txt"}, ("edit", set())),
                 ("Read", {"file_path": "/private/w/fx/crates/a.rs"}, ("read", {"crates/a.rs"})),
                 ("Read", {}, ("read", set()))]
        for name, tool_input, want in cases:
            self.assertEqual(tool_kind(name, tool_input, roots), want, (name, tool_input))

    def test_refine_separates_reads_of_brief_named_files_and_finds_the_first_expected_read(self):
        roots = path_roots("/private/tmp/w/new/pixel")
        run = collect(stream(
            init_record(), *hook_pair("UserPromptSubmit", BRIEF_OUT), tool_use("t1", "Grep", pattern="x"),
            tool_use("t2", "Read", file_path="/tmp/w/new/pixel/crates/a/src/lib.rs"),
            tool_use("t3", "Read", file_path="/private/tmp/w/new/pixel/crates/b/other.rs"),
            tool_use("t4", "Bash", command="cat ./crates/c.rs"), tool_use("t5", "Edit", file_path="x"),
            tool_use("t6", "Read", file_path="crates/a/src/lib.rs"), text("done")))
        first = next(h for h in run["hooks"] if h["event"] == "UserPromptSubmit")
        got = refine(run["events"], hook_context(first["stdout"]), ["crates/a/src/lib.rs"], roots)
        self.assertEqual((got["ref_search"], got["ref_reads_brief"], got["ref_reads_other"], got["ref_reads_expected"]),
                         (1, 1, 2, 1))
        self.assertEqual((got["ref_calls"], got["ref_first_read_idx"], got["ref_first_touch_idx"], got["ref_reached"],
                          got["ref_reads_unexpected"]), (4, 2, 2, 1, 2))
        silent = refine(run["events"], None, ["crates/zzz.rs"], roots)
        self.assertEqual((silent["ref_reads_brief"], silent["ref_reads_other"], silent["ref_reached"]), (0, 3, 0))
        self.assertIsNone(silent["ref_first_read_idx"])
        off_topic = refine(run["events"], None, [], roots)
        self.assertIsNone(off_topic["ref_reached"])
        self.assertIsNone(off_topic["ref_reads_unexpected"])

    def test_brief_named_paths_skip_the_excluded_line_and_non_briefs(self):
        context = json.loads(BRIEF_OUT)["hookSpecificOutput"]["additionalContext"] + "\nexcluded (generated): a/skip.json"
        named = brief_named_paths(context)
        self.assertIn("crates/a/src/lib.rs", named)
        self.assertNotIn("a/skip.json", named)
        self.assertEqual(brief_named_paths("not a brief crates/a/src/lib.rs"), set())

    def test_refined_report_pairs_the_split_by_prompt(self):
        rows, index = [], 0
        for prompt in ("p1", "p2", "p3"):
            for arm in ("off", "new"):
                for rep in (1, 2):
                    index += 1
                    new = arm == "new"
                    rows.append({"arm": arm, "prompt_id": prompt, "rep": rep, "index": index, "ok": True, "problems": [],
                                 "on_topic": True, "native": 5 if not new else 4, "tools_total": 5, "pixel": 0,
                                 "input_total": 100, "output_tokens": 50, "wall_s": 9.0, "cost_usd": 0.1,
                                 "brief_fired": new, "brief_tier": "high" if new else None, "brief_names_expected": new,
                                 "cites_expected": True, "rl_five_hour": 0.5, "model": "m", "load1": 1.0,
                                 "num_turns": 6 if not new else 5, "ref_search": 3 if not new else 3,
                                 "ref_reads_brief": 1 if new else 0, "ref_reads_other": 2 if not new else 0,
                                 "ref_reads": 2 if not new else 1, "ref_reads_expected": 1, "ref_reads_unexpected": 1 if not new else 0,
                                 "ref_first_read_idx": 3 if not new else 1, "ref_reached": 1})
        refined = build_report(rows)["refined"]
        contrast_row = next(c for c in refined["contrasts"] if c["to"] == "new" and c["group"] == "on-topic")
        self.assertEqual((contrast_row["ref_search"]["mean_delta"], contrast_row["ref_reads_brief"]["mean_delta"],
                          contrast_row["ref_reads_other"]["mean_delta"], contrast_row["ref_first_read_idx"]["mean_delta"]),
                         (0, 1, -2, -2))
        self.assertEqual(contrast_row["ref_reads_unexpected"]["mean_delta"], -1)
        self.assertEqual(refined["tiers"]["on-topic: high"]["new"]["ref_reads_brief"]["mean"], 1)
        self.assertEqual(refined["check"]["search_plus_reads_differs_from_native"], 0)
        self.assertIn("reads of brief-named", render(build_report(rows)))

    def test_number_format_never_strips_integer_zeros(self):
        self.assertEqual((fmt(1990.0, 0), fmt(0.20), fmt(22.0, 1), fmt(None), fmt(5)), ("1990", "0.2", "22", "n/a", "5"))

    def test_bootstrap_interval_brackets_the_mean(self):
        deltas = [-3, -2, -4, -3, -1, -5]
        low, high = bootstrap_ci(deltas)
        self.assertLess(low, statistics.mean(deltas))
        self.assertGreater(high, statistics.mean(deltas))
        self.assertIsNone(bootstrap_ci([1]))

    def test_redaction_replaces_paths_in_nested_values(self):
        self.assertEqual(redact({"a": ["/w/x", {"b": "/w"}]}, [("/w", "$WORK")]), {"a": ["$WORK/x", {"b": "$WORK"}]})


def self_test():
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(SelfTest)
    result = unittest.TextTestRunner(verbosity=1).run(suite)
    return 0 if result.wasSuccessful() else 1


# --------------------------------------------------------------------------
# Command line
# --------------------------------------------------------------------------

def common(parser):
    parser.add_argument("--work", default=str(DEFAULT_WORK), help="scratch directory (binaries, fixtures)")
    parser.add_argument("--results", default=str(DEFAULT_RESULTS), help="results directory (gitignored)")
    parser.add_argument("--set", default=str(DEFAULT_SET))


def agent_flags(parser):
    parser.add_argument("--claude", default=shutil.which("claude") or "claude", help="the real claude binary, not a shell function")
    parser.add_argument("--model", default=None, help="default: the CLI's own")
    parser.add_argument("--effort", default=None, help="default: the CLI's own")
    parser.add_argument("--budget", type=float, default=2.0, help="--max-budget-usd per run")
    parser.add_argument("--timeout", type=int, default=900, help="seconds per run before it is killed")


def build_parser():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--self-test", action="store_true")
    sub = parser.add_subparsers(dest="command")
    setup = sub.add_parser("setup", help="copy and hash a binary, clone and index its fixture, start its daemon, warm the brief")
    common(setup)
    setup.add_argument("--side", choices=SIDES, required=True)
    setup.add_argument("--binary", required=True)
    setup.add_argument("--expect-sha", default=None, help="the commit the binary must report in `--version`")
    setup.add_argument("--source", default=None, help="repository to clone the fixture from (default: this checkout)")
    setup.add_argument("--force", action="store_true", help="replace a binary of another hash (never during a run)")
    select = sub.add_parser("select", help="print the prompt rows a run would use")
    common(select)
    for target in (select,):
        target.add_argument("--split", choices=("dev", "test"), default="test")
        target.add_argument("--n-on", type=int, default=16)
        target.add_argument("--n-off", type=int, default=4)
        target.add_argument("--select-seed", default=SELECT_SEED)
    probe = sub.add_parser("probe", help="isolation and delivery evidence on the real claude")
    common(probe)
    agent_flags(probe)
    probe.add_argument("--run-id", required=True)
    probe.add_argument("--arms", default="off,old")
    run = sub.add_parser("run", help="the campaign")
    common(run)
    agent_flags(run)
    run.add_argument("--run-id", required=True)
    run.add_argument("--arms", default="off,old,new")
    run.add_argument("--reps", type=int, default=2)
    run.add_argument("--split", choices=("dev", "test"), default="test")
    run.add_argument("--n-on", type=int, default=16)
    run.add_argument("--n-off", type=int, default=4)
    run.add_argument("--select-seed", default=SELECT_SEED)
    run.add_argument("--prompts", default=None, help="comma-separated row ids instead of the selection")
    run.add_argument("--seed", type=int, default=883, help="order of the runs")
    run.add_argument("--concurrency", type=int, default=4)
    run.add_argument("--max-utilization", type=float, default=0.9, help="stop launching at this five-hour utilisation")
    run.add_argument("--retry-failed", action="store_true", help="rerun units whose earlier row is invalid")
    run.add_argument("--plan", action="store_true", help="print the plan and exit")
    report = sub.add_parser("report", help="tables, contrasts and criteria of a finished run")
    common(report)
    report.add_argument("--run-id", required=True)
    report.add_argument("--receipt", default=None, help="write the compact redacted receipt here")
    return parser


def main(argv=None):
    args = build_parser().parse_args(argv)
    if args.self_test:
        return self_test()
    handlers = {"setup": cmd_setup, "select": cmd_select, "probe": cmd_probe, "run": cmd_run, "report": cmd_report}
    if args.command not in handlers:
        build_parser().print_help()
        return 2
    os.environ.pop("ANTHROPIC_API_KEY", None)  # banned: auth is the claude CLI's own OAuth
    return handlers[args.command](args)


if __name__ == "__main__":
    sys.exit(main())
