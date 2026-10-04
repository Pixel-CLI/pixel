# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""How agents actually retrieve in Pixel-indexed repositories, from their transcripts.

Usage: python3 adherence.py <window> [--projects DIR]... [--codex DIR] [--all-repos] [--json]
       (window: 12h, 7d, 3w, or an ISO date, as for lead_time.py)

The controlled A/B (eval/, #626) says where Pixel should win; this says what
agents do in real sessions, so a change to hooks, prompts or routing can be
judged by the behaviour it moves. Each session is reduced to one ordered
stream of retrieval events, from Claude Code tool calls and from the shell
commands Codex runs:

- `pixel`: a Pixel retrieval call (search-content, find-code, impact, ...);
- `search`: a native search (Grep/Glob, rg, grep, ag, ack, git grep, find -name);
- `read`: a file read, with its width in lines (`None` when unbounded:
  Read without `limit`, `cat`, `nl`; `head`/`tail` default to 10 lines);
- `edit`: Edit/Write/NotebookEdit/MultiEdit or Codex `apply_patch`.

Per host (never pooled) it prints: the Pixel share of searches, how often a
session's first search was Pixel, how often a Pixel call was immediately
followed by a native search as the very next event, unbounded and wide reads overall and right after a Pixel call, and
how many editing sessions ran `impact`/`who-calls`/`call-path` before their
first edit.

Only sessions whose working directory belongs to a repository with a
`.pixel/` index are counted, unless `--all-repos`. Claude transcripts are
read from `~/.claude/projects/*/*.jsonl` (top-level sessions; subagent time
is the parent's), Codex rollouts from `~/.codex/sessions/**/rollout-*.jsonl`.
Only counts are printed; no prompt, command or file content.
"""
import datetime
import json
import pathlib
import re
import shlex
import statistics
import sys

UTC = datetime.timezone.utc
EDIT_TOOLS = {"Edit", "Write", "NotebookEdit", "MultiEdit"}
SEARCH_TOOLS = {"Grep", "Glob"}
PIXEL_RETRIEVAL = {
    "search-content", "find-code", "find-symbol", "search-meaning", "impact", "who-calls",
    "call-path", "pack-context", "scope-task", "execution-brief", "list-areas", "what-changed",
    "review-changes", "search-like-rg",
}
STRUCTURAL = {"impact", "who-calls", "call-path"}
NATIVE_SEARCH = {"rg", "grep", "egrep", "fgrep", "ag", "ack"}
WRAPPERS = {"rtk", "time", "nice", "command", "builtin", "exec", "env"}
WIDE_READ = 150
CODEX_CMD = re.compile(r'["\']?\bcmd["\']?\s*:\s*"((?:[^"\\]|\\.)*)"')
CODEX_TOOL = re.compile(r"tools\.(exec_command|apply_patch)\(")
SEDP = re.compile(r"^(\d+)(?:,(\+?)(\d+|\$))?p$")


def parse_window(arg, now):
    m = re.fullmatch(r"(\d+)([hdw])", arg)
    if m:
        hours = {"h": 1, "d": 24, "w": 168}[m.group(2)] * int(m.group(1))
        return now - datetime.timedelta(hours=hours)
    start = datetime.datetime.fromisoformat(arg)
    return start if start.tzinfo else start.replace(tzinfo=UTC)


def parse_ts(value):
    try:
        return datetime.datetime.fromisoformat(str(value).replace("Z", "+00:00"))
    except ValueError:
        return None


def first_stages(command):
    """The first pipeline stage of each statement, split on `;`, `&&`, `||`,
    `|` and newlines outside quotes; here-document bodies are skipped, so
    text that is only quoted or fed to a command is never read as one."""
    stages, current, quote, starts = [], [], None, True
    heredoc, pending = None, None
    i, n = 0, len(command)
    while i < n:
        c = command[i]
        if heredoc is not None:
            end = command.find("\n", i)
            line = command[i:] if end < 0 else command[i:end]
            if line.strip() == heredoc:
                heredoc = None
            i = n if end < 0 else end + 1
            continue
        if quote:
            if c == "\\" and quote == '"' and i + 1 < n:
                current.append(command[i:i + 2])
                i += 2
                continue
            if c == quote:
                quote = None
            current.append(c)
            i += 1
            continue
        if c in "'\"":
            quote = c
            current.append(c)
            i += 1
            continue
        if c == "\\" and i + 1 < n:
            current.append(command[i:i + 2])
            i += 2
            continue
        if command.startswith("<<", i):
            m = re.match(r"<<-?\s*(['\"]?)([A-Za-z0-9_]+)\1", command[i:])
            if m:
                pending = m.group(2)
                current.append(m.group(0))
                i += len(m.group(0))
                continue
        two = command[i:i + 2]
        separator = two if two in ("&&", "||") else (c if c in ";|\n" else None)
        if separator:
            if starts:
                stages.append("".join(current))
            current = []
            starts = separator != "|"
            i += len(separator)
            if separator == "\n" and pending:
                heredoc, pending = pending, None
            continue
        current.append(c)
        i += 1
    if starts:
        stages.append("".join(current))
    return [stage.strip() for stage in stages if stage.strip()]


def statements(command):
    """Each statement's first pipeline stage, as argv without wrappers."""
    out = []
    for stage in first_stages(command):
        try:
            words = shlex.split(stage)
        except ValueError:
            words = stage.split()
        while words and (re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*=.*", words[0]) or words[0] in WRAPPERS
                         or words[0] == "proxy"):
            words = words[1:]
        if words and words[0] == "timeout" and len(words) > 1:
            words = words[2:]
        if words:
            out.append(words)
    return out


def sed_width(args):
    if "-i" in args or not any(a in ("-n", "--quiet", "--silent") for a in args):
        return False, None
    for arg in args:
        m = SEDP.match(arg)
        if m:
            start, plus, end = int(m.group(1)), m.group(2), m.group(3)
            if end is None:
                return True, 1
            if end == "$":
                return True, None
            return True, (int(end) + 1 if plus else max(int(end) - start + 1, 1))
    return False, None


def head_width(program, args):
    """Lines a `head`/`tail` prints; `None` for `head -n -N` (all but the last
    N) and `tail -n +N` (from line N to the end), which are unbounded."""
    count, skip = None, False
    for i, arg in enumerate(args):
        if skip:
            skip = False
            continue
        if arg in ("-n", "--lines") and i + 1 < len(args):
            count, skip = args[i + 1], True
        elif arg.startswith("--lines="):
            count = arg.split("=", 1)[1]
        elif re.fullmatch(r"-n[+-]?\d+", arg):
            count = arg[2:]
        elif re.fullmatch(r"-\d+", arg):
            count = arg[1:]
    if count is None:
        return 10
    if (program == "head" and count.startswith("-")) or (program == "tail" and count.startswith("+")):
        return None
    digits = count.lstrip("+-")
    return int(digits) if digits.isdigit() else None


def shell_events(command):
    """Retrieval events of one shell command, in order."""
    events = []
    for words in statements(command):
        program = words[0].rsplit("/", 1)[-1]
        args = words[1:]
        # Operands: not an option or its value, not a redirection or its target.
        operands = [a for i, a in enumerate(args)
                    if not a.startswith(("-", "<", ">"))
                    and not (i and args[i - 1] in ("-n", "--lines", "-c", ">", ">>", "<", "2>", "&>"))]
        if program in ("pixel", "pixel-dev") and args and args[0] in PIXEL_RETRIEVAL:
            events.append(("pixel", args[0]))
        elif program in NATIVE_SEARCH or (program == "git" and args[:1] == ["grep"]):
            events.append(("search", program))
        elif program == "find" and any(a in ("-name", "-iname", "-path", "-regex") for a in args):
            events.append(("search", "find"))
        elif program in ("cat", "nl", "bat", "less") and operands:
            events.append(("read", None))
        elif program in ("head", "tail") and operands:
            events.append(("read", head_width(program, args)))
        elif program == "sed":
            is_read, width = sed_width(args)
            if is_read:
                events.append(("read", width))
        elif program == "apply_patch":
            events.append(("edit", None))
    return events


def claude_events(tool, inp):
    if tool in SEARCH_TOOLS:
        return [("search", tool)]
    if tool in EDIT_TOOLS:
        return [("edit", None)]
    if tool == "Read":
        limit = inp.get("limit")
        return [("read", int(limit) if isinstance(limit, (int, float)) and limit > 0 else None)]
    if tool == "Bash":
        return shell_events(str(inp.get("command", "")))
    if tool.startswith("mcp__") and "pixel" in tool.lower():
        operation = tool.rsplit("__", 1)[-1].replace("_", "-")
        return [("pixel", operation)] if operation in PIXEL_RETRIEVAL else []
    return []


def read_claude(path, start):
    """(cwd, events, active) of one Claude Code session file: events after
    `start`, and whether the agent acted at all in the window."""
    cwd, events, active = None, [], False
    with path.open(encoding="utf-8", errors="replace") as handle:
        lines = handle.readlines()
    for line in lines:
        try:
            entry = json.loads(line)
        except ValueError:
            continue
        cwd = cwd or entry.get("cwd")
        if entry.get("type") != "assistant":
            continue
        ts = parse_ts(entry.get("timestamp", ""))
        if ts is None or ts < start:
            continue
        active = True
        content = (entry.get("message") or {}).get("content")
        for part in content if isinstance(content, list) else []:
            if part.get("type") == "tool_use":
                events += claude_events(part.get("name", ""), part.get("input") or {})
    return cwd, events, active


def call_arguments(script, start):
    """The argument text of the call whose `(` ends at `start`, up to its
    matching `)`, skipping JavaScript string contents."""
    depth, quote, i = 1, None, start
    while i < len(script) and depth:
        c = script[i]
        if quote:
            if c == "\\":
                i += 1
            elif c == quote:
                quote = None
        elif c in "'\"`":
            quote = c
        elif c == "(":
            depth += 1
        elif c == ")":
            depth -= 1
        i += 1
    return script[start:i - 1]


def exec_script_events(script):
    """Events of a Codex `exec` script, in call order: each
    `tools.exec_command({cmd: ...})` and each `tools.apply_patch(...)`, each
    command read from its own call's arguments."""
    events = []
    for call in CODEX_TOOL.finditer(script):
        if call.group(1) == "apply_patch":
            events.append(("edit", None))
            continue
        cmd = CODEX_CMD.search(call_arguments(script, call.end()))
        if cmd:
            events += shell_events(json.loads(f'"{cmd.group(1)}"'))
    return events


def read_codex(path, start):
    """(cwd, events, active) of one Codex rollout, as for `read_claude`."""
    cwd, events, active = None, [], False
    with path.open(encoding="utf-8", errors="replace") as handle:
        lines = handle.readlines()
    for line in lines:
        try:
            entry = json.loads(line)
        except ValueError:
            continue
        payload = entry.get("payload")
        if not isinstance(payload, dict):
            continue
        cwd = cwd or payload.get("cwd")
        ts = parse_ts(entry.get("timestamp", ""))
        if entry.get("type") != "response_item" or ts is None or ts < start:
            continue
        active = True
        kind, name = payload.get("type"), payload.get("name", "")
        if kind == "custom_tool_call" and name == "apply_patch":
            events.append(("edit", None))
        elif kind == "custom_tool_call":
            events += exec_script_events(str(payload.get("input", "")))
        elif kind == "function_call" and name in ("shell", "exec_command", "local_shell"):
            try:
                args = json.loads(payload.get("arguments") or "{}")
            except ValueError:
                continue
            cmd = args.get("cmd") or args.get("command") or ""
            if isinstance(cmd, list):
                cmd = cmd[-1] if cmd[:2] in (["bash", "-lc"], ["sh", "-c"]) else shlex.join(cmd)
            events += shell_events(str(cmd))
        elif kind == "function_call" and name == "apply_patch":
            events.append(("edit", None))
    return cwd, events, active


def indexed(cwd):
    if not cwd:
        return False
    path = pathlib.Path(cwd)
    for candidate in (path, *path.parents):
        if (candidate / ".pixel").is_dir():
            return True
        if (candidate / ".git").exists():
            return False
    return False


def summarize(sessions):
    """Adherence counts over (events) lists of one host."""
    s = {"sessions": len(sessions), "pixel_calls": 0, "native_searches": 0, "sessions_with_search": 0,
         "sessions_with_pixel": 0, "first_search_pixel": 0, "pixel_then_native": 0, "reads": 0, "unbounded_reads": 0, "wide_reads": 0,
         "reads_after_pixel": 0, "unbounded_after_pixel": 0, "wide_after_pixel": 0,
         "edit_sessions": 0, "impact_before_edit": 0}
    widths_after_pixel = []
    for events in sessions:
        searches = [e for e in events if e[0] in ("pixel", "search")]
        s["pixel_calls"] += sum(e[0] == "pixel" for e in events)
        s["native_searches"] += sum(e[0] == "search" for e in events)
        if searches:
            s["sessions_with_search"] += 1
            s["first_search_pixel"] += searches[0][0] == "pixel"
        s["sessions_with_pixel"] += any(e[0] == "pixel" for e in events)
        # "Right after Pixel" means the very next event: nothing in between.
        for previous, (kind, detail) in zip([None, *events], events):
            after_pixel = previous is not None and previous[0] == "pixel"
            if kind == "search":
                s["pixel_then_native"] += after_pixel
            elif kind == "read":
                unbounded, wide = detail is None, detail is not None and detail > WIDE_READ
                s["reads"] += 1
                s["unbounded_reads"] += unbounded
                s["wide_reads"] += wide
                if after_pixel:
                    s["reads_after_pixel"] += 1
                    s["unbounded_after_pixel"] += unbounded
                    s["wide_after_pixel"] += wide
                    if detail is not None:
                        widths_after_pixel.append(detail)
        edits = [i for i, e in enumerate(events) if e[0] == "edit"]
        if edits:
            s["edit_sessions"] += 1
            s["impact_before_edit"] += any(
                e[0] == "pixel" and e[1] in STRUCTURAL for e in events[: edits[0]])
    searches_total = s["pixel_calls"] + s["native_searches"]
    s["pixel_share"] = round(s["pixel_calls"] / searches_total, 3) if searches_total else None
    s["median_read_width_after_pixel"] = statistics.median(widths_after_pixel) if widths_after_pixel else None
    return s


def collect(start, projects, codex_dir, all_repos):
    hosts = {"claude": [], "codex": []}
    for directory in projects:
        for path in sorted(directory.glob("*/*.jsonl")):
            if datetime.datetime.fromtimestamp(path.stat().st_mtime, UTC) < start:
                continue
            cwd, events, active = read_claude(path, start)
            if active and (all_repos or indexed(cwd)):
                hosts["claude"].append(events)
    if codex_dir and codex_dir.is_dir():
        for path in sorted(codex_dir.glob("**/rollout-*.jsonl")):
            if datetime.datetime.fromtimestamp(path.stat().st_mtime, UTC) < start:
                continue
            cwd, events, active = read_codex(path, start)
            if active and (all_repos or indexed(cwd)):
                hosts["codex"].append(events)
    return {host: summarize(sessions) for host, sessions in hosts.items()}


def pct(numerator, denominator):
    return f"{numerator}/{denominator} ({100 * numerator / denominator:.0f}%)" if denominator else "n/a"


def render(window, result):
    lines = [f"Pixel adherence since {window} (sessions in indexed repositories; hosts never pooled)", ""]
    for host, s in result.items():
        lines += [
            f"{host}: {s['sessions']} sessions",
            f"  searches: {s['pixel_calls']} pixel, {s['native_searches']} native"
            f" (pixel share {s['pixel_share'] if s['pixel_share'] is not None else 'n/a'})",
            f"  first search was pixel: {pct(s['first_search_pixel'], s['sessions_with_search'])}",
            f"  sessions that called pixel: {pct(s['sessions_with_pixel'], s['sessions'])}",
            f"  pixel calls whose next event is a native search: {pct(s['pixel_then_native'], s['pixel_calls'])}",
            f"  unbounded reads: {pct(s['unbounded_reads'], s['reads'])};"
            f" wider than {WIDE_READ} lines: {pct(s['wide_reads'], s['reads'])}",
            f"  reads right after pixel: {s['reads_after_pixel']}, unbounded"
            f" {pct(s['unbounded_after_pixel'], s['reads_after_pixel'])}, median width"
            f" {s['median_read_width_after_pixel'] if s['median_read_width_after_pixel'] is not None else 'n/a'}",
            f"  editing sessions with impact/who-calls before the first edit:"
            f" {pct(s['impact_before_edit'], s['edit_sessions'])}",
            "",
        ]
    return "\n".join(lines)


def main(argv):
    if not argv or argv[0] in ("-h", "--help"):
        print(__doc__)
        return 0 if argv else 2
    now = datetime.datetime.now(UTC)
    start = parse_window(argv[0], now)
    projects, codex_dir = [], pathlib.Path.home() / ".codex" / "sessions"
    rest = argv[1:]
    while rest:
        flag = rest.pop(0)
        if flag == "--projects" and rest:
            projects.append(pathlib.Path(rest.pop(0)).expanduser())
        elif flag == "--codex" and rest:
            codex_dir = pathlib.Path(rest.pop(0)).expanduser()
    projects = projects or [pathlib.Path.home() / ".claude" / "projects"]
    result = collect(start, projects, codex_dir, "--all-repos" in argv)
    if "--json" in argv:
        print(json.dumps({"since": start.isoformat(), "hosts": result}, indent=2))
    else:
        print(render(argv[0], result))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
