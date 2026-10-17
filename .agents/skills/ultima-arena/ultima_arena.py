#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Ultima Arena: do the brief and the metrics reach every agent CLI?

Installs Pixel from the repository, starts Claude Code, Codex, pi, agy and
Devin in one Herdr grid (random order) in the operator's tab, sends each one
the same neutral prompt, and judges each CLI from evidence only:

* brief:   `<repo>/.pixel/brief-feedback.jsonl` holds a `prompt` line and a
           `part` line (bytes > 0) of that CLI's provider, written after the
           prompt was sent;
* metrics: the CLI's scrollback shows a `🟩 pixel search-content ... #<id>`
           box whose id is the one of a `search-content` record that
           `<repo>/.pixel/actions.jsonl` gained after the prompt was sent.

The prompt never mentions metrics and the model's answer is never scored: a
model asked about metrics would say yes. See SKILL.md.

    ultima_arena.py [run] [--repo P] [--skip-install] [--seed N] [--clis a,b]
                    [--timeout S] [--out DIR]
    ultima_arena.py abort [--out DIR]
    ultima_arena.py --self-test
"""

from __future__ import annotations

import argparse
import contextlib
import dataclasses
import datetime
import fcntl
import json
import os
import pathlib
import random
import re
import shlex
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest

HERE = pathlib.Path(__file__).resolve().parent

# ---------------------------------------------------------------- constants

SEARCH_TERM = "start_brief"
SEARCH_PATH = "crates/pixel/src"
SHELL_CMD = f"pixel search-content -F {SEARCH_TERM} {SEARCH_PATH}"
# The shell command comes first: a model handed a brief that says "answer from
# this context" skipped a command asked for after the question (pi, run 1).
PROMPT = (
    "This request tests a tool invocation, not whether you can answer from context. "
    "Even if the brief or prior context already contains the answer, you must run the exact command below once "
    f"with your shell tool and wait for its full output: `{SHELL_CMD}`. Do not use Git history or existing context "
    f"as a substitute. Only after the command returns, answer in two lines with file:line: where is `{SEARCH_TERM}` "
    "defined and what calls it? If you cannot run the command, say `NOT RUN`; do not answer from memory. "
    "Do not edit any file."
)
# The prompt must never lead the model toward the thing being measured.
PROMPT_FORBIDDEN = re.compile(r"metric|🟩|❀", re.IGNORECASE)

BUILD_CMD = "cargo build --profile dev-release -p pixel-cli"
NAME_PREFIX = "ultima-"
# Herdr reports a turn that ended as `done` (Claude, Codex, agy, Devin) or `idle` (pi).
FINISHED = ("idle", "done")
# Polls a screen must hold still before its turn counts as ended: Herdr sees no
# state change on pi, so the screen is the only signal there.
STABLE_POLLS = 3
OPERATOR_SHARE = 0.3
FEEDBACK_FILE = "brief-feedback.jsonl"
ACTIONS_FILE = "actions.jsonl"
BRIEF_LOG_OFF = ("0", "false", "off")  # decision_log::enabled
SCROLLBACK_LINES = 2000
DEFAULT_SEED = 42


@dataclasses.dataclass(frozen=True)
class Cli:
    name: str  # what the operator types in --clis
    kind: str  # `herdr agent start --kind`
    provider: str  # `TaskProvider::as_str` in crates/pixel/src/task_hook.rs


CLIS = {
    c.name: c
    for c in (
        Cli("claude", "claude", "claude"),
        Cli("codex", "codex", "codex"),
        Cli("pi", "pi", "pi"),
        Cli("agy", "agy", "antigravity"),
        Cli("devin", "devin", "devin"),
    )
}


class UltimaError(Exception):
    """A usage or environment problem: the run cannot start (exit 2)."""


@contextlib.contextmanager
def arena_lock(directory: pathlib.Path):
    """Only one runner may change the arena grid or judge its shared logs."""
    directory.mkdir(parents=True, exist_ok=True)
    with (directory / "run.lock").open("a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise UltimaError("another Ultima Arena run is active; wait for it to finish or interrupt that command") from error
        try:
            yield
        finally:
            fcntl.flock(lock, fcntl.LOCK_UN)


# ---------------------------------------------------------------- the prompt


def guard_prompt(text: str) -> str:
    """Refuse a prompt that names the measurement; return it otherwise."""
    hit = PROMPT_FORBIDDEN.search(text)
    if hit:
        raise UltimaError(f"prompt names the measurement ({hit.group(0)!r}); a model's word on metrics is a false positive")
    return text


def shuffled(names: list[str], seed: int) -> list[str]:
    order = list(names)
    random.Random(seed).shuffle(order)
    return order


# ---------------------------------------------------------------- evidence


def read_jsonl(path: pathlib.Path) -> list[dict]:
    """Every JSON object line of `path`; a missing file or a torn line is skipped."""
    try:
        raw = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return []
    rows = []
    for line in raw.splitlines():
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if isinstance(row, dict):
            rows.append(row)
    return rows


def short_id(invocation_id: str) -> str:
    """The `#id` Pixel displays: the last six characters of the middle field.

    Mirrors `format_metrics_line` in crates/pixel-actionlog/src/metrics.rs:
    `id.split('-').nth(1)` last six chars, or the whole id without a dash.
    """
    parts = invocation_id.split("-")
    return parts[1][-6:] if len(parts) > 1 else invocation_id


def judge_brief(rows: list[dict], provider: str, since_ms: int) -> dict:
    """PASS when a `prompt` line and a `part` line (bytes > 0) of `provider` came after `since_ms`."""
    mine = [r for r in rows if r.get("provider") == provider and isinstance(r.get("ts"), (int, float)) and r["ts"] >= since_ms]
    prompts = [r for r in mine if r.get("t") == "prompt"]
    parts = [r for r in mine if r.get("t") == "part" and isinstance(r.get("bytes"), (int, float)) and r["bytes"] > 0]
    same_session = any(p.get("session") == q.get("session") for p in prompts for q in parts)
    return {
        "verdict": "PASS" if prompts and parts else "FAIL",
        "prompt_lines": len(prompts),
        "part_lines": len(parts),
        "part_bytes": sum(int(q["bytes"]) for q in parts),
        "prompt_bytes": [int(p["bytes"]) for p in prompts if isinstance(p.get("bytes"), (int, float))],
        "kinds": sorted({str(p.get("kind")) for p in prompts if p.get("kind")}),
        "injected": sorted({str(i) for p in prompts for i in p.get("injected", [])})[:8],
        "late_parts": sum(1 for q in parts if q.get("late")),
        "same_session": same_session,
        "sessions": sorted({str(p["session"]) for p in prompts if p.get("session")}),
    }


BOX = re.compile(r"🟩 pixel (?P<op>[\w-]+)[^\n]*?❀ (?P<ms>[0-9.]+)ms ❀ #(?P<id>[0-9A-Fa-f]+)")
FRAMING = ("│", "⎿", "└", "┃", "|", "┆")


# A narrow pane hard-wraps a header over two or three lines (Codex, Claude): the
# flat pass joins every line break, with the framing that follows it, into one space.
WRAP = re.compile(r"[ \t]*\n[ \t│┃⎿└├|┆]*")
HYPHEN_WRAP = re.compile(r"-[ \t]*\n[ \t│┃⎿└├|┆]*")
BOX_FLAT = re.compile(
    r"🟩 ?pixel (?P<op>[\w-]+) [^🟩]*?❀ ?(?P<ms>[0-9.]+) ?ms ?❀ ?#(?P<id>(?:[0-9A-Fa-f] ?){6})"
)


def flatten_wrapped(scrollback: str) -> str:
    """Join pane wraps without inserting whitespace inside a visually split token."""
    return WRAP.sub(" ", HYPHEN_WRAP.sub("-", scrollback))


def parse_boxes(scrollback: str) -> list[dict]:
    """Every `🟩 pixel <op> ❀ <ms>ms ❀ #<id>` header; `framed` when it sits inside tool-output framing.

    Headers on one line come first; a header only the flat pass finds was wrapped
    by the pane, and its framing is unknown (`framed` None)."""
    boxes = []
    for line in scrollback.splitlines():
        m = BOX.search(line)
        if m:
            boxes.append({"op": m["op"], "ms": float(m["ms"]), "id": m["id"].lower(), "line": line.strip(), "framed": line.strip().startswith(FRAMING)})
    whole = {(b["op"], b["id"]) for b in boxes}
    for m in BOX_FLAT.finditer(flatten_wrapped(scrollback)):
        key = (m["op"], m["id"].replace(" ", "").lower())
        if key not in whole:
            whole.add(key)
            boxes.append({"op": key[0], "ms": float(m["ms"]), "id": key[1], "line": m[0], "framed": None})
    return boxes


SUBMIT_FLAT = re.compile(r"🟩 ?pixel prompt-submit ?❀ ?[0-9.]+ ?ms")


def prompt_submit_line(scrollback: str) -> str | None:
    """The `🟩 pixel prompt-submit ❀ Xms` relay (Claude, Codex, pi), when on screen: the whole
    line when it fits, else the header with the pane's line breaks joined (Claude wraps it)."""
    for line in scrollback.splitlines():
        if SUBMIT_FLAT.search(line):
            return line.strip()
    m = SUBMIT_FLAT.search(flatten_wrapped(scrollback))
    return m[0] if m else None


