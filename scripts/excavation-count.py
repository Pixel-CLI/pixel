#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Count how much an agent excavates before it answers or edits (issue #883).

*Excavation* is the native exploration an agent does to find the code a prompt
is about: Grep, Glob, Read, and Bash commands that search or read files
(``grep``/``rg``/``find``/``cat``/``ls``/``sed -n`` and their kin). The
prompt-submit brief is meant to make that number smaller, so an A/B arm with
the brief off (``PIXEL_BRIEF=0``) is compared with an arm with it on.

The script reads agent transcripts (Claude Code JSONL, pi session JSONL),
takes one prompt of each (the first by default) and walks the tool calls that
follow it, parsing every ``tool_use`` / ``toolCall`` input; transcript prose is
never pattern-matched (the one text test is for the brief's tag inside a hook
attachment, below). It counts, before the stop marker:

* ``native``    exploration calls (Grep, Glob, Read, Find, Ls, and exploring Bash);
* ``pixel``     calls to the pixel CLI (its own retrieval, not native search);
* ``delegated`` subagent spawns (their exploration lives in other transcripts).

The headline stop is whichever comes first of the first edit (Edit, Write,
MultiEdit, NotebookEdit, or a Bash command that writes a file) and the answer
(the closing assistant text of the turn: the one no tool call follows). The
counts at the first assistant text of any kind, at the first edit, at the
answer, and at the end of the turn are reported too.

    python3 scripts/excavation-count.py ~/.claude/projects/<slug>/<session>.jsonl
    python3 scripts/excavation-count.py --arm off='runs/off/*.jsonl' --arm on='runs/on/*.jsonl'

Each result also says whether the transcript carries the injected brief (a
Claude Code ``hook_additional_context`` attachment, a pi ``pixel-brief`` custom
message), so an A/B arm can be checked to be what it claims: ``--require
on=brief --require off=no-brief`` drops the runs that are not.

