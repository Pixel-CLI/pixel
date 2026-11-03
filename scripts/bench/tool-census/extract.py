#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Tool-call extractor for the tool census (issue #905).

Turns agent transcripts into ``(tool call, output)`` rows with a class and an
information need (``classify.py``) in the census SQLite store (``sessions``, ``calls``: the ``SCHEMA`` below).

    python3 -I scripts/bench/tool-census/extract.py ingest --census     # raw/*.stream.jsonl of a census run
    python3 -I scripts/bench/tool-census/extract.py ingest --history    # sessions already on disk, last 120 days
    python3 -I scripts/bench/tool-census/extract.py watch               # re-ingest the raw dir every 3 s
    python3 -I scripts/bench/tool-census/extract.py reclassify          # recompute class/need of stored calls after a rule change
    python3 -I scripts/bench/tool-census/extract.py summary             # aggregate tables, no prompt or output text

Parsers (tool_use paired with tool_result by call id, output capped at 16 KB
with its true byte size kept, ``is_error`` kept):

* Claude Code stream-json (``claude -p --output-format stream-json --verbose``)
  and Claude Code session JSONL (``~/.claude/projects/*/*.jsonl``; subagent and
  sidechain files are skipped): both carry the same ``assistant``/``user``
  records.
* Codex rollouts (``~/.codex/sessions/**/rollout-*.jsonl``): ``function_call``,
  ``custom_tool_call`` (the ``exec`` tool is expanded into the ``tools.*`` calls
  of its script), ``local_shell_call``, ``web_search_call`` and their outputs.