def turn_records(actions: list[dict], since_ms: int, until_ms: int | None = None) -> list[dict]:
    """The `search-content` records of one turn: written at or after `since_ms` and, when the turn
    ended, by `until_ms`. Concurrent turns may share this time window; the pane's displayed
    invocation id, not the time window alone, attributes a relay to a CLI."""
    return [
        r for r in actions
        if r.get("command") == "search-content" and isinstance(r.get("ts_ms"), (int, float))
        and r["ts_ms"] >= since_ms and (until_ms is None or r["ts_ms"] <= until_ms)
        and isinstance(r.get("invocation_id"), str)
    ]


def judge_metrics(scrollback: str, actions: list[dict], since_ms: int, until_ms: int | None = None) -> dict:
    """PASS when a displayed `search-content` box carries the id of a record of this turn."""
    records = {}
    for r in turn_records(actions, since_ms, until_ms):
        records[short_id(r["invocation_id"]).lower()] = r
    boxes = [b for b in parse_boxes(scrollback) if b["op"] == "search-content"]
    matched = [b for b in boxes if b["id"] in records]
    return {
        "verdict": "PASS" if matched else "FAIL",
        "seen": bool(matched),
        "occurrences": len(matched),
        # None when no matched header sat whole on one line: a wrapped header's framing is unknown.
        "outside_tool_framing": (any(not b["framed"] for b in known) if (known := [b for b in matched if b["framed"] is not None]) else None),
        "box_lines": [b["line"] for b in matched][:6],
        "box_ids_without_record": sorted({b["id"] for b in boxes if b["id"] not in records}),
        "record_ids": sorted(records),
        "record_found": bool(records),
    }


def _pi_timestamp_ms(value) -> int | None:
    if isinstance(value, (int, float)):
        return int(value)
    if isinstance(value, str):
        try:
            return int(datetime.datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp() * 1000)
        except ValueError:
            return None
    return None


def _is_arena_search_call(command: str) -> bool:
    """Accept only the exact search command, optionally prefixed with RTK."""
    try:
        argv = shlex.split(command)
    except ValueError:
        return False
    if argv[:1] == ["rtk"]:
        argv = argv[1:]
    return argv == ["pixel", "search-content", "-F", SEARCH_TERM, SEARCH_PATH]


def _is_arena_search_args(args: str) -> bool:
    """Accept the exact command arguments as stored by either action-log version."""
    try:
        argv = shlex.split(args)
    except ValueError:
        return False
    return argv in (
        ["-F", SEARCH_TERM, SEARCH_PATH],
        ["search-content", "-F", SEARCH_TERM, SEARCH_PATH],
    )


def pi_transcript_evidence(
    session_dir: pathlib.Path,
    actions: list[dict],
    since_ms: int,
    until_ms: int | None,
    repo: pathlib.Path,
) -> dict:
    """Judge Pi from its run-owned session transcript, not its terminal viewport."""
    files = sorted(session_dir.glob("*.jsonl")) if session_dir.is_dir() else []
    rows = [row for path in files for row in read_jsonl(path)]
    session_id = next((r.get("id") for r in rows if r.get("type") == "session"), None)
    in_turn = [
        row for row in rows
        if (timestamp := _pi_timestamp_ms(row.get("timestamp"))) is not None
        and timestamp >= since_ms and (until_ms is None or timestamp <= until_ms)
    ]
    brief = next(
        (
            row for row in in_turn
            if row.get("type") == "custom_message"
            and row.get("customType") == "pixel-brief"
            and isinstance(row.get("content"), str)
            and any(marker in row["content"] for marker in BRIEF_TEXT_MARKERS)
        ),
        None,
    )
    calls = {}
    for row in in_turn:
        message = row.get("message")
        if not isinstance(message, dict) or message.get("role") != "assistant":
            continue
        for part in message.get("content", []):
            if not isinstance(part, dict) or part.get("type") != "toolCall" or part.get("name") != "bash":
                continue
            args = part.get("arguments")
            call_id = part.get("id")
            if (
                isinstance(call_id, str)
                and isinstance(args, dict)
                and isinstance(args.get("command"), str)
                and _is_arena_search_call(args["command"])
            ):
                calls[call_id] = row

    matching_actions = []
    for record in turn_records(actions, since_ms, until_ms):
        cwd = record.get("cwd")
        if (
            isinstance(record.get("args"), str)
            and _is_arena_search_args(record["args"])
            and isinstance(cwd, str)
            and cwd
            and pathlib.Path(cwd).resolve() == repo.resolve()
        ):
            matching_actions.append(record)
    action_ids = {short_id(r["invocation_id"]).lower(): r for r in matching_actions}
    result_lines = []
    for row in in_turn:
        message = row.get("message")
        if not isinstance(message, dict) or message.get("role") != "toolResult" or message.get("isError"):
            continue
        call_id = message.get("toolCallId")
        call = calls.get(call_id)
        if call is None or message.get("toolName") != "bash":
            continue
        output = "\n".join(
            part.get("text", "") for part in message.get("content", [])
            if isinstance(part, dict) and part.get("type") == "text" and isinstance(part.get("text"), str)
        )
        boxes = [box for box in parse_boxes(output) if box["op"] == "search-content" and box["id"] in action_ids]
        if boxes:
            result_lines.append(output)
    metrics = judge_metrics("\n".join(result_lines), matching_actions, since_ms, until_ms)
    latest_model = next(
        (row for row in reversed(rows) if row.get("type") == "model_change"),
        {},
    )
    stop_reasons = [
        row.get("message", {}).get("stopReason")
        for row in in_turn
        if isinstance(row.get("message"), dict) and row["message"].get("role") == "assistant"
    ]
    stop_reason = next((reason for reason in reversed(stop_reasons) if reason in {"stop", "length", "error", "aborted"}), None)
    return {
        "session_dir": str(session_dir),
        "transcript_path": str(files[0]) if len(files) == 1 else [str(path) for path in files],
        "session_id": session_id,
        "provider": latest_model.get("provider"),
        "model": latest_model.get("modelId"),
        "turn_complete": stop_reason is not None,
        "stop_reason": stop_reason,
        "brief_evidence": "custom_message: [PIXEL:BRIEF]" if brief else None,
        "shell_calls": len(calls),
        "metrics": metrics,
    }


BRIEF_TEXT_MARKERS = ("[PIXEL:BRIEF]", "[pixel-brief]")
DEVIN_HOOK_DONE = "Hookcompleted:"
DEVIN_BRIEF_VERB = "run-hooktask-event--providerdevin--eventprompt-submit"


def devin_hook_line(scrollback: str) -> str | None:
    """Devin's `✱ Hook completed: '<pixel>' run-hook task-event --provider devin --event
    prompt-submit` line, which `show_output` prints: Devin draws neither the hook's
    `systemMessage` nor its output. Matched with every space removed, since Devin wraps the
    command mid-word (`prompt-` / `submit`)."""
    squeezed = re.sub(r"\s+", "", WRAP.sub(" ", scrollback))
    for chunk in squeezed.split("✱")[1:]:
        if chunk.startswith(DEVIN_HOOK_DONE) and DEVIN_BRIEF_VERB in chunk[:600]:
            raw = [line.strip() for line in scrollback.splitlines() if "Hook completed:" in line]
            return raw[0] if raw else "✱ Hook completed: … --provider devin --event prompt-submit"
    return None