``--self-test`` runs the parser tests. Standard library only.
"""

import argparse
import contextlib
import glob
import io
import hashlib
import json
import os
import re
import shlex
import statistics
import sys
import tempfile
import unittest
from pathlib import Path

EDIT_TOOLS = {"edit", "write", "multiedit", "notebookedit", "apply_patch", "str_replace_editor"}
NATIVE_TOOLS = {"read", "grep", "glob", "find", "ls", "notebookread"}
DELEGATE_TOOLS = {"agent", "task", "subagent", "scout"}
EXPLORE_VERBS = {"grep", "egrep", "fgrep", "rg", "ag", "ack", "find", "fd", "fdfind", "cat", "bat",
                 "head", "tail", "ls", "tree", "less", "more", "nl"}
GIT_EXPLORE = {"grep", "ls-files", "ls-tree", "cat-file"}
# Wrappers that run the command after their options. `short` lists the option
# letters that take an argument (attached, `-sKILL`, or the next token),
# `long` the long options that do (`--signal KILL`; `--signal=KILL` is whole),
# `positional` the arguments before the command (timeout's duration) and
# `lookup` the options that make the wrapper look a command up instead of run it.
WRAPPER_SPECS = {
    "timeout": {"short": "sk", "long": {"--signal", "--kill-after"}, "positional": 1},
    "nice": {"short": "n", "long": {"--adjustment"}},
    "env": {"short": "uCSP", "long": {"--unset", "--chdir", "--split-string"}},
    "command": {"lookup": {"-v", "-V"}},
    "time": {"short": "fo", "long": {"--format", "--output"}},
    "exec": {"short": "a"},
    "sudo": {"short": "ugCDhprRtTU", "long": {"--user", "--group", "--chdir", "--host", "--prompt", "--role", "--type"}},
    "nohup": {},
    "stdbuf": {"short": "ioe", "long": {"--input", "--output", "--error"}},
    "xargs": {"short": "InPLsdEaJRS", "long": {"--arg-file", "--delimiter", "--max-args", "--max-procs",
                                              "--max-chars", "--max-lines", "--eof"}},
}
WRAPPER_ALIASES = {"gtimeout": "timeout", "gnice": "nice", "genv": "env", "gstdbuf": "stdbuf"}
SHELLS = {"sh", "bash", "zsh", "dash", "ksh"}
SHELL_C = re.compile(r"^-[A-Za-z]*c$")
OPERATORS = {"&&", "||", ";", ";;", "&", "(", ")", "{", "}", "\n"}
PIXEL_BINARIES = {"pixel", "pixel-dev", "gitpixel"}
ASSIGNMENT = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")
STOPS = ("first_text", "first_edit", "answer", "end")
BRIEF_TAG = "[PIXEL:BRIEF]"
SKIP_PROMPT_PREFIXES = ("<command-", "<local-command", "<system-reminder", "Caveat:", "<task-notification")


# --------------------------------------------------------------------------
# Bash classification
# --------------------------------------------------------------------------

HEREDOC = re.compile(r"<<-?\s*(['\"]?)([A-Za-z_][A-Za-z0-9_]*)\1")


def strip_heredocs(command):
    """The command without heredoc bodies, so their text is never read as shell."""
    kept, delimiter = [], None
    for line in command.split("\n"):
        if delimiter is not None:
            if line.strip() == delimiter:
                delimiter = None
            continue
        kept.append(line)
        found = HEREDOC.search(line)
        if found:
            delimiter = found.group(2)
    return "\n".join(kept)


def split_pipelines(command):
    """Stages of each pipeline as token lists, or None when unparseable.

    Splits on ``;``, ``&&``, ``||``, ``&`` and newlines, and each pipeline on
    ``|``. Only a pipeline's first stage can explore (``cargo test | grep FAIL``
    filters, it does not search), but any stage can write a file (``| tee``).
    Redirection targets stay in the tokens so a writing command is recognised.
    """
    try:
        lexer = shlex.shlex(strip_heredocs(command), posix=True, punctuation_chars=True)
        lexer.whitespace_split = True
        tokens = list(lexer)
    except ValueError:
        return None
    pipelines, stages, current = [], [], []
    for token in tokens:
        if token in ("|", "|&"):
            stages.append(current)
            current = []
        elif token in OPERATORS:
            stages.append(current)
            pipelines.append([stage for stage in stages if stage])
            stages, current = [], []
        else:
            current.append(token)
    stages.append(current)
    pipelines.append([stage for stage in stages if stage])
    return [pipeline for pipeline in pipelines if pipeline]


def skip_wrapper(name, tokens):
    """The command tokens after wrapper ``name`` and its options; ``[]`` when
    it runs none (``command -v rg`` looks `rg` up, a bare ``timeout 5`` has
    nothing to run)."""
    spec = WRAPPER_SPECS[name]
    tokens = list(tokens)
    while tokens and tokens[0].startswith("-") and tokens[0] != "-":
        option = tokens.pop(0)
        if option == "--":
            break
        if option in spec.get("lookup", ()):
            return []
        if option.startswith("--"):
            if "=" not in option and option in spec.get("long", ()) and tokens:
                tokens.pop(0)
            continue
        letters = option[1:]
        for index, letter in enumerate(letters):
            if letter in spec.get("short", ""):
                if index == len(letters) - 1 and tokens:
                    tokens.pop(0)  # `-s KILL`; `-sKILL` carries its own argument
                break
    for _ in range(spec.get("positional", 0)):
        if tokens:
            tokens.pop(0)
    return tokens


def strip_wrappers(tokens):
    """Drop env assignments, rtk, and the wrappers that run another command."""
    tokens = list(tokens)
    while tokens:
        head = tokens[0]
        base = WRAPPER_ALIASES.get(os.path.basename(head), os.path.basename(head))
        if ASSIGNMENT.match(head):
            tokens.pop(0)
        elif base == "rtk":
            tokens.pop(0)
            if tokens and tokens[0] == "proxy":
                tokens.pop(0)
            elif tokens and tokens[0] == "read":
                tokens[0] = "cat"
        elif base in WRAPPER_SPECS:
            tokens = skip_wrapper(base, tokens[1:])
        else:
            break
    return tokens


def writes_a_file(tokens):
    """Whether one command line edits files: ``sed -i``, ``tee``, ``>`` and friends."""
    base = os.path.basename(tokens[0]) if tokens else ""
    flags = [t for t in tokens[1:] if t.startswith("-")]
    if base in ("sed", "perl") and any(re.match(r"^-[A-Za-z]*i", f) or f.startswith("--in-place") for f in flags):
        return True
    if base == "patch" or (base == "git" and "apply" in tokens[1:3]) or base == "tee":
        return True
    for index, token in enumerate(tokens):
        if token in (">", ">>") and index + 1 < len(tokens) and tokens[index + 1] not in ("/dev/null", "&"):
            return True
    return False


def classify_pipeline(stages, depth=0):
    first = strip_wrappers(stages[0])
    if not first:
        return "other"
    if any(writes_a_file(strip_wrappers(stage)) for stage in stages):
        return "edit"
    base = os.path.basename(first[0])
    if base in SHELLS and depth < 2:
        for position, token in enumerate(first[1:], 1):
            if not token.startswith("-"):
                break  # a script path: not an inline command
            if SHELL_C.match(token):  # -c, -lc, -ic: the next token is the command line
                inner = first[position + 1:][:1]
                return classify_bash(inner[0], depth + 1) if inner else "other"
    if base in PIXEL_BINARIES:
        return "pixel"
    if base == "sed":
        flags = [t for t in first[1:] if t.startswith("-") and not t.startswith("--")]
        return "explore" if any("n" in f[1:] for f in flags) or "--quiet" in first or "--silent" in first else "other"
    if base == "git":
        sub = next((t for t in first[1:] if not t.startswith("-")), "")
        return "explore" if sub in GIT_EXPLORE else "other"
    return "explore" if base in EXPLORE_VERBS else "other"


def classify_bash(command, depth=0):
    """``edit``, ``explore``, ``pixel`` or ``other``, one per Bash call (that order wins)."""
    pipelines = split_pipelines(command or "")
    if pipelines is None:
        pipelines = [[(command or "").split()]]
    verdicts = {classify_pipeline(p, depth) for p in pipelines if p}
    for kind in ("edit", "explore", "pixel"):
        if kind in verdicts:
            return kind
    return "other"


def classify_tool(name, tool_input):
    """Bucket of one tool call: edit, explore, pixel, delegate or other, and its label."""
    lowered = (name or "").lower()
    if lowered in EDIT_TOOLS:
        return "edit", name
    if lowered in DELEGATE_TOOLS:
        return "delegate", name
    if lowered in NATIVE_TOOLS:
        return "explore", name
    if lowered == "bash":
        command = tool_input.get("command") if isinstance(tool_input, dict) else None
        return classify_bash(command if isinstance(command, str) else ""), "Bash"
    return "other", name


# --------------------------------------------------------------------------
# Transcript readers: each yields ("prompt", text) | ("text", text) | ("tool", name, input) | ("brief",)
# --------------------------------------------------------------------------

def read_lines(path):
    with open(path, encoding="utf-8", errors="replace") as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                yield json.loads(line)
            except json.JSONDecodeError:
                continue


def text_of(content):
    """User text of a message content, ignoring tool results; None when there is none."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = [p.get("text", "") for p in content if isinstance(p, dict) and p.get("type") == "text"]
        return "\n".join(parts) if parts else None
    return None