* pi sessions (``~/.pi/agent/sessions/*/*.jsonl``): ``toolCall`` / ``toolResult``.

History is private. Prompts are never stored (``sessions.prompt`` is NULL, only
a hash is kept), outputs live only in the local database, and ``summary``
prints aggregates only. A history session is kept when it has at least one
organic user prompt (not a harness envelope, a subagent brief, a slash command
or a scripted eval) and started in the last ``--days`` days.

Standard library only (sqlite3, json, multiprocessing).
"""

import argparse
import collections
import glob
import hashlib
import json
import multiprocessing
import os
import re
import sqlite3
import statistics
import sys
import time
from datetime import datetime, timedelta, timezone

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import classify  # noqa: E402

import question_source  # noqa: E402

CENSUS_DIR = os.environ.get("CENSUS_DIR") or str(question_source.default_root())
DB_PATH = os.environ.get("CENSUS_DB", os.path.join(CENSUS_DIR, "census.db"))
RAW_DIR = os.environ.get("CENSUS_RAW", os.path.join(CENSUS_DIR, "raw"))
OUTPUT_CAP = 16384
INPUT_CAP = 262144
HOME = os.path.expanduser("~")

# --------------------------------------------------------------------------
# Database
# --------------------------------------------------------------------------

SCHEMA = """
CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY, source TEXT, repo TEXT, sha TEXT, category TEXT, mode TEXT,
  prompt TEXT, prompt_hash TEXT, started_at TEXT, wall_s REAL, model TEXT, tokens_in INTEGER, tokens_out INTEGER,
  cost_usd REAL, final_answer TEXT, ok INTEGER, raw_path TEXT);
CREATE TABLE IF NOT EXISTS calls(session TEXT, seq INTEGER, tool TEXT, input_json TEXT, output TEXT,
  output_bytes INTEGER, is_error INTEGER, class TEXT, need TEXT, ts TEXT, PRIMARY KEY(session, seq));
CREATE TABLE IF NOT EXISTS ingest_state(path TEXT PRIMARY KEY, kind TEXT, size INTEGER, mtime_ns INTEGER, session TEXT);
CREATE TABLE IF NOT EXISTS history_prompts(session TEXT, idx INTEGER, key TEXT, chars INTEGER, organic INTEGER,
  PRIMARY KEY(session, idx));
"""


def connect(path=DB_PATH):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    conn = sqlite3.connect(path, timeout=60)
    conn.execute("PRAGMA busy_timeout=60000")
    conn.executescript(SCHEMA)
    have = {row[1] for row in conn.execute("PRAGMA table_info(calls)")}
    for column, decl in (("turn", "INTEGER"), ("call_id", "TEXT")):
        if column not in have:  # additive, per the contract: lane B may add columns, never rename
            conn.execute(f"ALTER TABLE calls ADD COLUMN {column} {decl}")
    conn.execute("CREATE INDEX IF NOT EXISTS calls_call_id ON calls(call_id)")
    conn.execute("CREATE INDEX IF NOT EXISTS calls_class ON calls(class)")
    conn.commit()
    return conn


# --------------------------------------------------------------------------
# Small helpers
# --------------------------------------------------------------------------

def cap_text(text):
    """(text capped at OUTPUT_CAP bytes, true byte size)."""
    if text is None:
        return None, None
    data = text.encode("utf-8", "replace")
    if len(data) <= OUTPUT_CAP:
        return text, len(data)
    return data[:OUTPUT_CAP].decode("utf-8", "ignore"), len(data)


def dump_input(value):
    text = json.dumps(value, ensure_ascii=False, default=str)
    return text if len(text) <= INPUT_CAP else text[:INPUT_CAP]


def flatten(content):
    """Text of a tool result's content (string, text blocks, tool references)."""
    if content is None:
        return ""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = []
        for block in content:
            if isinstance(block, str):
                parts.append(block)
            elif isinstance(block, dict):
                kind = block.get("type")
                if kind in ("text", "input_text", "output_text"):
                    parts.append(block.get("text") or "")
                elif kind == "tool_reference":
                    parts.append(block.get("tool_name") or "")
                elif kind in ("image", "input_image"):
                    parts.append("[image]")
                else:
                    parts.append(json.dumps(block, ensure_ascii=False)[:2000])
        return "\n".join(parts)
    return json.dumps(content, ensure_ascii=False, default=str)


def parse_ts(value):
    if isinstance(value, datetime):
        return value if value.tzinfo else value.replace(tzinfo=timezone.utc)
    if isinstance(value, (int, float)):
        seconds = value / 1000 if value > 1e11 else value
        return datetime.fromtimestamp(seconds, timezone.utc)
    if isinstance(value, str) and value:
        try:
            return datetime.fromisoformat(value.replace("Z", "+00:00"))
        except ValueError:
            return None
    return None


def iso(value):
    moment = parse_ts(value)
    return moment.strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z" if moment else None


def hash_repo(cwd):
    """Hash of the project a session ran in; worktrees fold into their repository."""
    cwd = (cwd or "").rstrip("/")
    cwd = re.sub(r"/\.(claude|codex)/worktrees/.*$", "", cwd)
    parts = cwd.split("/")
    depth = 5 if len(parts) > 3 and parts[1] == "Users" and parts[3] in ("Documents", "Desktop", "Developer", "dev", "code", "Projects", "src") else 4
    return hashlib.sha256("/".join(parts[:depth]).encode()).hexdigest()[:10] if cwd else None


def read_jsonl(path):
    with open(path, encoding="utf-8", errors="replace") as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                yield json.loads(line)
            except ValueError:
                continue  # a partial last line while the file is still written


# --------------------------------------------------------------------------
# Organic prompts (ideas from the prompt-taxonomy scripts: drop harness envelopes,
# subagent briefs, slash commands, very short replies)
# --------------------------------------------------------------------------

BAD_PROMPT = re.compile(
    r"^(MODE: SUBAGENT|<command-|<task-notification|<local-command|Caveat:|\[Request interrupted|"
    r"This session is being continued|<system-reminder|Base directory for this skill|Stop hook|You are |"
    r"<environment_context|<user_instructions|# AGENTS\.md instructions|<turn_aborted|<subagent_notification|"
    r"<skill>|<codex_delegation|<hook_prompt|<permissions instructions|<collaboration_mode|\[SUBAGENT|"
    r"<local-command-caveat|<ide_|<tool_use_error|Continue from where you left off|<session_context)", re.I)
WRAPPER_TAGS = re.compile(
    r"<(system-reminder|command-[a-z-]+|task-notification|local-command-[a-z]+|user-prompt-submit-hook|"
    r"environment_context|ide_selection|ide_opened_file|ide_diagnostics)[^>]*>.*?</\1>", re.S)
PASTED = re.compile(r"<pasted_content[^>]*>.*?</pasted_content[^>]*>|<pasted_content[^>]*>.*", re.S)
IMAGE_TAG = re.compile(r"\[Image[^\]]*\]|\[image[^\]]*\]")
IDE_REQUEST = re.compile(r"##\s*My request for (?:Codex|Claude)[^\n]*\n", re.I)
NO_PREFIX_ORIGINATORS = {"acpx", "ultimate-harness", "@agentclientprotocol/codex-acp", "Claude Code"}
TMP_CWD = ("/tmp/", "/private/tmp/", "/var/folders/", "/private/var/folders/")


def clean_prompt(text):
    if not isinstance(text, str):
        return ""
    if "# Context from my IDE setup" in text and IDE_REQUEST.search(text):
        text = IDE_REQUEST.split(text, maxsplit=1)[-1]
    text = PASTED.sub("", text)
    text = WRAPPER_TAGS.sub("", text)
    text = IMAGE_TAG.sub("", text)
    return text.strip()


def prompt_kind(text):
    """``None`` (not a user turn), ``boundary`` (a user turn) or ``organic`` (a real task prompt)."""
    text = clean_prompt(text)
    if not text or BAD_PROMPT.match(text):
        return None, text
    if re.match(r"^/[\w:-]+(\s|$)", text):  # a slash command (a path has more slashes)
        return None, text
    if len(text.split()) <= 3:
        return "boundary", text
    return "organic", text


def prompt_key(text):
    lowered = re.sub(r"\d+", "0", text.lower())
    lowered = re.sub(r"[^a-z ]", " ", lowered)
    return hashlib.sha1(" ".join(lowered.split()[:14]).encode()).hexdigest()[:12]


# --------------------------------------------------------------------------
# Session builder shared by the parsers
# --------------------------------------------------------------------------

class Builder:
    def __init__(self, session_id, source, raw_path, fixed_turn=None):
        self.id = session_id
        self.source = source
        self.raw_path = raw_path
        self.fixed_turn = fixed_turn  # census: every call belongs to turn 0
        self.turn = fixed_turn if fixed_turn is not None else -1
        self.calls = []
        self.pending = {}
        self.prompts = []
        self.cwd = None
        self.model = None
        self.started = None
        self.ended = None
        self.tokens_in = None
        self.tokens_out = None
        self.flags = set()  # reasons to drop the session (history only)
        self.seen_ids = set()

    def touch(self, ts):
        moment = parse_ts(ts)
        if moment is None:
            return
        if self.started is None or moment < self.started:
            self.started = moment
        if self.ended is None or moment > self.ended:
            self.ended = moment

    def prompt(self, text, ts=None):
        kind, cleaned = prompt_kind(text)
        if kind is None:
            return
        if self.fixed_turn is None:
            self.turn += 1
        self.prompts.append({"organic": kind == "organic", "chars": len(cleaned), "key": prompt_key(cleaned),
                             "hash": hashlib.sha256(cleaned.encode()).hexdigest()[:16]})

    def call(self, call_id, tool, tool_input, ts=None, group=None):
        """Register one tool call; ``group`` is the call_id of the call that owns the shared result."""
        if call_id and call_id in self.seen_ids:
            return None
        if call_id:
            self.seen_ids.add(call_id)
        klass, need = classify.classify(tool, tool_input)
        row = {"seq": len(self.calls), "tool": tool, "input_json": dump_input(tool_input), "output": None,
               "output_bytes": None, "is_error": 0, "class": klass, "need": need, "ts": iso(ts),
               "turn": self.turn, "call_id": call_id}
        self.calls.append(row)
        if group is None and call_id:
            self.pending[call_id] = row
        return row

    def result(self, call_id, output, is_error=False):
        row = self.pending.pop(call_id, None)
        if row is None:
            return
        row["output"], row["output_bytes"] = cap_text(output if output is not None else "")
        row["is_error"] = 1 if is_error else 0

    def finish(self):
        return {"id": self.id, "source": self.source, "raw_path": self.raw_path, "cwd": self.cwd, "model": self.model,
                "started": self.started, "ended": self.ended, "tokens_in": self.tokens_in,
                "tokens_out": self.tokens_out, "prompts": self.prompts, "calls": self.calls, "flags": sorted(self.flags)}


# --------------------------------------------------------------------------
# Claude Code: stream-json and session JSONL
# --------------------------------------------------------------------------

def parse_claude(path, session_id, source, census=False):
    builder = Builder(session_id, source, path, fixed_turn=0 if census else None)
    usage = {}
    for record in read_jsonl(path):
        kind = record.get("type")
        if record.get("isSidechain") and not census:
            builder.flags.add("sidechain")
            continue
        builder.touch(record.get("timestamp"))
        if record.get("cwd") and not builder.cwd:
            builder.cwd = record["cwd"]
        entry = record.get("entrypoint")
        if entry in ("sdk-cli", "sdk-ts", "sdk-py") and not census:
            builder.flags.add("scripted")
        if kind == "system" and record.get("subtype") == "init":
            builder.model = builder.model or record.get("model")
            builder.cwd = builder.cwd or record.get("cwd")
            continue
        message = record.get("message")
        if kind not in ("user", "assistant") or not isinstance(message, dict):
            continue
        content = message.get("content")
        if kind == "assistant":
            model = message.get("model")
            if model and model != "<synthetic>" and not builder.model:
                builder.model = model
            if isinstance(message.get("usage"), dict) and message.get("id"):
                usage[message["id"]] = message["usage"]
            if isinstance(content, list):
                for block in content:
                    if isinstance(block, dict) and block.get("type") == "tool_use":
                        builder.call(block.get("id"), block.get("name") or "", block.get("input") or {}, record.get("timestamp"))
            continue
        if isinstance(content, list) and any(isinstance(b, dict) and b.get("type") == "tool_result" for b in content):
            for block in content:
                if isinstance(block, dict) and block.get("type") == "tool_result":
                    builder.result(block.get("tool_use_id"), flatten(block.get("content")), bool(block.get("is_error")))
            continue
        if census or record.get("isMeta") or record.get("isCompactSummary") or record.get("isVisibleInTranscriptOnly"):
            continue
        if record.get("toolUseResult") is not None or record.get("sourceToolUseID"):
            continue
        text = content if isinstance(content, str) else "\n".join(
            b.get("text", "") for b in content if isinstance(b, dict) and b.get("type") == "text") if isinstance(content, list) else None
        if text:
            builder.prompt(text, record.get("timestamp"))
    if usage:
        builder.tokens_in = sum((u.get("input_tokens") or 0) + (u.get("cache_creation_input_tokens") or 0)
                                + (u.get("cache_read_input_tokens") or 0) for u in usage.values())
        builder.tokens_out = sum(u.get("output_tokens") or 0 for u in usage.values())
    return builder.finish()


# --------------------------------------------------------------------------
# pi sessions
# --------------------------------------------------------------------------

def parse_pi(path, session_id):
    builder = Builder(session_id, "pi-history", path)
    tokens_in = tokens_out = 0
    for record in read_jsonl(path):
        kind = record.get("type")
        builder.touch(record.get("timestamp"))
        if kind == "session":
            builder.cwd = record.get("cwd") or builder.cwd
            continue
        if kind == "model_change":
            builder.model = builder.model or record.get("modelId")
            continue
        message = record.get("message")
        if kind != "message" or not isinstance(message, dict):
            continue
        role, content = message.get("role"), message.get("content")
        if role == "user":
            text = content if isinstance(content, str) else "\n".join(
                b.get("text", "") for b in content if isinstance(b, dict) and b.get("type") == "text") if isinstance(content, list) else None
            if text:
                builder.prompt(text, record.get("timestamp"))
        elif role == "assistant":
            if message.get("model") and not builder.model:
                builder.model = message["model"]
            usage = message.get("usage") or {}
            tokens_in += (usage.get("input") or 0) + (usage.get("cacheRead") or 0) + (usage.get("cacheWrite") or 0)
            tokens_out += usage.get("output") or 0
            if isinstance(content, list):
                for block in content:
                    if isinstance(block, dict) and block.get("type") == "toolCall":
                        builder.call(block.get("id"), block.get("name") or "", block.get("arguments") or {}, record.get("timestamp"))
        elif role == "toolResult":
            builder.result(message.get("toolCallId"), flatten(content), bool(message.get("isError")))
    builder.tokens_in, builder.tokens_out = tokens_in or None, tokens_out or None
    return builder.finish()


# --------------------------------------------------------------------------
# Codex rollouts
# --------------------------------------------------------------------------

JS_TOOL = re.compile(r"\btools\.([A-Za-z_]\w*)\s*\(\s*")


class Expr(str):
    """Raw text of a JS expression the parser could not evaluate (``"rtk bash -c "+q(script)``)."""


def _skip_ws(src, i):
    while i < len(src) and src[i] in " \t\r\n":
        i += 1
    return i


def _quoted(src, i):
    """A JS string literal starting at ``i`` (', " or `): (value, end)."""
    quote = src[i]
    if quote == '"':
        try:
            return json.JSONDecoder().raw_decode(src, i)
        except ValueError:
            pass
    j, out = i + 1, []
    escapes = {"n": "\n", "t": "\t", "r": "\r", "0": "\0"}
    while j < len(src):
        char = src[j]
        if char == "\\" and j + 1 < len(src):
            out.append(escapes.get(src[j + 1], src[j + 1]))
            j += 2
            continue
        if char == quote:
            return "".join(out), j + 1
        out.append(char)
        j += 1
    raise ValueError("unterminated string")


def _expression(src, i):
    """Raw text of a JS expression up to the next top-level ``,``, ``}`` or ``]``."""
    depth, j = 0, i
    while j < len(src):
        char = src[j]
        if char in "\"'`":
            _, j = _quoted(src, j)
            continue
        if char in "{[(":
            depth += 1
        elif char in "}])":
            if depth == 0:
                break
            depth -= 1
        elif char == "," and depth == 0:
            break
        j += 1
    return src[i:j].strip(), j


def _js_value(src, i):
    i = _skip_ws(src, i)
    char = src[i]
    if char in "\"'`":
        value, end = _quoted(src, i)
        after = _skip_ws(src, end)
        if after < len(src) and src[after] not in ",}]":  # `"a" + b`: keep the raw expression
            raw, end = _expression(src, i)
            return Expr(raw), end
        return value, end
    if char == "{":
        return _js_object(src, i)
    if char == "[":
        items, i = [], _skip_ws(src, i + 1)
        while i < len(src) and src[i] != "]":
            value, i = _js_value(src, i)
            items.append(value)
            i = _skip_ws(src, i)
            if i < len(src) and src[i] == ",":
                i = _skip_ws(src, i + 1)
        return items, i + 1
    raw, end = _expression(src, i)
    if not isinstance(raw, Expr) and raw and raw[0] in "\"'`":
        raw = Expr(raw)
    for literal, value in (("true", True), ("false", False), ("null", None)):
        if raw == literal:
            return value, end
    try:
        return json.loads(raw), end
    except ValueError:
        return raw, end


def _js_object(src, i):
    """A JS object literal (quoted or bare keys) starting at ``{``: (dict, end)."""
    result, i = {}, _skip_ws(src, i + 1)
    while i < len(src) and src[i] != "}":
        if src[i] in "\"'`":
            key, i = _quoted(src, i)
        else:
            match = re.compile(r"[A-Za-z_$][\w$]*").match(src, i)
            if not match:
                raise ValueError("bad key")
            key, i = match.group(0), match.end()
        i = _skip_ws(src, i)
        if i < len(src) and src[i] == ":":
            value, i = _js_value(src, i + 1)
        else:
            value = key  # shorthand `{ cmd }`
        result[key] = value
        i = _skip_ws(src, i)
        if i < len(src) and src[i] == ",":
            i = _skip_ws(src, i + 1)
    return result, i + 1


def exec_calls(source):
    """The ``tools.NAME({...})`` calls of one Codex ``exec`` script: [(name, input)]."""
    calls = []
    for match in JS_TOOL.finditer(source):
        name, start = match.group(1), match.end()
        try:
            if start < len(source) and source[start] == "{":
                value, _ = _js_object(source, start)
            elif start < len(source) and source[start] in "\"'`":
                value = {"input": _quoted(source, start)[0]}
            else:
                value = {}
        except (ValueError, IndexError, RecursionError):
            value = {"raw": source[start:start + 400]}
        calls.extend(_expand(name, value, source))
    return calls


IDENTIFIER = re.compile(r"^[A-Za-z_$][\w$]*$")


STRING_LITERAL = re.compile(r"\"(?:[^\"\\\n]|\\.)*\"|'(?:[^'\\\n]|\\.)*'|`(?:[^`\\]|\\.)*`")


def _literals(text):
    out = []
    for match in STRING_LITERAL.finditer(text):
        try:
            out.append(_quoted(text, match.start())[0])
        except (ValueError, IndexError):
            continue
    return out


def _expand(name, value, source):
    """[(tool, input)] for one ``tools.NAME(value)``: variables resolved, ``const cmds = [...]`` loops expanded."""
    value = _resolve_names(value, source)
    command = value.get("cmd") if isinstance(value, dict) else None
    if isinstance(command, Expr):  # `"rtk bash -c "+q(script)`: the longest literal is the command line
        literals = _literals(command)
        if literals:
            value = dict(value, cmd=max(literals, key=len))
    elif isinstance(command, str) and IDENTIFIER.match(command):
        commands = []  # `for (const cmd of cmds)`: every command-looking string of the script's array literals
        for array in re.finditer(r"\b(?:const|let|var)\s+\w+\s*=\s*\[", source):
            depth, j = 1, array.end()
            while j < len(source) and depth:
                if source[j] in "\"'`":
                    try:
                        j = _quoted(source, j)[1]
                    except (ValueError, IndexError):
                        break
                    continue
                depth += (source[j] == "[") - (source[j] == "]")
                j += 1
            commands += [t for t in _literals(source[array.end():j]) if " " in t or t.startswith("rtk")]
        if commands:
            return [(name, dict(value, cmd=text)) for text in commands]
    return [(name, value)]


def _resolve_names(value, source):
    """Replace a bare variable (``{ cmd }`` / ``cmd: command``) by its string, when the script assigns one."""
    if not isinstance(value, dict):
        return value
    resolved = dict(value)
    for key, item in value.items():
        if isinstance(item, str) and IDENTIFIER.match(item):
            found = re.search(r"\b(?:const|let|var)\s+" + re.escape(item) + r"\s*=\s*(?=[\"'`])", source)
            if found:
                try:
                    resolved[key] = _quoted(source, found.end())[0]
                except (ValueError, IndexError):
                    pass
    return resolved


OUTPUT_HEADER = re.compile(
    r"\A(?:Chunk ID: [^\n]*\n)?(?:Script (?:completed|failed|running)[^\n]*\n)?(?:Wall time:? [^\n]*\n)?"
    r"(?:Process exited with code -?\d+\n)?(?:Original token count: \d+\n)?(?:Output:\n\n?)?")
EXIT_CODE = re.compile(r"Process exited with code (-?\d+)|^Exit code: ?(-?\d+)", re.M)

CODEX_LINE = re.compile(r'"type":"(\w+)","payload":\{"type":"(\w+)"')
CODEX_WANTED_RESPONSE = {"function_call", "custom_tool_call", "local_shell_call", "web_search_call",
                         "function_call_output", "custom_tool_call_output", "local_shell_call_output",
                         "tool_search_call", "tool_search_output"}


def codex_tool_name(payload):
    namespace, name = payload.get("namespace"), payload.get("name") or ""
    return f"{namespace}__{name}" if namespace and namespace.startswith("mcp__") else name


def _lines(path):
    with open(path, encoding="utf-8", errors="replace") as handle:
        yield from handle


def parse_codex(path, session_id):
    builder = Builder(session_id, "codex-history", path)
    groups = {}  # exec call_id -> first expanded row (holds the shared result)
    last_tokens = None
    for raw in _lines(path):
        head = CODEX_LINE.search(raw, 0, 400)
        top, sub = (head.group(1), head.group(2)) if head else (None, None)
        if top in ("token_usage_record", "world_state", "compacted", "inter_agent_communication_metadata", None) and top != "session_meta":
            if top is None:
                try:
                    record = json.loads(raw)
                except ValueError:
                    continue
                top, sub = record.get("type"), (record.get("payload") or {}).get("type")
            else:
                continue
        if top == "event_msg":
            if sub == "token_count":
                last_tokens = raw
                continue
            if sub not in ("item_completed", "user_message") or (sub == "item_completed" and '"UserMessage"' not in raw):
                continue
        elif top == "response_item":
            if sub not in CODEX_WANTED_RESPONSE:
                continue
        elif top not in ("session_meta", "turn_context"):
            continue
        if top == "turn_context" and builder.model:
            continue
        try:
            record = json.loads(raw)
        except ValueError:
            continue
        payload = record.get("payload") or {}
        ts = record.get("timestamp")
        builder.touch(ts)
        if top == "session_meta":
            builder.cwd = payload.get("cwd") or builder.cwd
            if payload.get("thread_source") == "subagent" or isinstance(payload.get("source"), dict):
                builder.flags.add("subagent")
            if payload.get("originator") in NO_PREFIX_ORIGINATORS:
                builder.flags.add("scripted")
            builder.touch(payload.get("timestamp"))
        elif top == "turn_context":
            builder.model = payload.get("model") or builder.model
        elif top == "event_msg":
            if sub == "user_message":
                builder.prompt(payload.get("message") or "", ts)
            else:
                item = payload.get("item") or {}
                if item.get("type") == "UserMessage":
                    builder.prompt("\n".join(c.get("text", "") for c in item.get("content") or [] if isinstance(c, dict)), ts)
        elif sub == "function_call":
            arguments = payload.get("arguments")
            try:
                tool_input = json.loads(arguments) if isinstance(arguments, str) else (arguments or {})
            except ValueError:
                tool_input = {"arguments": arguments}
            builder.call(payload.get("call_id"), codex_tool_name(payload), tool_input, ts)
        elif sub == "local_shell_call":
            action = payload.get("action") or {}
            builder.call(payload.get("call_id") or payload.get("id"), "local_shell",
                         {"cmd": " ".join(action.get("command") or []) if isinstance(action.get("command"), list) else action.get("command", "")}, ts)
        elif sub == "custom_tool_call":
            name, source = payload.get("name") or "", payload.get("input")
            call_id = payload.get("call_id")
            if name == "exec" and isinstance(source, str):
                expanded = exec_calls(source) or [("exec", {"script": source[:INPUT_CAP]})]
                first = None
                for index, (inner, inner_input) in enumerate(expanded):
                    row = builder.call(f"{call_id}#{index}", inner, inner_input, ts, group=call_id if index else None)
                    if index == 0:
                        first = row
                    elif row is not None:
                        row["output"], row["output_bytes"] = "", 0
                if first is not None and call_id:
                    builder.pending[call_id] = first
                    builder.seen_ids.add(call_id)
            else:
                tool_input = {"patch": source} if name == "apply_patch" else {"input": source}
                builder.call(call_id, name, tool_input, ts)
        elif sub == "web_search_call":
            action = payload.get("action") or {}
            builder.call(payload.get("id"), "web_search", {"query": action.get("query") or action.get("url") or ""}, ts)
            if builder.calls:
                row = builder.calls[-1]
                row["output"], row["output_bytes"] = "", 0
                builder.pending.pop(payload.get("id"), None)
        elif sub in ("tool_search_call",):
            builder.call(payload.get("call_id") or payload.get("id"), "tool_search_call", payload.get("arguments") or {}, ts)
        elif sub in ("function_call_output", "custom_tool_call_output", "local_shell_call_output", "tool_search_output"):
            text = flatten(payload.get("output"))
            error = text.startswith(("Script failed", "aborted by user", "failed to parse function arguments"))
            for match in EXIT_CODE.finditer(text[:600]):
                error = error or int(match.group(1) or match.group(2)) != 0
            call_id = payload.get("call_id")
            row = builder.pending.get(call_id)
            if row is not None and row["tool"] in ("exec_command", "write_stdin", "exec", "wait", "local_shell") or sub != "function_call_output":
                header = OUTPUT_HEADER.match(text)
                text = text[header.end():] if header else text
            builder.result(call_id, text, error)
    if last_tokens:
        try:
            usage = (json.loads(last_tokens).get("payload", {}).get("info") or {}).get("total_token_usage") or {}
            builder.tokens_in, builder.tokens_out = usage.get("input_tokens"), usage.get("output_tokens")
        except ValueError:
            pass
    return builder.finish()


# --------------------------------------------------------------------------
# Census raw streams
# --------------------------------------------------------------------------

def census_session_id(path):
    name = os.path.basename(path)
    return name[:-len(".stream.jsonl")] if name.endswith(".stream.jsonl") else os.path.splitext(name)[0]


def parse_census(path):
    return parse_claude(path, census_session_id(path), "census", census=True)


# --------------------------------------------------------------------------
# Ingest
# --------------------------------------------------------------------------

def replace_calls(conn, session_id, calls):
    conn.execute("DELETE FROM calls WHERE session=?", (session_id,))
    conn.executemany(
        "INSERT INTO calls(session, seq, tool, input_json, output, output_bytes, is_error, class, need, ts, turn, call_id) "
        "VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
        [(session_id, c["seq"], c["tool"], c["input_json"], c["output"], c["output_bytes"], c["is_error"], c["class"],
          c["need"], c["ts"], c["turn"], c["call_id"]) for c in calls])


def file_state(path):
    stat = os.stat(path)
    return stat.st_size, stat.st_mtime_ns


def is_current(conn, path):
    size, mtime = file_state(path)
    row = conn.execute("SELECT size, mtime_ns FROM ingest_state WHERE path=?", (path,)).fetchone()
    return row == (size, mtime)


def ingest_census(conn, verbose=True):
    """(files seen, files ingested, calls written) for the census raw directory."""
    paths = sorted(glob.glob(os.path.join(RAW_DIR, "*.stream.jsonl")))
    changed = written = 0
    for path in paths:
        if is_current(conn, path):
            continue
        size, mtime = file_state(path)
        session = parse_census(path)
        with conn:
            replace_calls(conn, session["id"], session["calls"])
            conn.execute("INSERT OR REPLACE INTO ingest_state VALUES (?,?,?,?,?)", (path, "census", size, mtime, session["id"]))
        changed += 1
        written += len(session["calls"])
        if verbose:
            print(f"  census {session['id']}: {len(session['calls'])} calls", flush=True)
    return len(paths), changed, written


HISTORY_SOURCES = ("claude", "codex", "pi")


def history_files(sources, cutoff):
    """[(kind, path, session_id)] of candidate main-session files modified since ``cutoff``."""
    found = []
    mtime_floor = cutoff.timestamp()
    if "claude" in sources:
        for path in glob.glob(os.path.join(HOME, ".claude/projects/*/*.jsonl")):
            found.append(("claude", path, os.path.splitext(os.path.basename(path))[0]))
    if "codex" in sources:
        for path in glob.glob(os.path.join(HOME, ".codex/sessions/*/*/*/rollout-*.jsonl")):
            stem = os.path.splitext(os.path.basename(path))[0]
            found.append(("codex", path, stem[-36:] if re.search(r"[0-9a-f-]{36}$", stem) else stem))
    if "pi" in sources:
        for path in glob.glob(os.path.join(HOME, ".pi/agent/sessions/*/*.jsonl")):
            stem = os.path.splitext(os.path.basename(path))[0]
            found.append(("pi", path, stem.split("_", 1)[-1]))
    return [item for item in found if os.path.getmtime(item[1]) >= mtime_floor]


def parse_history(job):
    kind, path, session_id = job
    try:
        if kind == "claude":
            return parse_claude(path, f"claude-history:{session_id}", "claude-history")
        if kind == "codex":
            return parse_codex(path, f"codex-history:{session_id}")
        return parse_pi(path, f"pi-history:{session_id}")
    except Exception as error:  # a corrupt transcript must not stop a 14 GB run
        return {"id": f"{kind}-history:{session_id}", "error": f"{type(error).__name__}: {error}", "path": path}


def qualifies(session, cutoff):
    """Reason string when a parsed history session is dropped, else ``None``."""
    if "error" in session:
        return "error"
    if session["flags"]:
        return session["flags"][0]
    if not any(p["organic"] for p in session["prompts"]):
        return "no-organic-prompt"
    started = session["started"]
    if started is None or started < cutoff:
        return "outside-window"
    cwd = session["cwd"] or ""
    if cwd.startswith(TMP_CWD):
        return "tmp-cwd"
    return None


def write_history_session(conn, session):
    organic = [p for p in session["prompts"] if p["organic"]]
    edited = any(c["class"] in ("edit", "write") for c in session["calls"])
    wall = (session["ended"] - session["started"]).total_seconds() if session["started"] and session["ended"] else None
    conn.execute(
        "INSERT OR REPLACE INTO sessions(id, source, repo, sha, category, mode, prompt, prompt_hash, started_at, wall_s, model, "
        "tokens_in, tokens_out, cost_usd, final_answer, ok, raw_path) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        (session["id"], session["source"], hash_repo(session["cwd"]), None, None, "edit" if edited else "answer", None,
         organic[0]["hash"], iso(session["started"]), wall, session["model"], session["tokens_in"], session["tokens_out"],
         None, None, 1, session["raw_path"]))
    replace_calls(conn, session["id"], session["calls"])
    conn.execute("DELETE FROM history_prompts WHERE session=?", (session["id"],))
    conn.executemany("INSERT INTO history_prompts VALUES (?,?,?,?,?)",
                     [(session["id"], i, p["key"], p["chars"], 1 if p["organic"] else 0) for i, p in enumerate(session["prompts"])])