def brief_on_screen(scrollback: str, submit_line: str | None) -> str | None:
    """What on the pane shows the brief reached the host: its text (pi prints it), the
    `🟩 pixel prompt-submit` notice, which the hook adds only to a delivered brief (Claude, Codex),
    or Devin's `✱ Hook completed` line for the brief hook (the logged `part` bytes say it delivered)."""
    for marker in BRIEF_TEXT_MARKERS:
        if marker in scrollback:
            return marker
    return submit_line or devin_hook_line(scrollback)


# agy draws no hook output (1.4.0: of every response shape tried, only a `userMessage` step
# shows, as a fake user turn), so what its hooks injected is read back from its own store:
# one SQLite file per conversation, each injected `ephemeralMessage` a step of type 90. A
# tool's own output is another type (132 for run_command), so its box never counts as relayed.
AGY_PROVIDER = "antigravity"
AGY_STORE = pathlib.PurePath(".gemini/antigravity-cli/conversations")
AGY_EPHEMERAL_STEP = 90


def agy_injected(sessions: list[str], since_ms: int, store: pathlib.Path | None = None) -> dict:
    """The `ephemeralMessage` steps agy stored for the turn: from the conversations the brief log
    names (its `session` is agy's `conversationId`), else from every conversation file written
    since the prompt."""
    store = store or pathlib.Path.home() / AGY_STORE
    dbs = [store / f"{s}.db" for s in sessions if re.fullmatch(r"[\w-]+", s)]
    dbs = [db for db in dbs if db.is_file()]
    if not dbs:
        with contextlib.suppress(OSError):
            dbs = [db for db in store.glob("*.db") if db.stat().st_mtime * 1000 >= since_ms]
    dbs = sorted(set(dbs))
    steps = []
    for db in dbs:
        try:
            with contextlib.closing(sqlite3.connect(f"file:{db}?mode=ro", uri=True)) as conn:
                rows = conn.execute("SELECT step_payload FROM steps WHERE step_type = ? ORDER BY idx", (AGY_EPHEMERAL_STEP,)).fetchall()
        except sqlite3.Error:
            continue
        steps += [bytes(payload).decode("utf-8", "replace") for (payload,) in rows if payload]
    return {"conversations": [db.stem for db in dbs], "steps": steps}


def judge_brief_shown(logged: dict, shown: str | None) -> dict:
    """Brief PASS when the hook logged it for that provider and delivery is evidenced."""
    ok = logged.get("verdict") == "PASS" and shown is not None
    return {
        **logged,
        "verdict": "PASS" if ok else "FAIL",
        "logged": logged.get("verdict") == "PASS",
        "on_screen": shown is not None,
        "screen_evidence": shown,
        "delivery_evidence": shown,
        "evidence_source": "screen" if shown is not None else None,
    }


def judge_brief_delivery(logged: dict, shown: str | None, source: str) -> dict:
    """Keep transcript and store evidence distinct from what appeared on screen."""
    result = judge_brief_shown(logged, shown)
    if shown is not None and source != "screen":
        result.update(on_screen=False, screen_evidence=None, evidence_source=source)
    return result


def judge_relay(relay: dict, shell_calls: int) -> dict:
    """Metrics PASS when the pane shows the 🟩 box of this CLI's Pixel call (`judge_metrics`).
    NO-CALL when the model ran no Pixel command and no box is on screen: nothing to relay, so
    neither a pass nor a relay failure."""
    if relay.get("verdict") != "PASS" and shell_calls < 1 and not relay.get("box_ids_without_record"):
        return {**relay, "verdict": "NO-CALL"}
    return relay


def shell_call_count(scrollback: str) -> int:
    """Count this pane's displayed arena command; time-window action records overlap in parallel runs."""
    command = r"\s+".join(re.escape(token) for token in SHELL_CMD.split())
    starts = list(re.finditer(r"(?m)^[ \t]*(?:[$❯›][ \t]*)?pixel(?=[ \t]|$)", scrollback))
    count = 0
    for index, start in enumerate(starts):
        end = starts[index + 1].start() if index + 1 < len(starts) else len(scrollback)
        command_block = flatten_wrapped(scrollback[start.start():end])
        if re.search(command, command_block):
            count += 1
    return count


# ---------------------------------------------------------------- herdr


class Herdr:
    """The `herdr` CLI. Every call sets HERDR_ENV=1; failures come back as a result, never raise."""

    def __init__(self, binary: str = "herdr"):
        self.binary = binary

    def run(self, *args: str, timeout: float = 40.0) -> dict:
        env = dict(os.environ, HERDR_ENV="1")
        try:
            p = subprocess.run([self.binary, *args], capture_output=True, text=True, timeout=timeout, env=env)
        except (OSError, subprocess.TimeoutExpired) as e:
            return {"rc": 127, "out": "", "err": str(e), "json": None}
        try:
            data = json.loads(p.stdout)
        except ValueError:
            data = None
        return {"rc": p.returncode, "out": p.stdout, "err": p.stderr, "json": data}

    @staticmethod
    def _dig(res: dict, *path: str):
        node = res["json"]
        for key in path:
            if not isinstance(node, dict):
                return None
            node = node.get(key)
        return node

    def split(self, pane: str, direction: str, ratio: float, cwd: str) -> str:
        res = self.run("pane", "split", pane, "--direction", direction, "--ratio", f"{ratio:.4f}", "--cwd", cwd, "--no-focus")
        pid = self._dig(res, "result", "pane", "pane_id")
        if res["rc"] != 0 or not pid:
            raise UltimaError(f"pane split failed: {(res['err'] or res['out']).strip()[:300]}")
        return pid

    def start(self, name: str, kind: str, pane: str, *agent_args: str) -> dict:
        args = ["agent", "start", name, "--kind", kind, "--pane", pane]
        if agent_args:
            args.extend(("--", *agent_args))
        return self.run(*args, timeout=120)

    def prompt(self, name: str, text: str) -> dict:
        return self.run("agent", "prompt", name, text)  # never --wait: it reports a stall for a prompt that ran

    def get(self, name: str) -> dict:
        res = self.run("agent", "get", name)
        agent = self._dig(res, "result", "agent")
        return agent if isinstance(agent, dict) else {}

    def agents(self) -> list[dict]:
        """Agents Herdr currently sees, including their status and working directory."""
        res = self.run("agent", "list")
        agents = self._dig(res, "result", "agents")
        return agents if isinstance(agents, list) else []

    def read(self, name: str, source: str, lines: int) -> str:
        res = self.run("agent", "read", name, "--source", source, "--lines", str(lines))
        data = res["json"]
        if isinstance(data, dict):  # tolerate a JSON envelope; the CLI prints raw text today
            strings = [v for v in _walk_strings(data) if len(v) > 20]
            return max(strings, key=len) if strings else ""
        return res["out"]

    def read_pane(self, pane: str, source: str, lines: int) -> str:
        res = self.run("pane", "read", pane, "--source", source, "--lines", str(lines))
        data = res["json"]
        if isinstance(data, dict):
            strings = [v for v in _walk_strings(data) if len(v) > 20]
            return max(strings, key=len) if strings else ""
        return res["out"]

    def keys(self, name: str, *keys: str) -> None:
        self.run("agent", "send-keys", name, *keys)

    def close(self, pane: str) -> None:
        self.run("pane", "close", pane)

    def panes(self) -> list[dict]:
        res = self.run("pane", "list")
        panes = self._dig(res, "result", "panes")
        return panes if isinstance(panes, list) else []

    def rename(self, pane: str, label: str) -> None:
        self.run("pane", "rename", pane, label)

    def layout(self) -> dict:
        res = self.run("pane", "layout")
        return self._dig(res, "result", "layout") or {}


def _walk_strings(node):
    if isinstance(node, str):
        yield node
    elif isinstance(node, dict):
        for v in node.values():
            yield from _walk_strings(v)
    elif isinstance(node, list):
        for v in node:
            yield from _walk_strings(v)