def is_prompt(text):
    return bool(text and text.strip() and not text.lstrip().startswith(SKIP_PROMPT_PREFIXES))


def claude_events(path, include_sidechain=False):
    seen = set()
    for record in read_lines(path):
        kind = record.get("type")
        if record.get("isSidechain") and not include_sidechain:
            continue
        if kind == "attachment":
            # A hook's additional context is its own record, after the prompt it answers.
            attachment = record.get("attachment")
            if (isinstance(attachment, dict) and attachment.get("type") == "hook_additional_context"
                    and BRIEF_TAG in json.dumps(attachment.get("content"))):
                yield ("brief",)
            continue
        if kind not in ("user", "assistant"):
            continue
        message = record.get("message")
        if not isinstance(message, dict):
            continue
        content = message.get("content")
        if kind == "user":
            if record.get("isMeta") or record.get("isCompactSummary"):
                continue
            text = text_of(content)
            if is_prompt(text):
                yield ("prompt", text)
        elif isinstance(content, list):
            for part in content:
                if not isinstance(part, dict):
                    continue
                if part.get("type") == "text" and part.get("text", "").strip():
                    yield ("text", part["text"])
                elif part.get("type") == "tool_use":
                    ident = part.get("id")
                    if ident and ident in seen:
                        continue
                    seen.add(ident)
                    yield ("tool", part.get("name", ""), part.get("input") or {})


def pi_events(path, include_sidechain=False):
    seen = set()
    for record in read_lines(path):
        if record.get("type") == "custom_message" and record.get("customType") == "pixel-brief":
            yield ("brief",)  # the pi extension's injection
            continue
        message = record.get("message")
        if record.get("type") != "message" or not isinstance(message, dict):
            continue
        role, content = message.get("role"), message.get("content")
        if role == "user":
            text = text_of(content)
            if is_prompt(text):
                yield ("prompt", text)
        elif role == "assistant" and isinstance(content, list):
            for part in content:
                if not isinstance(part, dict):
                    continue
                if part.get("type") == "text" and part.get("text", "").strip():
                    yield ("text", part["text"])
                elif part.get("type") == "toolCall":
                    ident = part.get("id")
                    if ident and ident in seen:
                        continue
                    seen.add(ident)
                    yield ("tool", part.get("name", ""), part.get("arguments") or {})


def detect_agent(path):
    for count, record in enumerate(read_lines(path)):
        if record.get("type") == "session" or (record.get("type") == "message" and "message" in record):
            return "pi"
        if record.get("type") in ("user", "assistant") and isinstance(record.get("message"), dict):
            return "claude"
        if count > 200:
            break
    raise ValueError(f"{path}: neither a Claude Code nor a pi transcript")


# --------------------------------------------------------------------------
# Counting
# --------------------------------------------------------------------------

def empty_counts():
    return {"native": 0, "pixel": 0, "delegated": 0, "by_tool": {}}


def add_call(counts, bucket, label):
    if bucket == "explore":
        counts["native"] += 1
        counts["by_tool"][label] = counts["by_tool"].get(label, 0) + 1
    elif bucket == "pixel":
        counts["pixel"] += 1
    elif bucket == "delegate":
        counts["delegated"] += 1


def snapshot(counts):
    return {**counts, "by_tool": dict(sorted(counts["by_tool"].items()))}


def turn_events(events, prompt_index):
    """The prompt text and the events between it and the next prompt."""
    turn, index = None, -1
    for event in events:
        if event[0] == "prompt":
            index += 1
            if index == prompt_index:
                turn = (event[1], [])
            elif index > prompt_index:
                break
        elif turn is not None:
            turn[1].append(event)
    return turn