def forget_session(conn, session_id):
    for table, column in (("calls", "session"), ("sessions", "id"), ("history_prompts", "session")):
        conn.execute(f"DELETE FROM {table} WHERE {column}=?", (session_id,))


def prune_history(conn):
    """Drop sessions whose first organic prompt is a scripted template; dedupe replayed Claude calls."""
    counts = collections.Counter()
    for (key,) in conn.execute("SELECT key FROM history_prompts WHERE organic=1"):
        counts[key] += 1

    def scripted(key, chars):
        return (counts[key] > 2 and chars > 300) or (counts[key] >= 8 and chars >= 60)

    first_organic = {}  # session -> (key, chars) of its first organic prompt: a templated start means a scripted run
    for session, key, chars in conn.execute("SELECT session, key, chars FROM history_prompts WHERE organic=1 ORDER BY session, idx"):
        first_organic.setdefault(session, (key, chars))
    present = {r[0] for r in conn.execute("SELECT id FROM sessions WHERE source LIKE '%-history'")}
    dropped = [s for s in present if s not in first_organic or scripted(*first_organic[s])]
    with conn:  # history_prompts stays: it is the registry the repeat counts come from
        for session in dropped:
            conn.execute("DELETE FROM calls WHERE session=?", (session,))
            conn.execute("DELETE FROM sessions WHERE id=?", (session,))
    # a resumed Claude session replays the earlier tool_use ids: keep each in its earliest session
    with conn:
        before = conn.execute("SELECT COUNT(*) FROM calls").fetchone()[0]
        conn.execute(
            "DELETE FROM calls WHERE rowid IN (SELECT c.rowid FROM calls c JOIN sessions s ON s.id=c.session "
            "WHERE s.source='claude-history' AND c.call_id LIKE 'toolu_%' AND EXISTS ("
            "SELECT 1 FROM calls c2 JOIN sessions s2 ON s2.id=c2.session WHERE c2.call_id=c.call_id AND "
            "s2.source='claude-history' AND c2.session<>c.session AND (COALESCE(s2.started_at,'')<COALESCE(s.started_at,'') OR "
            "(COALESCE(s2.started_at,'')=COALESCE(s.started_at,'') AND c2.session<c.session))))")
        replayed = before - conn.execute("SELECT COUNT(*) FROM calls").fetchone()[0]
    return len(dropped), replayed