def grid_plan(n: int) -> list[tuple[str, str, float, str]]:
    """Splits that turn the operator's right half into `n` cells: ceil(n/2) on top, the rest below.

    Each step is (new key, parent key, direction, ratio); `ratio` is the share the
    parent keeps (Herdr's first child), so 1/k leaves k equal columns. The order is
    the one agent-session-debugging prescribes: the right region, the row split,
    then each row's columns left to right. Keys `t0..` top row, `b0..` bottom row,
    `root` the operator pane.
    """
    top = (n + 1) // 2
    bottom = n - top
    # The operator keeps OPERATOR_SHARE of the width: five agents in the rest
    # wrapped every Pixel header at half width (run 2).
    steps = [("t0", "root", "right", OPERATOR_SHARE)]
    if bottom:
        steps.append(("b0", "t0", "down", 0.5))
    for row, count in (("t", top), ("b", bottom)):
        for i in range(1, count):
            steps.append((f"{row}{i}", f"{row}{i - 1}", "right", 1.0 / (count - i + 1)))
    return steps


def cell_keys(n: int) -> list[str]:
    top = (n + 1) // 2
    return [f"t{i}" for i in range(top)] + [f"b{i}" for i in range(n - top)]


# ---------------------------------------------------------------- dialogs

CODEX_PANEL = re.compile(r"hooks? need review", re.IGNORECASE)
CODEX_PANEL_KEY = re.compile(r"t trust all")
CODEX_MODAL = re.compile(r"Hooks need review")
AGY_VERBOSITY_BANNER = re.compile(r"Verbosity.*?esc to dismiss", re.IGNORECASE | re.DOTALL)
TRUST_OPTION = re.compile(r"(\d)\.\s*(Yes,? (?:I )?trust[^\n]*|Trust[^\n]*)", re.IGNORECASE)
TRUST_HINT = re.compile(r"trust this (?:folder|workspace|directory|project)|Yes, I trust", re.IGNORECASE)
PERMISSION_Q = re.compile(r"Do you want to proceed\?|Allow (?:this|once)|Run this command\?|Would you like to run", re.IGNORECASE)
UNKNOWN_DIALOG = re.compile(r"trust|\(y/n\)|\[y/N\]|press enter|do you want|approve|accept|allow", re.IGNORECASE)
PIXEL_HOOK = re.compile(r"""^\s*['"]?(?:[^\s'"]*/)?pixel(?:-dev)?['"]?\s+run-hook\s""")
FAIL_OPEN_TAIL = re.compile(r"\s*\|\|\s*true\s*$")


def hook_commands(node) -> list[str]:
    """Every `command` string in a hooks.json document."""
    found = []
    if isinstance(node, dict):
        for key, value in node.items():
            if key == "command" and isinstance(value, str):
                found.append(value)
            else:
                found.extend(hook_commands(value))
    elif isinstance(node, list):
        for value in node:
            found.extend(hook_commands(value))
    return found


def foreign_codex_hooks(repo: pathlib.Path, home: pathlib.Path | None = None) -> list[str]:
    """Hook commands Codex would ask to review that are not Pixel's own `<path>/pixel run-hook ...`.

    Reads the machine file and the repository's. A file that does not parse counts as foreign.
    """
    codex_home = pathlib.Path(os.environ.get("CODEX_HOME") or (home or pathlib.Path.home()) / ".codex")
    foreign = []
    for path in (codex_home / "hooks.json", repo / ".codex" / "hooks.json"):
        if not path.exists():
            continue
        try:
            doc = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            foreign.append(f"{path}: unreadable")
            continue
        for cmd in hook_commands(doc):
            if not PIXEL_HOOK.match(FAIL_OPEN_TAIL.sub("", cmd)):
                foreign.append(cmd)
    return foreign


# ---------------------------------------------------------------- the run


@dataclasses.dataclass
class Cell:
    cli: Cli
    key: str
    pane: str = ""
    status: str = "pending"  # done | blocked-dialog | needs-operator | timeout | start-failed
    note: str = ""
    t_prompt_ms: int = 0
    sent_at: float = 0.0
    seq0: int = -1
    elapsed_s: float = 0.0
    scrollback: str = ""
    excerpt: str = ""
    keys_sent: list[str] = dataclasses.field(default_factory=list)
    events: list[str] = dataclasses.field(default_factory=list)
    brief: dict = dataclasses.field(default_factory=dict)
    metrics: dict = dataclasses.field(default_factory=dict)
    submit_line: str | None = None
    shell_calls: int = 0
    t_done_ms: int = 0
    screen0: str = ""
    last_screen: str = ""
    stable: int = 0
    store: dict = dataclasses.field(default_factory=dict)  # agy only: `agy_injected`
    pi_session_dir: pathlib.Path | None = None
    pi_transcript: dict = dataclasses.field(default_factory=dict)

    @property
    def name(self) -> str:
        return NAME_PREFIX + self.cli.name

    @property
    def ok(self) -> bool:
        return (
            self.status in ("done", "evidence-complete")
            and self.brief.get("verdict") == "PASS"
            and self.metrics.get("verdict") == "PASS"
        )


@dataclasses.dataclass
class Settings:
    repo: pathlib.Path
    out: pathlib.Path
    seed: int
    clis: list[str]
    timeout: float = 300.0
    start_timeout: float = 90.0
    gap_s: float = 3.0  # pause after one turn ends, before the next prompt
    poll_s: float = 2.0
    skip_install: bool = False
    state_dir: pathlib.Path = dataclasses.field(default_factory=lambda: state_dir())


def state_dir() -> pathlib.Path:
    override = os.environ.get("ULTIMA_ARENA_STATE_DIR")
    return pathlib.Path(override) if override else pathlib.Path(tempfile.gettempdir()) / f"ultima-arena-{os.getuid()}"


def now_ms() -> int:
    return int(time.time() * 1000)