def count_turn(body):
    """Counts at each stop marker over one turn's events."""
    answer_at = None
    for position in range(len(body) - 1, -1, -1):
        if body[position][0] == "tool":
            break
        if body[position][0] == "text":
            answer_at = position
    counts, marks = empty_counts(), {}
    for position, event in enumerate(body):
        if event[0] == "text":
            marks.setdefault("first_text", snapshot(counts))
            if position == answer_at:
                marks.setdefault("answer", snapshot(counts))
        elif event[0] == "tool":
            bucket, label = classify_tool(event[1], event[2])
            if bucket == "edit":
                marks.setdefault("first_edit", snapshot(counts))
            else:
                add_call(counts, bucket, label)
    marks["end"] = snapshot(counts)
    reached = [name for name in ("first_edit", "answer") if name in marks]
    first = None
    for position, event in enumerate(body):  # whichever of edit and answer comes first
        if event[0] == "tool" and classify_tool(event[1], event[2])[0] == "edit":
            first = "first_edit"
            break
        if position == answer_at:
            first = "answer"
            break
    headline = first if first in reached else "end"
    return {"headline_stop": headline, "headline": marks[headline],
            "at": {name: marks.get(name) for name in STOPS}}


def count_file(path, agent="auto", prompt_index=0, include_sidechain=False):
    agent = detect_agent(path) if agent == "auto" else agent
    events = (claude_events if agent == "claude" else pi_events)(path, include_sidechain)
    turn = turn_events(events, prompt_index)
    if turn is None:
        raise ValueError(f"{path}: no prompt number {prompt_index}")
    prompt, body = turn
    result = count_turn(body)
    result.update({"path": str(path), "agent": agent,
                   "brief": any(event[0] == "brief" for event in body),
                   "prompt_sha": hashlib.sha256(prompt.encode()).hexdigest()[:12],
                   "prompt_chars": len(prompt), "events": len(body)})
    return result


# --------------------------------------------------------------------------
# Aggregation and report
# --------------------------------------------------------------------------

def expand(patterns):
    paths = []
    for pattern in patterns:
        if os.path.isdir(pattern):
            paths.extend(sorted(glob.glob(os.path.join(pattern, "**", "*.jsonl"), recursive=True)))
        else:
            paths.extend(sorted(glob.glob(pattern, recursive=True)) or [pattern])
    return list(dict.fromkeys(paths))