def ingest_history(conn, days=120, sources=HISTORY_SOURCES, jobs=None, limit=None, force=False):
    cutoff = datetime.now(timezone.utc) - timedelta(days=days)
    candidates = history_files(sources, cutoff)
    if not force:
        candidates = [c for c in candidates if not is_current(conn, c[1])]
    candidates.sort(key=lambda c: -os.path.getsize(c[1]))  # big files first: better pool balance
    if limit:
        candidates = candidates[:limit]
    total = len(candidates)
    print(f"history: {total} files to parse (window {days} days, sources {','.join(sources)})", flush=True)
    stats = collections.Counter()
    started = time.time()
    workers = jobs or max(1, min(8, (os.cpu_count() or 4) - 2))
    context = multiprocessing.get_context("spawn")
    with context.Pool(workers) as pool:
        for done, session in enumerate(pool.imap_unordered(parse_history, candidates, chunksize=1), 1):
            path = session.get("raw_path") or session.get("path")
            reason = qualifies(session, cutoff)
            size, mtime = file_state(path)
            with conn:
                if reason is None:
                    write_history_session(conn, session)
                    stats["kept:" + session["source"]] += 1
                    stats["calls:" + session["source"]] += len(session["calls"])
                else:
                    forget_session(conn, session["id"])
                    stats["dropped:" + reason] += 1
                conn.execute("INSERT OR REPLACE INTO ingest_state VALUES (?,?,?,?,?)",
                             (path, "history", size, mtime, session["id"] if reason is None else None))
            if done % 250 == 0 or done == total:
                print(f"  {done}/{total} files, {time.time() - started:.0f}s", flush=True)
    dropped, replayed = prune_history(conn)
    return stats, dropped, replayed