class Arena:
    def __init__(self, cfg: Settings, herdr: Herdr, say=print):
        self.cfg = cfg
        self.h = herdr
        def logged_say(message: str) -> None:
            say(message)
            cfg.out.mkdir(parents=True, exist_ok=True)
            with (cfg.out / "run.log").open("a", encoding="utf-8") as log:
                log.write(f"{datetime.datetime.now(datetime.timezone.utc).isoformat(timespec='seconds')} {message}\n")

        self.say = logged_say
        self.cells: dict[str, Cell] = {}
        self.created: list[dict] = []  # {"pane_id", "name"}: what abort may close
        self.operator = os.environ.get("HERDR_PANE_ID", "")

    # -- state ------------------------------------------------------------

    def save_state(self) -> None:
        self.cfg.state_dir.mkdir(parents=True, exist_ok=True)
        doc = {"operator": self.operator, "out": str(self.cfg.out), "seed": self.cfg.seed, "panes": self.created}
        (self.cfg.state_dir / "last-run.json").write_text(json.dumps(doc, indent=2), encoding="utf-8")

    # -- install ----------------------------------------------------------

    def sh(self, label: str, argv: list[str], cwd: pathlib.Path, fatal: bool, fatal_rc: tuple[int, ...] | None = None) -> dict:
        t0 = time.time()
        try:
            p = subprocess.run(argv, cwd=cwd, capture_output=True, text=True, timeout=1800)
            rc, out, err = p.returncode, p.stdout, p.stderr
        except (OSError, subprocess.TimeoutExpired) as e:
            rc, out, err = 127, "", str(e)
        (self.cfg.out / f"install-{label}.log").write_text(f"$ {' '.join(argv)}\n--- stdout\n{out}\n--- stderr\n{err}\nexit {rc}\n", encoding="utf-8")
        step = {"step": label, "argv": argv, "exit": rc, "seconds": round(time.time() - t0, 1)}
        bad = rc != 0 if fatal_rc is None else rc in fatal_rc
        if fatal and bad:
            raise UltimaError(f"{label} failed (exit {rc}): {(err or out).strip()[:600]}")
        return step

    def install(self) -> list[dict]:
        repo = self.cfg.repo
        return [
            self.sh("self-update", ["pixel", "self-update", "--repo", str(repo), "--build", BUILD_CMD], repo, fatal=True),
            self.sh("setup", ["pixel", "setup", "--yes"], repo, fatal=True),
            self.sh("build-index", ["pixel", "build-index", "--history", "."], repo, fatal=False),
            # doctor: 1 = a check reached the threshold (recorded, not fatal); 2 = could not run.
            self.sh("doctor", ["pixel", "doctor", str(repo), "--fail-on", "yellow", "--json"], repo, fatal=True, fatal_rc=(2, 127)),
        ]

    def identity(self) -> dict:
        def out(argv: list[str]) -> str:
            try:
                p = subprocess.run(argv, cwd=self.cfg.repo, capture_output=True, text=True, timeout=30)
                return (p.stdout or p.stderr).strip()
            except (OSError, subprocess.TimeoutExpired) as e:
                return f"unavailable: {e}"

        return {
            "pixel_path": shutil.which("pixel"),
            "pixel_version": out(["pixel", "--version"]),
            "repo": str(self.cfg.repo),
            "repo_head": out(["git", "rev-parse", "HEAD"]),
            "repo_branch": out(["git", "branch", "--show-current"]),
            "repo_dirty": bool(out(["git", "status", "--porcelain"])),
        }

    # -- grid -------------------------------------------------------------

    def build_grid(self, order: list[str]) -> None:
        if not self.operator:
            raise UltimaError("HERDR_PANE_ID is not set: run this from a Herdr pane (the operator pane stays on the left)")
        panes = {"root": self.operator}
        for key, parent, direction, ratio in grid_plan(len(order)):
            panes[key] = self.h.split(panes[parent], direction, ratio, str(self.cfg.repo))  # sequential: parallel splits race
            self.created.append({"pane_id": panes[key], "name": "pending"})
            self.save_state()
            pane = next((p for p in self.h.panes() if p.get("pane_id") == panes[key]), {})
            self.created[-1]["terminal_id"] = pane.get("terminal_id")
            self.save_state()
        for cli_name, key in zip(order, cell_keys(len(order))):
            cell = Cell(CLIS[cli_name], key, panes[key])
            self.cells[cli_name] = cell
            for rec in self.created:
                if rec["pane_id"] == cell.pane:
                    rec["name"] = cell.name
            self.h.rename(cell.pane, cell.name)
        self.save_state()

    # -- dialogs ----------------------------------------------------------

    def dialog(self, cell: Cell, screen: str) -> str | None:
        """Act on a known startup/approval dialog. Returns a note when one was handled or blocks."""
        tail = "\n".join(screen.splitlines()[-30:])
        if cell.cli.name == "agy" and AGY_VERBOSITY_BANNER.search(tail):
            self.press(cell, "esc")
            cell.events.append("dismissed agy verbosity banner")
            return "handled"
        if cell.cli.name == "codex" and CODEX_PANEL.search(tail):
            foreign = foreign_codex_hooks(self.cfg.repo)
            if foreign:
                cell.status, cell.note = "needs-operator", f"Codex hook review pending and {len(foreign)} hook(s) are not Pixel's: {foreign[0][:120]}"
                return "needs-operator"
            if CODEX_PANEL_KEY.search(tail):
                self.press(cell, "t")
                time.sleep(self.cfg.poll_s / 2)
                self.press(cell, "esc")
            else:
                self.press(cell, "2")
                self.press(cell, "enter")
            cell.events.append("codex hooks trusted (every hook is Pixel's own)")
            return "handled"
        m = TRUST_OPTION.search(tail)
        if cell.cli.name != "codex" and TRUST_HINT.search(tail) and m:
            self.press(cell, m.group(1))
            self.press(cell, "enter")
            cell.events.append(f"folder trust accepted ({m.group(1)})")
            return "handled"
        if PERMISSION_Q.search(tail):
            own = re.search(r"(?m)^[^\w]*(?:Bash\()?\$?\s*" + re.escape(SHELL_CMD) + r"\)?[^\w]*$", tail)
            if own:
                self.press(cell, "enter")
                cell.events.append("approved the arena's own command")
                return "handled"
            cell.status, cell.note, cell.excerpt = "blocked-dialog", "a tool approval for a command that is not the arena's", tail[-600:]
            return "blocked-dialog"
        return None

    def press(self, cell: Cell, key: str) -> None:
        cell.keys_sent.append(key)
        self.h.keys(cell.name, key)

    # -- start ------------------------------------------------------------

    def start_agent(self, cell: Cell) -> bool:
        res = None
        agent_args = []
        if cell.cli.name == "pi":
            self.cfg.out.mkdir(parents=True, exist_ok=True)
            cell.pi_session_dir = pathlib.Path(tempfile.mkdtemp(prefix="pi-session-", dir=self.cfg.out))
            agent_args = ["--tui-mode", "fullscreen", "--session-dir", str(cell.pi_session_dir)]
        elif cell.cli.name == "claude":
            agent_args = ["--verbose"]
        for attempt in range(4):
            res = self.h.start(cell.name, cell.cli.kind, cell.pane, *agent_args)
            if res["rc"] == 0:
                break
            if "agent_pane_busy" not in res["out"] + res["err"]:
                break
            time.sleep(self.cfg.poll_s * 2)  # a fresh pane is not a shell prompt yet
        if res is None or res["rc"] != 0:
            cell.status = "start-failed"
            cell.note = ((res or {}).get("err") or (res or {}).get("out") or "agent start failed").strip()[:300]
            return False
        deadline = time.time() + self.cfg.start_timeout
        stable = 0
        screen = ""
        while time.time() < deadline:
            screen = self.h.read(cell.name, "visible", 60)
            act = self.dialog(cell, screen)
            if act == "needs-operator" or act == "blocked-dialog":
                return False
            if act == "handled":
                stable = 0
                time.sleep(self.cfg.poll_s)
                continue
            agent = self.h.get(cell.name)
            if not agent:
                self.fail_lost_agent(cell, "after start")
                return False
            if agent.get("agent_status") == "idle" and agent.get("interactive_ready", True):
                stable += 1
                if stable >= 2:
                    cell.seq0 = int(agent.get("state_change_seq", -1))
                    return True
            else:
                stable = 0
            time.sleep(self.cfg.poll_s)
        if UNKNOWN_DIALOG.search(screen):
            cell.status, cell.note, cell.excerpt = "blocked-dialog", "not ready; the screen shows an unknown dialog", screen[-600:]
        else:
            cell.status, cell.note, cell.excerpt = "start-failed", "the agent never reached an idle prompt", screen[-600:]
        return False

    # -- prompt and wait --------------------------------------------------

    def send_prompt(self, cell: Cell) -> bool:
        agent = self.h.get(cell.name)
        if not agent:
            self.fail_lost_agent(cell, "before prompt dispatch")
            return False
        cell.seq0 = int(agent.get("state_change_seq", cell.seq0))
        cell.screen0 = cell.last_screen = self.h.read(cell.name, "visible", 60)
        cell.t_prompt_ms = now_ms()
        cell.sent_at = time.time()
        res = self.h.prompt(cell.name, guard_prompt(PROMPT))
        if res["rc"] != 0:
            cell.status, cell.note = "start-failed", f"agent prompt failed: {(res['err'] or res['out']).strip()[:300]}"
            return False
        cell.status = "waiting"
        return True

    def record_since(self, cell: Cell) -> bool:
        return bool(turn_records(read_jsonl(self.cfg.repo / ".pixel" / ACTIONS_FILE), cell.t_prompt_ms))

    def fail_lost_agent(self, cell: Cell, phase: str) -> None:
        cell.status = "start-failed"
        cell.note = f"Herdr no longer reports the agent {phase}"
        cell.scrollback = self.h.read_pane(cell.pane, "recent-unwrapped", SCROLLBACK_LINES)
        if not cell.scrollback:
            cell.scrollback = self.h.read(cell.name, "recent-unwrapped", SCROLLBACK_LINES)
        cell.excerpt = "\n".join(cell.scrollback.splitlines()[-25:])
        self.say(f"{cell.name}: agent disappeared {phase}; saved the pane output")

    def evidence_ready(self, cell: Cell, screen: str) -> bool:
        """Whether this turn has submitted its brief and displayed its own metrics relay."""
        logged = judge_brief(read_jsonl(self.cfg.repo / ".pixel" / FEEDBACK_FILE), cell.cli.provider, cell.t_prompt_ms)
        submit_line = prompt_submit_line(screen)
        shown = brief_on_screen(screen, submit_line)
        evidence = screen
        evidence_source = "screen"
        metrics = None
        if cell.cli.name == "pi" and cell.pi_session_dir:
            cell.pi_transcript = pi_transcript_evidence(
                cell.pi_session_dir,
                read_jsonl(self.cfg.repo / ".pixel" / ACTIONS_FILE),
                cell.t_prompt_ms,
                None,
                self.cfg.repo,
            )
            shown = cell.pi_transcript["brief_evidence"]
            evidence_source = "pi session transcript"
            metrics = cell.pi_transcript["metrics"]
        if cell.cli.provider == AGY_PROVIDER:
            cell.store = agy_injected(logged["sessions"], cell.t_prompt_ms)
            injected = "\n".join(cell.store["steps"])
            store_shown = next((f"agy store: {m}" for m in BRIEF_TEXT_MARKERS if m in injected), None)
            if shown is None and store_shown:
                shown = store_shown
                evidence_source = "agy conversation store"
            evidence += "\n" + injected
        brief = judge_brief_delivery(logged, shown, evidence_source)
        actions = read_jsonl(self.cfg.repo / ".pixel" / ACTIONS_FILE)
        if metrics is None:
            metrics = judge_metrics(evidence, actions, cell.t_prompt_ms)
            shell_calls = shell_call_count(evidence)
        else:
            shell_calls = cell.pi_transcript["shell_calls"]
        metrics = judge_relay(metrics, shell_calls)
        cell.brief = brief
        cell.metrics = metrics
        cell.shell_calls = shell_calls
        cell.submit_line = submit_line
        return brief["verdict"] == "PASS" and metrics["verdict"] == "PASS"

    def settle(self, cells: list[Cell]) -> None:
        """Poll until each prompted cell's evidence is complete or its turn ends.

        A turn ended when Herdr reports `idle` or `done` after the cell moved (its
        state counter changed, or its screen did: Herdr sees no state change on pi)
        and the screen then held still for `STABLE_POLLS` polls. When both evidence
        signals arrive first, interrupt the model turn to avoid spending tokens on
        an answer the arena does not score, then wait for the pane to become idle."""
        pending = [c for c in cells if c.status == "waiting"]
        while pending:
            for cell in list(pending):
                waited = time.time() - cell.sent_at
                screen = self.h.read(cell.name, "visible", 60)
                act = self.dialog(cell, screen)
                if act in ("needs-operator", "blocked-dialog"):
                    pending.remove(cell)
                    continue
                cell.stable = cell.stable + 1 if screen == cell.last_screen else 0
                cell.last_screen = screen
                agent = self.h.get(cell.name)
                if not agent:
                    self.fail_lost_agent(cell, "after prompt dispatch")
                    cell.elapsed_s = round(time.time() - cell.sent_at, 1)
                    cell.t_done_ms = now_ms()
                    pending.remove(cell)
                    continue
                moved = int(agent.get("state_change_seq", cell.seq0)) != cell.seq0 or screen != cell.screen0
                finished = agent.get("agent_status") in FINISHED and moved and cell.stable >= STABLE_POLLS
                if cell.cli.name == "pi" and cell.pi_session_dir:
                    cell.pi_transcript = pi_transcript_evidence(
                        cell.pi_session_dir,
                        read_jsonl(self.cfg.repo / ".pixel" / ACTIONS_FILE),
                        cell.t_prompt_ms,
                        None,
                        self.cfg.repo,
                    )
                    finished = finished and cell.pi_transcript["turn_complete"]
                if cell.status == "waiting" and not finished and self.evidence_ready(
                    cell, self.h.read(cell.name, "recent-unwrapped", SCROLLBACK_LINES)
                ):
                    self.say(f"{cell.name}: brief and matching metrics are visible; interrupting the turn")
                    self.h.keys(cell.name, "ctrl+c")
                    cell.keys_sent.append("ctrl+c")
                    cell.events.append("interrupted after brief and metrics evidence; answer is not scored")
                    cell.status = "stopping"
                if cell.status == "stopping" and finished:
                    cell.status = "evidence-complete"
                    cell.note = "stopped after both evidence signals"
                if finished and waited >= 3 * self.cfg.poll_s and self.record_since(cell):
                    if cell.status != "evidence-complete":
                        cell.status = "done"
                elif finished and waited >= 10 * self.cfg.poll_s:
                    if cell.status != "evidence-complete":
                        cell.status = "done"  # ended with no record: judged below on its evidence
                elif waited >= self.cfg.timeout:
                    if "ctrl+c" not in cell.keys_sent:
                        self.h.keys(cell.name, "ctrl+c")
                        cell.keys_sent.append("ctrl+c")
                        cell.events.append("interrupted after the turn deadline")
                    cell.status, cell.note = "timeout", f"turn not ended after {self.cfg.timeout:g}s"
                if cell.status not in ("waiting", "stopping"):
                    pending.remove(cell)
                    cell.elapsed_s = round(time.time() - cell.sent_at, 1)
                    cell.t_done_ms = now_ms()
            if pending:
                time.sleep(self.cfg.poll_s)

    # -- judge ------------------------------------------------------------

    def judge(self, cell: Cell) -> None:
        if not cell.t_prompt_ms:
            cell.brief = {"verdict": "N/A"}
            cell.metrics = {"verdict": "N/A"}
            return
        cell.scrollback = self.h.read_pane(cell.pane, "recent-unwrapped", SCROLLBACK_LINES)
        actions = read_jsonl(self.cfg.repo / ".pixel" / ACTIONS_FILE)
        until = cell.t_done_ms or None
        preliminary_metrics = judge_metrics(cell.scrollback, actions, cell.t_prompt_ms, until)
        if (
            cell.cli.name == "codex"
            and cell.status in ("done", "evidence-complete")
            and preliminary_metrics["verdict"] != "PASS"
        ):
            self.h.keys(cell.name, "ctrl+t")
            cell.keys_sent.append("ctrl+t")
            cell.events.append("expanded Codex tool output once after the turn ended to inspect the metrics evidence")
            cell.scrollback = self.h.read_pane(cell.pane, "recent-unwrapped", SCROLLBACK_LINES)
        cell.submit_line = prompt_submit_line(cell.scrollback)
        logged = judge_brief(read_jsonl(self.cfg.repo / ".pixel" / FEEDBACK_FILE), cell.cli.provider, cell.t_prompt_ms)
        shown = brief_on_screen(cell.scrollback, cell.submit_line)
        evidence = cell.scrollback
        evidence_source = "screen"
        pi_metrics = None
        if cell.cli.name == "pi" and cell.pi_session_dir:
            cell.pi_transcript = pi_transcript_evidence(
                cell.pi_session_dir,
                read_jsonl(self.cfg.repo / ".pixel" / ACTIONS_FILE),
                cell.t_prompt_ms,
                cell.t_done_ms or None,
                self.cfg.repo,
            )
            shown = cell.pi_transcript["brief_evidence"]
            evidence_source = "pi session transcript"
            pi_metrics = cell.pi_transcript["metrics"]
        if cell.cli.provider == AGY_PROVIDER:  # agy may draw nothing: its store records injected context
            cell.store = agy_injected(logged["sessions"], cell.t_prompt_ms)
            injected = "\n".join(cell.store["steps"])
            store_shown = next((f"agy store: {m}" for m in BRIEF_TEXT_MARKERS if m in injected), None)
            if shown is None and store_shown:
                shown = store_shown
                evidence_source = "agy conversation store"
            evidence += "\n" + injected
        cell.brief = judge_brief_delivery(logged, shown, evidence_source)
        relay = pi_metrics if pi_metrics is not None else judge_metrics(evidence, actions, cell.t_prompt_ms, until)
        cell.shell_calls = max(
            cell.pi_transcript.get("shell_calls", 0) if cell.cli.name == "pi" else 0,
            shell_call_count(evidence),
            relay["occurrences"],
        )
        cell.metrics = judge_relay(relay, cell.shell_calls)
        if cell.metrics["verdict"] == "NO-CALL":
            cell.events.append("no Pixel shell call on this pane: the model answered without running the command, so nothing could be relayed")
        if not cell.elapsed_s:
            cell.elapsed_s = round(time.time() - cell.sent_at, 1)
        if cell.status in ("waiting", "pending"):
            cell.status = "done"
        if cell.status in ("timeout", "blocked-dialog") and not cell.excerpt:
            cell.excerpt = "\n".join(cell.scrollback.splitlines()[-25:])

    # -- orchestration ----------------------------------------------------

    def run(self, order: list[str]) -> int:
        cfg = self.cfg
        cfg.out.mkdir(parents=True, exist_ok=True)
        overlap = active_agents_in_repo(self.h.agents(), cfg.repo, self.operator)
        if overlap:
            details = "; ".join(f"{a['agent']} ({a['agent_status']}) at {a['path']}" for a in overlap)
            self.say(f"WARNING: other active agent session(s) may write to {cfg.repo} during this run: {details}")
        install = [] if cfg.skip_install else self.install()
        ident = self.identity()
        self.say(f"pixel: {ident['pixel_version']} at {ident['pixel_path']}; repo head {ident['repo_head'][:12]}")
        self.say(f"seed {cfg.seed}; order {', '.join(order)}")
        self.build_grid(order)
        started = []
        for name in order:  # start in the random order, one at a time
            cell = self.cells[name]
            self.say(f"{cell.name}: starting in {cell.pane}")
            if self.start_agent(cell):
                started.append(cell)
        # Dispatch every turn before monitoring any of them. The evidence judge
        # matches each pane's displayed invocation id to the shared action log.
        for cell in started:
            if self.send_prompt(cell):
                self.say(f"{cell.name}: prompted")
        self.settle([cell for cell in started if cell.status == "waiting"])
        for cell in started:
            self.say(f"{cell.name}: {cell.status} after {cell.elapsed_s:g}s")
        for name in order:
            self.judge(self.cells[name])
        write_receipt(cfg, order, ident, install, self.cells)
        return report(self.cells, order, cfg.out, self.say)

    def abort(self) -> int:
        return abort_run(self.h, self.cfg.state_dir, self.say)