def summarize(results, key="native"):
    values = [r["headline"][key] for r in results]
    if not values:
        return {"n": 0, "mean": None, "median": None, "p90": None, "min": None, "max": None}
    ordered = sorted(values)
    p90 = ordered[max(0, -(-90 * len(ordered) // 100) - 1)]
    return {"n": len(values), "mean": round(statistics.mean(values), 2), "median": statistics.median(values),
            "p90": p90, "min": ordered[0], "max": ordered[-1]}


def paired(arms, first, second):
    """Per-prompt native means of two arms and their difference (second - first).

    Only prompts both arms ran count; a prompt run several times in an arm
    contributes the mean of its runs. ``lower`` counts the prompts where
    ``second`` explored less.
    """
    by = {name: {} for name in (first, second)}
    for name in (first, second):
        for result in arms[name]:
            by[name].setdefault(result["prompt_sha"], []).append(result["headline"]["native"])
    common = sorted(set(by[first]) & set(by[second]))
    deltas = [statistics.mean(by[second][p]) - statistics.mean(by[first][p]) for p in common]
    return {"prompts": len(common), "mean_delta": round(statistics.mean(deltas), 2) if deltas else None,
            "lower": sum(1 for d in deltas if d < 0), "equal": sum(1 for d in deltas if d == 0),
            "higher": sum(1 for d in deltas if d > 0)}


def fmt(value):
    return "n/a" if value is None else f"{value:g}"


def render(arms):
    lines = ["| arm | transcripts | brief seen | native mean | native median | native p90 | pixel mean | delegated mean |",
             "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"]
    for name, results in arms.items():
        native, pixel, delegated = (summarize(results, k) for k in ("native", "pixel", "delegated"))
        seen = sum(1 for r in results if r.get("brief"))
        lines.append(f"| {name} | {native['n']} | {seen} | {fmt(native['mean'])} | {fmt(native['median'])} | "
                     f"{fmt(native['p90'])} | {fmt(pixel['mean'])} | {fmt(delegated['mean'])} |")
    names = list(arms)
    if len(names) == 2:
        a, b = (summarize(arms[n]) for n in names)
        if a["mean"] is not None and b["mean"] is not None:
            lines.append("")
            lines.append(f"native mean `{names[1]}` - `{names[0]}`: {b['mean'] - a['mean']:+.2f} "
                         f"({names[0]} {a['mean']:g}, {names[1]} {b['mean']:g})")
        pair = paired(arms, names[0], names[1])
        if pair["prompts"]:
            lines.append(f"paired by prompt ({pair['prompts']} prompts): mean difference {pair['mean_delta']:+.2f}; "
                         f"`{names[1]}` explored less on {pair['lower']}, the same on {pair['equal']}, "
                         f"more on {pair['higher']}")
    return "\n".join(lines)


def run(args):
    arms = {}
    if args.arm:
        for spec in args.arm:
            name, _, pattern = spec.partition("=")
            if not pattern:
                raise SystemExit(f"--arm wants NAME=GLOB, got {spec!r}")
            arms.setdefault(name, []).extend(expand([pattern]))
    if args.paths:
        arms.setdefault("all", []).extend(expand(args.paths))
    if not arms:
        raise SystemExit("give transcript paths or --arm NAME=GLOB")
    required = {}
    for spec in args.require or []:
        name, _, want = spec.partition("=")
        if want not in ("brief", "no-brief"):
            raise SystemExit(f"--require wants NAME=brief or NAME=no-brief, got {spec!r}")
        required[name] = want == "brief"
    unknown = sorted(set(required) - set(arms))
    if unknown:  # a misspelt arm would leave its runs unchecked and the A/B unvalidated
        raise SystemExit(f"--require names no arm: {', '.join(unknown)} (arms: {', '.join(arms)})")
    counted, failures, dropped = {}, [], {}
    for name, paths in arms.items():
        counted[name] = []
        for path in paths:
            try:
                result = count_file(path, args.agent, args.prompt_index, args.include_sidechain)
            except (ValueError, OSError) as error:
                failures.append(str(error))
                continue
            if name in required and result["brief"] != required[name]:
                dropped.setdefault(name, []).append(str(path))  # the run is not what its arm says
                continue
            counted[name].append(result)
    if args.json:
        payload = {"arms": counted, "failures": failures, "dropped": dropped,
                   "summary": {name: summarize(results) for name, results in counted.items()}}
        Path(args.json).write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n")
    print(render(counted))
    if len(counted) == 1 and sum(len(v) for v in counted.values()) <= 20:
        print("")
        print("| transcript | agent | stop | native | pixel | delegated | by tool |")
        print("| --- | --- | --- | ---: | ---: | ---: | --- |")
        for result in next(iter(counted.values())):
            h = result["headline"]
            tools = ", ".join(f"{k} {v}" for k, v in h["by_tool"].items()) or "-"
            print(f"| {os.path.basename(result['path'])} | {result['agent']} | {result['headline_stop']} | "
                  f"{h['native']} | {h['pixel']} | {h['delegated']} | {tools} |")
    for name, paths in sorted(dropped.items()):
        print(f"\ndropped {len(paths)} `{name}` transcripts whose brief did not match --require", file=sys.stderr)
    for failure in failures:
        print(f"skipped: {failure}", file=sys.stderr)
    return 1 if failures and not any(counted.values()) else 0


# --------------------------------------------------------------------------
# Self-test
# --------------------------------------------------------------------------

def claude_line(kind, content, **extra):
    return json.dumps({"type": kind, "message": {"content": content}, **extra})


def tool_use(name, tool_input, ident):
    return {"type": "tool_use", "id": ident, "name": name, "input": tool_input}


class SelfTest(unittest.TestCase):
    def test_bash_commands_are_classified_by_the_first_command_of_each_pipeline(self):
        explore = ["rg -n foo crates", "grep -rn foo .", "find . -name '*.rs'", "cat Cargo.toml", "ls -la crates",
                   "sed -n '1,20p' src/main.rs", "cd crates && rg foo | head -5", "rtk grep foo src",
                   "rtk proxy rg foo src", "rtk read src/main.rs", "FOO=1 rg foo", "git grep foo",
                   "bash -c 'rg foo src'", "time rg foo", "head -20 README.md"]
        for command in explore:
            self.assertEqual(classify_bash(command), "explore", command)
        other = ["cargo test 2>&1 | grep FAIL", "git status", "npm install", "echo rg", "sed s/a/b/ file",
                 "cd crates", "bun run dev", "git log --oneline"]
        for command in other:
            self.assertEqual(classify_bash(command), "other", command)

    def test_timeout_options_and_duration_are_skipped_before_the_command(self):
        shapes = ["timeout 5 rg foo", "timeout -s KILL 5 rg foo", "timeout -sKILL 5 rg foo",
                  "timeout --signal=KILL 5 rg foo", "timeout --signal KILL 5 rg foo", "timeout -k 2 5 rg foo",
                  "timeout --kill-after=2 5 rg foo", "timeout --kill-after 2 5 rg foo",
                  "timeout --foreground 5 rg foo", "timeout --preserve-status 5 rg foo", "timeout -v 5 rg foo",
                  "timeout -k 2 -s KILL --foreground --preserve-status -v 10.5s rg foo", "timeout -vs KILL 5 rg foo",
                  "timeout -- 5 rg foo", "gtimeout -s KILL 5 rg foo", "rtk timeout -s KILL 5 rg foo",
                  "timeout -s KILL 5 rg foo | head -3"]
        for command in shapes:
            self.assertEqual(classify_bash(command), "explore", command)
        self.assertEqual(strip_wrappers(shlex.split("timeout -s KILL 5 rg foo")), ["rg", "foo"])
        self.assertEqual(strip_wrappers(shlex.split("timeout -k 2 -s KILL 5 rg foo")), ["rg", "foo"])
        self.assertEqual(strip_wrappers(shlex.split("timeout --signal=KILL 5 rg foo")), ["rg", "foo"])
        for command in ["timeout -s KILL 5 cargo test", "timeout 5", "timeout -s KILL", "timeout -s KILL 5 git status"]:
            self.assertEqual(classify_bash(command), "other", command)
        self.assertEqual(classify_bash("timeout -s KILL 5 pixel find-code x"), "pixel")
        self.assertEqual(classify_bash("timeout -s KILL 5 sed -i s/a/b/ f.rs"), "edit")
        self.assertEqual(classify_bash("printf x | timeout -s KILL 5 tee out.txt"), "edit")

    def test_the_other_wrappers_skip_their_options_too(self):
        explore = ["env -i rg foo", "env -u HOME rg foo", "env -C /tmp rg foo", "env -i FOO=1 rg foo", "env FOO=1 BAR=2 rg foo",
                   "nice rg foo", "nice -n 10 rg foo", "nice -10 rg foo", "nice -n10 rg foo", "nice --adjustment=5 rg foo",
                   "command rg foo", "command -p rg foo", "time rg foo", "time -p rg foo", "time -f %e rg foo",
                   "time -o /tmp/t rg foo", "time --format=%e rg foo", "exec rg foo", "exec -a name rg foo", "exec -c rg foo",
                   "sudo rg foo", "sudo -n rg foo", "sudo -u bob rg foo", "sudo -E -u bob rg foo", "sudo --user=bob rg foo",
                   "nohup rg foo", "stdbuf -oL rg foo", "stdbuf -o L rg foo", "stdbuf --output=L rg foo", "stdbuf -i0 -oL -eL rg foo",
                   "xargs rg foo", "xargs -n1 rg foo", "xargs -n 1 rg foo", "xargs -I {} cat {}", "xargs -0 -P 4 -n 1 rg foo",
                   "xargs --max-args=1 rg foo",
                   "timeout 5 nice -n 10 env -i FOO=1 rg foo", "rtk proxy timeout -s KILL 5 nice -n 10 rg foo",
                   "time -p timeout -s KILL 5 stdbuf -oL rg foo"]
        for command in explore:
            self.assertEqual(classify_bash(command), "explore", command)
        other = ["command -v rg", "command -V rg", "env", "env -i", "nice -n 10 cargo build", "sudo -u bob cargo build",
                 "time -p cargo test", "xargs -n1 echo", "exec"]
        for command in other:
            self.assertEqual(classify_bash(command), "other", command)

    def test_shell_dash_c_with_clustered_flags_is_unwrapped(self):
        for command in ["bash -c 'rg foo'", "bash -lc 'rg foo'", "zsh -ic 'rg foo'", "sh -c 'timeout -s KILL 5 rg foo'",
                        "bash -lc \"cd src && rg foo\""]:
            self.assertEqual(classify_bash(command), "explore", command)
        for command in ["bash -lc 'cargo test'", "bash script.sh", "bash -c", "bash -x script.sh"]:
            self.assertEqual(classify_bash(command), "other", command)

    def test_pixel_calls_are_their_own_bucket_and_writes_are_edits(self):
        self.assertEqual(classify_bash("pixel find-code 'x' | head"), "pixel")
        self.assertEqual(classify_bash("rtk pixel search-content -F foo"), "pixel")
        for command in ["sed -i '' s/a/b/ f.rs", "cat > notes.md <<EOF\nhi\nEOF", "echo x >> out.txt", "git apply fix.patch",
                        "printf x | tee out.txt"]:
            self.assertEqual(classify_bash(command), "edit", command)
        self.assertEqual(classify_bash("rg foo 2>/dev/null"), "explore")
        self.assertEqual(classify_bash("rg foo > /dev/null"), "explore")

    def test_heredoc_bodies_are_not_shell(self):
        self.assertEqual(classify_bash("python3 - <<'EOF'\nprint(a > b)\nEOF"), "other")
        self.assertEqual(classify_bash("cat <<EOF > out.txt\nhello\nEOF"), "edit")
        self.assertEqual(classify_bash("rg foo <<EOF\nx > y\nEOF"), "explore")

    def test_an_unparseable_command_does_not_crash(self):
        self.assertEqual(classify_bash("rg 'unterminated"), "explore")
        self.assertEqual(classify_bash(""), "other")

    def test_tool_buckets(self):
        self.assertEqual(classify_tool("Grep", {})[0], "explore")
        self.assertEqual(classify_tool("read", {"path": "a"})[0], "explore")
        self.assertEqual(classify_tool("Edit", {})[0], "edit")
        self.assertEqual(classify_tool("Agent", {})[0], "delegate")
        self.assertEqual(classify_tool("WebFetch", {})[0], "other")
        self.assertEqual(classify_tool("Bash", {"command": "rg x"})[0], "explore")
        self.assertEqual(classify_tool("bash", None)[0], "other")

    def write(self, directory, name, lines):
        path = Path(directory) / name
        path.write_text("\n".join(lines) + "\n")
        return path

    def test_claude_transcript_counts_until_the_first_edit(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = self.write(tmp, "c.jsonl", [
                json.dumps({"type": "summary", "summary": "x"}),
                claude_line("user", "where is the gate"),
                claude_line("assistant", [{"type": "text", "text": "Let me look."}, tool_use("Grep", {"pattern": "gate"}, "1")]),
                claude_line("user", [{"type": "tool_result", "content": "hit"}]),
                claude_line("assistant", [tool_use("Read", {"file_path": "a.rs"}, "2")]),
                claude_line("assistant", [tool_use("Bash", {"command": "rtk proxy rg gate src"}, "3")]),
                claude_line("assistant", [tool_use("Bash", {"command": "pixel find-code gate"}, "4")]),
                claude_line("assistant", [tool_use("Agent", {"prompt": "x"}, "5")]),
                claude_line("assistant", [tool_use("Edit", {"file_path": "a.rs"}, "6")]),
                claude_line("assistant", [tool_use("Read", {"file_path": "b.rs"}, "7")]),
                claude_line("assistant", [{"type": "text", "text": "Done."}]),
                claude_line("user", "second prompt"),
                claude_line("assistant", [tool_use("Glob", {"pattern": "*.rs"}, "8")]),
            ])
            result = count_file(path)
            self.assertEqual(result["agent"], "claude")
            self.assertEqual(result["headline_stop"], "first_edit")
            self.assertEqual(result["headline"]["native"], 3)
            self.assertEqual(result["headline"]["by_tool"], {"Bash": 1, "Grep": 1, "Read": 1})
            self.assertEqual((result["headline"]["pixel"], result["headline"]["delegated"]), (1, 1))
            self.assertEqual(result["at"]["first_text"]["native"], 0)
            self.assertEqual(result["at"]["answer"]["native"], 4)
            self.assertEqual(result["at"]["end"]["native"], 4)
            second = count_file(path, prompt_index=1)
            self.assertEqual(second["headline"]["native"], 1)
            with self.assertRaises(ValueError):
                count_file(path, prompt_index=2)

    def test_the_injected_brief_is_seen_in_claude_and_pi_transcripts(self):
        with tempfile.TemporaryDirectory() as tmp:
            attach = lambda content, kind="hook_additional_context": json.dumps(  # noqa: E731
                {"type": "attachment", "attachment": {"type": kind, "content": content, "hookEvent": "UserPromptSubmit"}})
            with_brief = self.write(tmp, "b.jsonl", [claude_line("user", "q"), attach("[PIXEL:BRIEF]\nkind: lookup"),
                                                     claude_line("assistant", [{"type": "text", "text": "a"}])])
            other_hook = self.write(tmp, "o.jsonl", [claude_line("user", "q"), attach("session start notes"),
                                                     attach("[PIXEL:BRIEF]", kind="hook_success"),
                                                     claude_line("assistant", [{"type": "text", "text": "a"}])])
            pi_brief = self.write(tmp, "p.jsonl", [
                json.dumps({"type": "session", "id": "s"}),
                json.dumps({"type": "message", "message": {"role": "user", "content": "q"}}),
                json.dumps({"type": "custom_message", "customType": "pixel-brief", "content": "[PIXEL:BRIEF]"}),
                json.dumps({"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "a"}]}})])
            self.assertTrue(count_file(with_brief)["brief"])
            self.assertFalse(count_file(other_hook)["brief"])
            self.assertTrue(count_file(pi_brief)["brief"])

    def test_require_drops_the_runs_that_are_not_what_their_arm_says(self):
        with tempfile.TemporaryDirectory() as tmp:
            good = self.write(tmp, "on-1.jsonl", [claude_line("user", "q"), json.dumps({"type": "attachment", "attachment": {
                "type": "hook_additional_context", "content": "[PIXEL:BRIEF]"}}), claude_line("assistant", [{"type": "text", "text": "a"}])])
            bad = self.write(tmp, "on-2.jsonl", [claude_line("user", "q"), claude_line("assistant", [{"type": "text", "text": "a"}])])
            args = argparse.Namespace(paths=[], arm=[f"on={good}", f"on={bad}"], require=["on=brief"], agent="auto",
                                      prompt_index=0, include_sidechain=False, json=str(Path(tmp) / "out.json"))
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                run(args)
            payload = json.loads((Path(tmp) / "out.json").read_text())
            self.assertEqual(len(payload["arms"]["on"]), 1)
            self.assertEqual(payload["dropped"], {"on": [str(bad)]})
            args.require = ["onn=brief"]
            with self.assertRaises(SystemExit) as refused:
                run(args)
            self.assertIn("--require names no arm: onn", str(refused.exception))

    def test_the_answer_is_the_closing_text_and_ends_a_question_turn(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = self.write(tmp, "q.jsonl", [
                claude_line("user", "how does it work"),
                claude_line("assistant", [{"type": "text", "text": "Searching."}, tool_use("Grep", {"pattern": "x"}, "1")]),
                claude_line("assistant", [tool_use("Read", {"file_path": "a.rs"}, "2")]),
                claude_line("assistant", [{"type": "text", "text": "It works like this."}]),
            ])
            result = count_file(path)
            self.assertEqual(result["headline_stop"], "answer")
            self.assertEqual(result["headline"]["native"], 2)
            self.assertEqual(result["at"]["first_text"]["native"], 0)

    def test_a_turn_with_no_answer_or_edit_counts_to_its_end(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = self.write(tmp, "e.jsonl", [
                claude_line("user", "go"),
                claude_line("assistant", [tool_use("Read", {"file_path": "a.rs"}, "1")]),
            ])
            result = count_file(path)
            self.assertEqual((result["headline_stop"], result["headline"]["native"]), ("end", 1))

    def test_sidechain_meta_and_command_records_are_not_prompts_or_calls(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = self.write(tmp, "s.jsonl", [
                claude_line("user", "<command-name>/clear</command-name>"),
                claude_line("user", "meta note", isMeta=True),
                claude_line("user", "the real prompt"),
                claude_line("assistant", [tool_use("Grep", {"pattern": "x"}, "1")], isSidechain=True),
                claude_line("assistant", [tool_use("Read", {"file_path": "a"}, "2")]),
                claude_line("assistant", [tool_use("Read", {"file_path": "a"}, "2")]),
            ])
            self.assertEqual(count_file(path)["headline"]["native"], 1)
            self.assertEqual(count_file(path, include_sidechain=True)["headline"]["native"], 2)

    def test_pi_transcript(self):
        with tempfile.TemporaryDirectory() as tmp:
            def message(role, content):
                return json.dumps({"type": "message", "message": {"role": role, "content": content}})
            path = self.write(tmp, "p.jsonl", [
                json.dumps({"type": "session", "id": "s", "cwd": "/x"}),
                message("user", [{"type": "text", "text": "find the gate"}]),
                message("assistant", [{"type": "thinking", "thinking": "hmm"},
                                      {"type": "toolCall", "id": "a", "name": "bash", "arguments": {"command": "rg gate"}},
                                      {"type": "toolCall", "id": "b", "name": "read", "arguments": {"path": "a.rs"}}]),
                message("toolResult", [{"type": "text", "text": "out"}]),
                message("assistant", [{"type": "toolCall", "id": "c", "name": "edit", "arguments": {"path": "a.rs", "edits": []}}]),
            ])
            result = count_file(path)
            self.assertEqual(result["agent"], "pi")
            self.assertEqual((result["headline_stop"], result["headline"]["native"]), ("first_edit", 2))

    def test_an_unknown_file_is_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = self.write(tmp, "n.jsonl", [json.dumps({"type": "other"})])
            with self.assertRaises(ValueError):
                count_file(path)

    def test_arms_summary_and_delta(self):
        def fake(native, prompt="p"):
            return {"headline": {"native": native, "pixel": 0, "delegated": 0, "by_tool": {}},
                    "headline_stop": "end", "path": "x", "agent": "claude", "prompt_sha": prompt}
        arms = {"off": [fake(10), fake(20), fake(30)], "on": [fake(2), fake(4)]}
        self.assertEqual(summarize(arms["off"])["median"], 20)
        self.assertEqual(summarize(arms["off"])["p90"], 30)
        self.assertEqual(summarize([])["mean"], None)
        self.assertIn("-17.00", render(arms))

    def test_pairing_is_by_prompt_and_ignores_prompts_one_arm_lacks(self):
        def fake(native, prompt):
            return {"headline": {"native": native}, "prompt_sha": prompt}
        arms = {"off": [fake(10, "a"), fake(20, "a"), fake(8, "b"), fake(5, "c")],
                "on": [fake(6, "a"), fake(9, "b"), fake(1, "d")]}
        pair = paired(arms, "off", "on")
        self.assertEqual(pair, {"prompts": 2, "mean_delta": -4.0, "lower": 1, "equal": 0, "higher": 1})


def self_test():
    outcome = unittest.TextTestRunner(verbosity=1).run(unittest.defaultTestLoader.loadTestsFromTestCase(SelfTest))
    return 0 if outcome.wasSuccessful() else 1


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("paths", nargs="*", help="transcript files, directories or globs")
    parser.add_argument("--arm", action="append", metavar="NAME=GLOB", help="a named group of transcripts (repeatable)")
    parser.add_argument("--agent", choices=("auto", "claude", "pi"), default="auto")
    parser.add_argument("--prompt-index", type=int, default=0, help="which user prompt to measure (0 = the first)")
    parser.add_argument("--require", action="append", metavar="NAME=brief|no-brief",
                        help="drop an arm's transcripts that do (no-brief) or do not (brief) carry the injected brief")
    parser.add_argument("--include-sidechain", action="store_true", help="count subagent records stored in the file")
    parser.add_argument("--json", help="write per-transcript counts and arm summaries here")
    parser.add_argument("--self-test", action="store_true", help="run the parser tests and exit")
    args = parser.parse_args(argv)
    return self_test() if args.self_test else run(args)


if __name__ == "__main__":
    sys.exit(main())