def reclassify(conn):
    """Recompute class and need of every stored call from its tool and input (no transcript re-parse)."""
    updates = []
    for rowid, tool, input_json, klass, need in conn.execute("SELECT rowid, tool, input_json, class, need FROM calls"):
        try:
            parsed = json.loads(input_json) if input_json else {}
        except ValueError:
            parsed = {}
        new = classify.classify(tool, parsed)
        if new != (klass, need):
            updates.append((new[0], new[1], rowid))
    with conn:
        conn.executemany("UPDATE calls SET class=?, need=? WHERE rowid=?", updates)
    return len(updates)


# --------------------------------------------------------------------------
# Summary (aggregates only: no prompt, command or output text is printed)
# --------------------------------------------------------------------------

def pct(part, whole):
    return f"{100 * part / whole:.1f}%" if whole else "n/a"


def table(headers, rows):
    widths = [max(len(str(h)), *(len(str(r[i])) for r in rows)) if rows else len(str(h)) for i, h in enumerate(headers)]
    line = lambda cells: "| " + " | ".join(str(c).rjust(w) if i else str(c).ljust(w) for i, (c, w) in enumerate(zip(cells, widths))) + " |"
    return "\n".join([line(headers), "|" + "|".join("-" * (w + 2) for w in widths) + "|"] + [line(r) for r in rows])