# ---------------------------------------------------------------- receipt


def report(cells: dict[str, Cell], order: list[str], out: pathlib.Path, say=print) -> int:
    n = len(order)
    for name in order:
        c = cells[name]
        say(f"{c.name:<14} {c.status:<15} brief={c.brief.get('verdict', 'N/A'):<4} metrics={c.metrics.get('verdict', 'N/A'):<4} {c.note}".rstrip())
    briefs = sum(1 for c in cells.values() if c.brief.get("verdict") == "PASS")
    metrics = sum(1 for c in cells.values() if c.metrics.get("verdict") == "PASS")
    say(f"{briefs}/{n} brief, {metrics}/{n} metrics")
    say(f"receipt: {out}")
    return 0 if all(c.ok for c in cells.values()) else 1


def git_common_dir(path: pathlib.Path) -> pathlib.Path | None:
    """Resolve a worktree's shared Git directory, returning None outside a repository."""
    try:
        result = subprocess.run(
            ["git", "-C", str(path), "rev-parse", "--git-common-dir"],
            capture_output=True,
            text=True,
            timeout=2,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    if result.returncode != 0 or not result.stdout.strip():
        return None
    common = pathlib.Path(result.stdout.strip())
    return (common if common.is_absolute() else path / common).resolve()


def active_agents_in_repo(agents: list[dict], repo: pathlib.Path, operator_pane: str = "") -> list[dict]:
    """Active Herdr agents in the target repo or another worktree sharing its Git directory."""
    target = repo.resolve()
    target_common = git_common_dir(target)
    common_cache: dict[pathlib.Path, pathlib.Path | None] = {target: target_common}

    def common_dir(path: pathlib.Path) -> pathlib.Path | None:
        if path not in common_cache:
            common_cache[path] = git_common_dir(path)
        return common_cache[path]

    found = []
    for agent in agents:
        if agent.get("pane_id") == operator_pane or agent.get("agent_status") != "working":
            continue
        paths = []
        for key in ("foreground_cwd", "cwd"):
            value = agent.get(key)
            if not isinstance(value, str) or not value:
                continue
            try:
                path = pathlib.Path(value).resolve()
            except OSError:
                continue
            same_path = path == target or target in path.parents
            same_worktree_family = not same_path and target_common is not None and common_dir(path) == target_common
            if same_path or same_worktree_family:
                paths.append(path)
        if paths:
            found.append({**agent, "path": str(paths[0])})
    return found


def write_receipt(cfg: Settings, order: list[str], ident: dict, install: list[dict], cells: dict[str, Cell]) -> None:
    per_cli = {}
    for name in order:
        c = cells[name]
        (cfg.out / f"{name}.scrollback.txt").write_text(c.scrollback, encoding="utf-8")
        if c.store:
            (cfg.out / f"{name}.injected.txt").write_text("\n\f\n".join(c.store["steps"]), encoding="utf-8")
        if c.pi_transcript:
            (cfg.out / "pi.evidence.json").write_text(json.dumps(c.pi_transcript, indent=2, ensure_ascii=False), encoding="utf-8")
        per_cli[name] = {
            "provider": c.cli.provider,
            "pane": c.pane,
            "status": c.status,
            "note": c.note,
            "elapsed_s": c.elapsed_s,
            "prompted_at_ms": c.t_prompt_ms,
            "brief": c.brief,
            "metrics": c.metrics,
            "prompt_submit_line": {"seen": c.submit_line is not None, "text": c.submit_line},
            "shell_call_seen": c.shell_calls >= 1,
            "shell_call_count": c.shell_calls,
            "keys_sent": c.keys_sent,
            "events": c.events,
            "screen_excerpt": c.excerpt,
            **({"pi_transcript": c.pi_transcript} if c.pi_transcript else {}),
            **({"agy_store": {"conversations": c.store["conversations"], "ephemeral_steps": len(c.store["steps"])}} if c.store else {}),
            "ok": c.ok,
        }
    doc = {
        "when_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
        "identity": ident,
        "install": install if install else "skipped",
        "seed": cfg.seed,
        "order": order,
        "prompt": PROMPT,
        "clis": per_cli,
        "summary": {
            "brief": sum(1 for c in cells.values() if c.brief.get("verdict") == "PASS"),
            "metrics": sum(1 for c in cells.values() if c.metrics.get("verdict") == "PASS"),
            "of": len(order),
        },
    }
    results_root = cfg.repo / "eval" / "arena-results"
    history = []
    for path in sorted(results_root.glob("ultima-*/receipt.json")):
        if path.parent == cfg.out:
            continue
        try:
            previous = json.loads(path.read_text(encoding="utf-8"))
            previous.setdefault("receipt_dir", path.parent.name)
            history.append(previous)
        except (OSError, ValueError):
            continue
    history.sort(key=lambda run: str(run.get("when_utc", "")))
    doc["run_number"] = len(history) + 1
    doc["receipt_dir"] = cfg.out.name
    (cfg.out / "receipt.json").write_text(json.dumps(doc, indent=2, ensure_ascii=False), encoding="utf-8")
    rows = [
        "| order | CLI | status | brief | logged | delivery evidence | metrics | Pixel shell call | elapsed |",
        "| --- | --- | --- | --- | --- | --- | --- | --- | --- |",
    ]
    for i, name in enumerate(order, 1):
        c = cells[name]
        shown = c.brief.get("delivery_evidence")
        rows.append(
            f"| {i} | {name} | {c.status} | {verdict_label(c.brief.get('verdict', 'N/A'))} | {'yes' if c.brief.get('logged') else 'no'} "
            f"| {('`' + shown + '` (' + str(c.brief.get('evidence_source')) + ')') if shown else 'no'} | {verdict_label(c.metrics.get('verdict', 'N/A'))} "
            f"| {'yes' if c.shell_calls >= 1 else 'no'} | {c.elapsed_s}s |"
        )
    s = doc["summary"]
    md = [
        f"# Ultima Arena — Run #{doc['run_number']} — {doc['when_utc']}",
        "",
        f"Pixel at `{ident['pixel_path']}`:\n\n```text\n{ident['pixel_version']}\n```",
        f"Repository `{ident['repo']}` at `{ident['repo_head']}`; dirty: {ident.get('repo_dirty', 'unknown')}; seed {cfg.seed}.",
        "",
        *rows,
        "",
        f"**{s['brief']}/{s['of']} brief, {s['metrics']}/{s['of']} metrics**",
        "",
        "Verdicts come from `.pixel/brief-feedback.jsonl`, `.pixel/actions.jsonl`, each CLI's scrollback and, for agy, which draws no hook output, the `ephemeralMessage` steps of its conversation store; the model's answer is not scored.",
    ]
    (cfg.out / "receipt.md").write_text("\n".join(md) + "\n", encoding="utf-8")
    write_run_index(results_root, [*history, doc])


def verdict_label(verdict: str) -> str:
    return f"{'🟩' if verdict == 'PASS' else '⬜' if verdict == 'NO-CALL' else '🟥'} {verdict}"


def write_run_index(results_root: pathlib.Path, runs: list[dict]) -> None:
    """Refresh the gitignored Markdown history from the run receipts."""
    results_root.mkdir(parents=True, exist_ok=True)
    rows = [
        "| Run | Date (UTC) | Seed / order | Brief | Metrics | Commit | Receipt |",
        "| ---: | --- | --- | ---: | ---: | --- | --- |",
    ]
    for number, run in enumerate(runs, start=1):
        identity = run.get("identity", {})
        summary = run.get("summary", {})
        commit = str(identity.get("repo_head", "unknown"))[:12]
        if identity.get("repo_dirty", "-dirty" in str(identity.get("pixel_version", ""))):
            commit += " (dirty)"
        order = ", ".join(run.get("order", []))
        dirname = str(run.get("receipt_dir", ""))
        link = f"[{dirname}](./{dirname}/receipt.md)" if dirname else "receipt"
        rows.append(
            f"| {number} | {run.get('when_utc', 'unknown')} | seed {run.get('seed', '?')}: {order} "
            f"| {summary.get('brief', '?')}/{summary.get('of', '?')} "
            f"| {summary.get('metrics', '?')}/{summary.get('of', '?')} | `{commit}` | {link} |"
        )
    contents = [
        "# Ultima Arena run history",
        "",
        "Each row links to the per-run receipt. Runs are ordered by receipt date; seed fixes CLI start order.",
        "",
        *rows,
        "",
    ]
    (results_root / "README.md").write_text("\n".join(contents), encoding="utf-8")


# ---------------------------------------------------------------- abort


def abort_run(herdr: Herdr, sdir: pathlib.Path, say=print) -> int:
    """Close panes still owned by the last run, including an identified crashed Pi shell."""
    try:
        doc = json.loads((sdir / "last-run.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):
        say("abort: no recorded run, nothing to close")
        return 0
    live = {p.get("pane_id"): p for p in herdr.panes()}
    operator = doc.get("operator")
    closed, kept = [], []
    for rec in doc.get("panes", []):
        pid, name = rec.get("pane_id"), str(rec.get("name", ""))
        if not pid or pid == operator or not name.startswith(NAME_PREFIX):
            kept.append(pid)
            continue
        pane = live.get(pid)
        if pane is None:
            continue  # already gone
        label = str(pane.get("label") or pane.get("terminal_title_stripped") or "")
        agent_pane = herdr.get(name).get("pane_id")
        expected_agent = name.removeprefix(NAME_PREFIX)
        current_agent = pane.get("agent")
        assigned_to_other = current_agent and current_agent != expected_agent
        if not assigned_to_other and (label.startswith(NAME_PREFIX) or agent_pane == pid):
            herdr.close(pid)
            closed.append(pid)
        elif (
            not current_agent
            and rec.get("terminal_id")
            and pane.get("terminal_id") == rec["terminal_id"]
            and re.search(r"pi exiting due to uncaughtException: Rendered line\d+ exceeds terminalwidth \(\d+>\d+\)", herdr.read_pane(pid, "recent-unwrapped", 80))
        ):
            herdr.close(pid)
            closed.append(pid)
        else:
            kept.append(pid)
            say(f"abort: kept recorded pane {pid}; its current label, agent or terminal no longer proves Ultima ownership")
    (sdir / "last-run.json").unlink(missing_ok=True)
    say(f"abort: closed {len(closed)} pane(s) {closed}; left {len([k for k in kept if k])} untouched")
    return 0


# ---------------------------------------------------------------- cli


def git_toplevel(cwd: pathlib.Path) -> pathlib.Path:
    p = subprocess.run(["git", "rev-parse", "--show-toplevel"], cwd=cwd, capture_output=True, text=True)
    if p.returncode != 0:
        raise UltimaError(f"{cwd} is not inside a git repository (pass --repo)")
    return pathlib.Path(p.stdout.strip())


def parse(argv: list[str]) -> argparse.Namespace:
    ap = argparse.ArgumentParser(prog="ultima_arena.py", description=__doc__.split("\n\n")[0])
    ap.add_argument("command", nargs="?", choices=["run", "abort"], default="run")
    ap.add_argument("--repo", help="repository the agents work in (default: git toplevel of the cwd)")
    ap.add_argument("--skip-install", action="store_true", help="use the pixel already on PATH")
    ap.add_argument("--seed", type=int, default=DEFAULT_SEED, help=f"shuffle seed (default: {DEFAULT_SEED})")
    ap.add_argument("--clis", default=",".join(CLIS), help="comma-separated subset of: " + ",".join(CLIS))
    ap.add_argument("--timeout", type=float, default=300.0, help="seconds per CLI after its prompt (default 300)")
    ap.add_argument("--out", help="receipt directory (default: <repo>/eval/arena-results/ultima-<UTC>)")
    ap.add_argument("--start-timeout", type=float, default=90.0, help=argparse.SUPPRESS)
    ap.add_argument("--gap", type=float, default=3.0, help=argparse.SUPPRESS)
    ap.add_argument("--poll-s", type=float, default=2.0, help=argparse.SUPPRESS)
    ap.add_argument("--self-test", action="store_true", help="run the offline contract tests and exit")
    return ap.parse_args(argv)


def self_test() -> int:
    suite = unittest.defaultTestLoader.discover(str(HERE), pattern="test_ultima_arena.py")
    result = unittest.TextTestRunner(verbosity=1).run(suite)
    return 0 if result.wasSuccessful() else 1


def main(argv: list[str] | None = None) -> int:
    args = parse(sys.argv[1:] if argv is None else argv)
    if args.self_test:
        return self_test()
    herdr = Herdr()
    try:
        if shutil.which("herdr") is None:
            raise UltimaError("herdr is not on PATH")
        if args.command == "abort":
            with arena_lock(state_dir()):
                return abort_run(herdr, state_dir())
        names = [n.strip() for n in args.clis.split(",") if n.strip()]
        unknown = [n for n in names if n not in CLIS]
        if unknown or not names:
            raise UltimaError(f"--clis takes {','.join(CLIS)}; got {args.clis!r}")
        guard_prompt(PROMPT)
        if os.environ.get("PIXEL_BRIEF_LOG", "").lower() in BRIEF_LOG_OFF:
            raise UltimaError("PIXEL_BRIEF_LOG switches .pixel/brief-feedback.jsonl off: the brief verdict has no evidence; unset it")
        repo = pathlib.Path(args.repo).resolve() if args.repo else git_toplevel(pathlib.Path.cwd())
        if not (repo / SEARCH_PATH).is_dir():
            raise UltimaError(f"{repo / SEARCH_PATH} does not exist: the prompt's search needs a pixel checkout")
        seed = args.seed
        stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")
        cfg = Settings(
            repo=repo,
            out=pathlib.Path(args.out).resolve() if args.out else repo / "eval" / "arena-results" / f"ultima-{stamp}",
            seed=seed,
            clis=names,
            timeout=args.timeout,
            start_timeout=args.start_timeout,
            gap_s=args.gap,
            poll_s=args.poll_s,
            skip_install=args.skip_install,
        )
        arena = Arena(cfg, herdr)
        with arena_lock(cfg.state_dir):
            if not arena.operator or not any(p.get("pane_id") == arena.operator for p in herdr.panes()):
                raise UltimaError("HERDR_PANE_ID must identify an existing operator pane; no panes or installation were changed")
            abort_run(herdr, cfg.state_dir, lambda m: None)  # panes of an earlier run of this tool
            return arena.run(shuffled(names, seed))
    except UltimaError as e:
        print(f"ultima-arena: {e}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