def quantile(values, q):
    if not values:
        return 0
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int(q * len(ordered)))]


def summary(conn, out=sys.stdout):
    def emit(text=""):
        print(text, file=out)

    rows = conn.execute(
        "SELECT c.session, COALESCE(s.source, 'census'), c.seq, c.tool, c.class, c.need, c.output_bytes, c.is_error, c.turn "
        "FROM calls c LEFT JOIN sessions s ON s.id=c.session").fetchall()
    sessions = collections.defaultdict(lambda: collections.defaultdict(list))
    for row in conn.execute("SELECT id, source FROM sessions"):
        sessions[row[1]][row[0]] = []
    by_source = collections.defaultdict(list)
    for call in rows:
        by_source[call[1]].append(call)
        sessions[call[1]][call[0]].append(call)
    sources = sorted(sessions, key=lambda s: (s != "census", s))
    emit(f"census.db: {sum(len(v) for v in by_source.values())} calls, {sum(len(v) for v in sessions.values())} sessions")
    emit()
    emit("## Calls per session")
    table_rows = []
    for source in sources:
        per = [len(calls) for calls in sessions[source].values()]
        table_rows.append((source, len(per), sum(1 for n in per if n), sum(per),
                           f"{statistics.mean(per):.1f}" if per else "n/a", f"{statistics.median(per):g}" if per else "n/a",
                           quantile(per, 0.9), max(per) if per else 0))
    emit(table(("source", "sessions", "with calls", "calls", "mean", "median", "p90", "max"), table_rows))

    emit("\n## Class distribution (% of calls)")
    classes = list(classify.CLASSES)
    emit(table(["source", "calls"] + classes, [
        [s, len(by_source[s])] + [pct(sum(1 for c in by_source[s] if c[4] == k), len(by_source[s])) for k in classes] for s in sources]))

    emit("\n## Need distribution (% of calls; unknown = no rule matched)")
    needs = sorted({c[5] for c in rows}, key=lambda n: -sum(1 for c in rows if c[5] == n))
    emit(table(["need", "all"] + sources, [
        [n, pct(sum(1 for c in rows if c[5] == n), len(rows))] + [pct(sum(1 for c in by_source[s] if c[5] == n), len(by_source[s])) for s in sources]
        for n in needs]))

    emit("\n## Top tools (share of calls)")
    for source in sources:
        counter = collections.Counter(c[3] for c in by_source[source])
        emit(f"{source}: " + ", ".join(f"{t} {pct(n, len(by_source[source]))}" for t, n in counter.most_common(10)))

    emit("\n## Output bytes (true size, before the 16 KB cap)")
    table_rows = []
    for source in sources:
        sizes = [c[6] for c in by_source[source] if c[6] is not None]
        table_rows.append((source, len(sizes), f"{sum(sizes) / 1e6:.1f} MB", f"{statistics.mean(sizes):.0f}" if sizes else "n/a",
                           f"{statistics.median(sizes):g}" if sizes else "n/a", quantile(sizes, 0.9),
                           pct(sum(1 for n in sizes if n > OUTPUT_CAP), len(sizes))))
    emit(table(("source", "with output", "total", "mean B", "median B", "p90 B", ">16KB"), table_rows))

    emit("\n## Error rate")
    emit(table(["source", "calls", "errors", "rate"] + [f"{k}" for k in ("read", "search", "build", "test", "run")], [
        [s, len(by_source[s]), sum(c[7] for c in by_source[s]), pct(sum(c[7] for c in by_source[s]), len(by_source[s]))] +
        [pct(sum(c[7] for c in by_source[s] if c[4] == k), sum(1 for c in by_source[s] if c[4] == k)) for k in ("read", "search", "build", "test", "run")]
        for s in sources]))

    emit("\n## Calls before the first edit or the answer (first turn only)")
    table_rows = []
    for source in sources:
        before = total = edited = counted = 0
        for calls in sessions[source].values():
            first = sorted((c for c in calls if c[8] == 0), key=lambda c: c[2])
            if not first:
                continue
            counted += 1
            stop = next((i for i, c in enumerate(first) if c[4] in ("edit", "write")), None)
            edited += stop is not None
            before += stop if stop is not None else len(first)
            total += len(first)
        table_rows.append((source, counted, pct(edited, counted), before, total, pct(before, total),
                           f"{before / counted:.1f}" if counted else "n/a"))
    emit(table(("source", "sessions with calls", "first turn edits", "calls before stop", "first-turn calls", "% before", "mean before"), table_rows))
    emit("(stop = first edit/write call, else the end of the first turn = its answer)")


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------

def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = parser.add_subparsers(dest="command", required=True)
    ingest = sub.add_parser("ingest", help="parse transcripts into census.db")
    ingest.add_argument("--census", action="store_true", help="raw/*.stream.jsonl of the census run")
    ingest.add_argument("--history", action="store_true", help="Claude, Codex and pi sessions on disk")
    ingest.add_argument("--days", type=int, default=120)
    ingest.add_argument("--source", default=",".join(HISTORY_SOURCES), help="history sources, comma separated")
    ingest.add_argument("--jobs", type=int, default=None)
    ingest.add_argument("--limit", type=int, default=None, help="parse at most N history files (largest first)")
    ingest.add_argument("--force", action="store_true", help="re-parse files whose size and mtime did not change")
    sub.add_parser("watch", help="poll the raw directory every 3 s and ingest what changed")
    sub.add_parser("reclassify", help="recompute class and need of the stored calls (after a rule change)")
    sub.add_parser("summary", help="print aggregate tables")
    args = parser.parse_args(argv)
    conn = connect()
    if args.command == "summary":
        summary(conn)
    elif args.command == "reclassify":
        print(f"reclassify: {reclassify(conn)} calls changed")
    elif args.command == "watch":
        print(f"watching {RAW_DIR} -> {DB_PATH} (ctrl-c to stop)", flush=True)
        while True:
            seen, changed, written = ingest_census(conn)
            if changed:
                print(f"{time.strftime('%H:%M:%S')} {seen} files, {changed} changed, {written} calls", flush=True)
            time.sleep(3)
    else:
        if not (args.census or args.history):
            parser.error("ingest needs --census and/or --history")
        if args.census:
            seen, changed, written = ingest_census(conn, verbose=False)
            print(f"census: {seen} raw files, {changed} (re)ingested, {written} calls written")
        if args.history:
            stats, dropped, replayed = ingest_history(
                conn, args.days, tuple(s for s in args.source.split(",") if s in HISTORY_SOURCES), args.jobs, args.limit, args.force)
            print("history:", dict(sorted(stats.items())))
            print(f"history: pruned {dropped} scripted-template sessions, removed {replayed} replayed claude calls")


if __name__ == "__main__":
    main()
