# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Lead time of the pixel repository's own pull requests, from agent transcripts.

Usage: python3 lead_time.py <window> [--projects DIR]... [--codex DIR]... [--no-gh] [--json]
       (window: 12h, 7d, 3w, or an ISO date, as for roots.py)

CONTRIBUTING.md ("Agent validation workflow") asks that the development loop
be measured from the first edit to a fully validated pull request, CI queue
and fix/push cycles included, not by agent activity alone. This script does
that for every pull request an agent opened in the window:

- A unit is one `gh pr create` whose result names `…/pull/<N>`. It starts
  at the first Edit/Write/NotebookEdit call after the previous unit of the
  same session (or the session start), so each PR owns its own edits.
- It ends when the PR is validated: the last check completion on the PR's
  final head commit (`gh pr view <N> --json headRefOid,statusCheckRollup`).
  Follow-up pushes from later sessions count; that is the real cost. With
  `--no-gh`, or when a check is still running, the end is unknown and only
  the edit-to-open part is reported.
- Inside the unit's own session, every gap between two events is charged to
  what came next: a tool result (a foreground tool), an assistant turn
  (the model), a genuine user prompt (waiting for the human), or a
  task notification (idle on a background task). The same gaps, keyed by
  the Bash command that preceded them, give the most blocking commands.

A PR belongs to this repository by its URL, never by where the session
ran: an agent in a sibling checkout opens PRs here too, and one started in
`pixel-mvp` must not lend its own `#33` to this repository's `#33`. So
every top-level session written to in the window is read, Claude Code's
from `~/.claude/projects/*/` and Codex rollouts from `~/.codex/sessions/**`
and `~/.codex/archived_sessions/` (where a finished one is moved). Only
`gh pr create` results naming the `origin` slug count, and `gh pr view` is
asked about that slug. Subagent transcripts are read for the PRs they open;
the parent's time while it waits on one stays the parent's idle. Only pixel commands, PR numbers and durations are
printed; no prompt or file content.
"""
import collections, datetime, json, pathlib, re, statistics, subprocess, sys

sys.path.insert(0, str(pathlib.Path(__file__).parent))
import adherence  # noqa: E402  (its Codex `exec` script parsing)

UTC = datetime.timezone.utc
EDIT_TOOLS = {"Edit", "Write", "NotebookEdit", "MultiEdit"}
PR_URL = re.compile(r"github\.com/([^/\s]+/[^/\s]+)/pull/(\d+)")
# `gh pr create` where a shell runs it (a line, `&&`, `;`, `|`, a subshell),
# not quoted inside another command (`grep 'gh pr create'`).
PR_CREATE = re.compile(r"(?:^|[;&|(\n])\s*(?:[A-Za-z_][A-Za-z0-9_]*=\S*\s+)*gh\s+pr\s+create\b")
# A Codex command still running when its call returned: its later output
# comes back through `write_stdin` polls naming the same session.
CODEX_SESSION = re.compile(r'\\?"session_id\\?"\s*:\s*(\d+)')
CODEX_POLL = re.compile(r"write_stdin\(\{\s*session_id\s*:\s*(\d+)")
GIT_PUSH = re.compile(r"\bgit\s+push\b")
# Injected user-role text that no human typed.
NOT_HUMAN = re.compile(r"^\s*<(task-notification|system-reminder|local-command|command-name|command-message|user-prompt-submit-hook)")
# Codex injects its context, automation heartbeats and environment as user
# messages; each opens on a tag or on the AGENTS.md header.
CODEX_NOT_HUMAN = re.compile(r"^\s*(<[A-Za-z_-]+[ >]|# AGENTS\.md instructions)")
# Codex calls that only wait on something already running.
CODEX_WAITS = {"wait", "sleep"}
# Codex edits files from the shell as often as with apply_patch: an in-place
# sed or perl, a heredoc script writing a file, a redirection into a source.
EDIT_COMMAND = re.compile(r"\bsed\s+-i|\bperl\s+-[a-z]*i|\.write_text\(|\bopen\([^)]*,\s*['\"][wa]|"
                          r"(?<![0-9&>])>{1,2}\s*\S+\.(?:rs|py|md|toml|sh|ts|js|json|ya?ml)\b")
BUCKETS = ("tool", "model", "idle", "human")


def parse_window(arg, now):
    m = re.fullmatch(r"(\d+)([hdw])", arg)
    if m:
        hours = {"h": 1, "d": 24, "w": 168}[m.group(2)] * int(m.group(1))
        return now - datetime.timedelta(hours=hours)
    start = datetime.datetime.fromisoformat(arg)
    return start if start.tzinfo else start.replace(tzinfo=UTC)


def parse_ts(value):
    return datetime.datetime.fromisoformat(value.replace("Z", "+00:00"))


def blocks(message):
    content = (message or {}).get("content")
    if isinstance(content, str):
        return [{"type": "text", "text": content}]
    return content if isinstance(content, list) else []


def text_of(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "\n".join(text_of(item.get("content") if item.get("type") == "tool_result" else item.get("text", ""))
                         for item in content if isinstance(item, dict))
    return ""


def load_events(lines, subagent=False):
    """The session's user and assistant events, in time order, each reduced to
    what the classification needs. A subagent's user text is its parent's
    prompt, never a human's."""
    events = []
    for line in lines:
        try:
            raw = json.loads(line)
        except ValueError:
            continue
        if raw.get("type") not in ("user", "assistant") or "timestamp" not in raw:
            continue
        items = blocks(raw.get("message"))
        event = {"ts": parse_ts(raw["timestamp"]), "role": raw["type"], "uses": [], "results": {}, "text": ""}
        for item in items:
            kind = item.get("type")
            if kind == "tool_use":
                event["uses"].append({"id": item.get("id"), "name": item.get("name"), "input": item.get("input") or {}})
            elif kind == "tool_result":
                event["results"][item.get("tool_use_id")] = text_of(item.get("content"))
            elif kind == "text" and not (subagent and raw["type"] == "user"):
                event["text"] += item.get("text", "")
        events.append(event)
    events.sort(key=lambda e: e["ts"])
    return events


def codex_uses(call_id, script):
    """The uses of one Codex `exec` script: each `tools.exec_command` as a Bash
    use, each `tools.apply_patch` as an Edit, all under the script's call id,
    whose single output answers them. A script that only polls running
    commands (`write_stdin`) is a wait."""
    uses = []
    for call in adherence.CODEX_TOOL.finditer(script):
        if call.group(1) == "apply_patch":
            uses.append({"id": call_id, "name": "Edit", "input": {}})
            continue
        cmd = adherence.CODEX_CMD.search(adherence.call_arguments(script, call.end()))
        if not cmd:
            continue
        command = json.loads(f'"{cmd.group(1)}"')
        uses.append({"id": call_id, "name": "Bash", "input": {"command": command}})
        if EDIT_COMMAND.search(command):
            uses.append({"id": call_id, "name": "Edit", "input": {}})
    if "write_stdin" in script:
        uses.append({"id": call_id, "name": "wait", "input": {"polls": CODEX_POLL.findall(script)}})
    return uses


def load_codex(lines):
    """(session id, cwd, events) of one Codex rollout, its events shaped as
    `load_events` shapes Claude's."""
    session, cwd, events, waits = None, None, [], set()
    for line in lines:
        try:
            raw = json.loads(line)
        except ValueError:
            continue
        payload = raw.get("payload")
        if not isinstance(payload, dict) or "timestamp" not in raw:
            continue
        if raw.get("type") == "session_meta":
            session, cwd = payload.get("id"), payload.get("cwd")
            continue
        if raw.get("type") != "response_item":
            continue
        event = {"ts": parse_ts(raw["timestamp"]), "role": "user", "uses": [], "results": {}, "text": ""}
        kind, call_id = payload.get("type"), payload.get("call_id")
        if kind == "message" and payload.get("role") in ("user", "assistant"):
            text = "\n".join(part.get("text", "") for part in payload.get("content") or [] if isinstance(part, dict))
            if payload["role"] == "assistant":
                event.update(role="assistant", text=text)
            elif not CODEX_NOT_HUMAN.match(text):
                event["text"] = text
        elif kind == "custom_tool_call":
            event["role"] = "assistant"
            if payload.get("name") == "apply_patch":
                event["uses"] = [{"id": call_id, "name": "Edit", "input": {}}]
            else:
                event["uses"] = codex_uses(call_id, str(payload.get("input", "")))
        elif kind == "function_call":
            event["role"] = "assistant"
            name = payload.get("name", "")
            if name in CODEX_WAITS:
                event["uses"] = [{"id": call_id, "name": "wait", "input": {}}]
            elif name == "apply_patch":
                event["uses"] = [{"id": call_id, "name": "Edit", "input": {}}]
            else:
                try:
                    args = json.loads(payload.get("arguments") or "{}")
                except ValueError:
                    args = {}
                cmd = args.get("cmd") or args.get("command") or ""
                cmd = cmd[-1] if isinstance(cmd, list) and cmd else cmd
                event["uses"] = [{"id": call_id, "name": "Bash", "input": {"command": str(cmd)}}]
        elif kind in ("custom_tool_call_output", "function_call_output"):
            output = payload.get("output")
            if isinstance(output, list):
                output = "\n".join(part.get("text", "") for part in output if isinstance(part, dict))
            event["results"][call_id] = str(output or "")
            event["idle"] = call_id in waits
        else:
            continue
        if event["uses"] and all(use["name"] == "wait" for use in event["uses"]):
            waits.add(call_id)
        events.append(event)
    events.sort(key=lambda e: e["ts"])
    return session, cwd, events


def next_kind(event):
    """What a gap ending at `event` was spent on."""
    if event["role"] == "assistant":
        return "model"
    if event.get("idle"):
        return "idle"
    if event["results"]:
        return "tool"
    if event["text"].strip() and not NOT_HUMAN.match(event["text"]):
        return "human"
    # A task notification, a system reminder, an empty user event.
    return "idle"


INTERPRETERS = {"sh", "bash", "zsh", "python", "python3", "timeout", "time", "nice", "env"}


def bash_key(command):
    """`cargo nextest run --workspace` → `cargo nextest run`; leading `cd … &&`,
    subshell parentheses and environment assignments dropped, pipes cut. An
    interpreter or wrapper keeps what it runs: `sh scripts/ci/gates.sh --force`
    → `sh gates.sh`, `bash -c 'cargo test -p x'` → `cargo test`."""
    words = []
    for segment in re.split(r"\s*(?:\|\||&&|;|\||\n)\s*", command):
        words = [w for w in segment.lstrip("( ").split() if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*=\S*", w)]
        if words and words[0] != "cd":
            break
        words = []
    if words and pathlib.PurePath(words[0]).name in INTERPRETERS:
        rest = words[1:]
        if "-c" in rest:
            return bash_key(" ".join(rest[rest.index("-c") + 1:]).strip("'\""))
        target = next((w for w in rest if not w.startswith("-") and not w.isdigit()), None)
        if pathlib.PurePath(words[0]).name in {"timeout", "time", "nice", "env"} and target:
            return bash_key(" ".join(rest[rest.index(target):]))
        return f"{pathlib.PurePath(words[0]).name} {pathlib.PurePath(target).name}" if target else words[0]
    keep = []
    for word in words:
        if word.startswith("-") or ("/" in word and keep):
            break
        keep.append(word)
        if len(keep) == 3:
            break
    return " ".join(keep) or "?"


def units(events, repo=None):
    """One unit per `gh pr create` whose result names a PR of `repo`
    (`owner/name`, any case; every repository when None)."""
    found, start = [], None
    for index, event in enumerate(events):
        for use in event["uses"]:
            if use["name"] in EDIT_TOOLS and start is None:
                start = event["ts"]
        for use in event["uses"]:
            if use["name"] != "Bash" or not PR_CREATE.search(use["input"].get("command", "")):
                continue
            match = next((m for m in PR_URL.finditer(output_of(events, index, use["id"]))
                          if repo is None or m.group(1).lower() == repo.lower()), None)
            if match:
                found.append({"pr": int(match.group(2)), "start": start or event["ts"], "opened": event["ts"]})
                start = None
    return found


def output_of(events, index, use_id):
    """What the call `use_id` made at `events[index]` printed: its result,
    and for a Codex command still running then, the output of the polls of
    its session that follow."""
    later = events[index + 1:]
    position, result = next(((i, e["results"][use_id]) for i, e in enumerate(later) if use_id in e["results"]), (0, ""))
    running = CODEX_SESSION.search(result)
    if not running:
        return result
    parts = [result]
    for i, event in enumerate(later[position + 1:], start=position + 1):
        for use in event["uses"]:
            if running.group(1) in use["input"].get("polls", ()):
                parts.append(next((e["results"][use["id"]] for e in later[i + 1:] if use["id"] in e["results"]), ""))
    return "\n".join(parts)


def breakdown(events, start, end):
    """Seconds per bucket, and the blocking seconds per Bash command, for the
    gaps of one session between `start` and `end`."""
    buckets = dict.fromkeys(BUCKETS, 0.0)
    blocking = collections.Counter()
    background = None  # the last command started with run_in_background
    pushes = 0
    for previous, event in zip(events, events[1:]):
        if event["ts"] <= start or previous["ts"] >= end:
            continue
        seconds = (min(event["ts"], end) - max(previous["ts"], start)).total_seconds()
        kind = next_kind(event)
        buckets[kind] += seconds
        names = []
        for use in previous["uses"]:
            if use["name"] == "Bash":
                command = use["input"].get("command", "")
                pushes += bool(GIT_PUSH.search(command))
                if use["input"].get("run_in_background"):
                    background = bash_key(command) + " (background)"
                    continue
                names.append(bash_key(command))
            elif use["name"] == "wait":
                # A Codex wait on a command already running: what follows is idle.
                background = background or "wait (background)"
            else:
                names.append(use["name"])
        if kind == "tool" and names:
            # Parallel calls share the gap: charge it to the first one.
            blocking[names[0]] += seconds
        elif kind == "idle" and background:
            blocking[background] += seconds
    return buckets, blocking, pushes


def validated_at(view):
    """Last completion among the checks of the final head. An open PR with an
    unfinished check is not validated yet (None). Once merged, a check left
    unfinished no longer holds it: a review status merged over stays
    `PENDING` forever (verified 2026-09 on #335, CodeRabbit)."""
    merged = bool(view.get("mergedAt"))
    times = []
    for check in view.get("statusCheckRollup") or []:
        if "state" in check:  # a commit status: no completion time, only its start
            finished = check["state"] not in ("PENDING", "EXPECTED")
            done = check.get("startedAt")
        else:  # a check run
            finished = check.get("status") == "COMPLETED"
            done = check.get("completedAt")
        if not finished or not done or done.startswith("0001"):
            if merged:
                continue
            return None
        times.append(parse_ts(done))
    return max(times) if times else None


def gh_view(pr, repo=None):
    out = subprocess.run(["gh", "pr", "view", str(pr), "--json", "headRefOid,statusCheckRollup,mergedAt",
                          *(["--repo", repo] if repo else [])],
                         capture_output=True, text=True, timeout=30)
    return json.loads(out.stdout) if out.returncode == 0 else None


def recent(paths, since):
    """The transcript files among `paths` written to since `since`: one that
    was not cannot hold a PR opened in the window."""
    cutoff = since.timestamp()
    return sorted(p for p in paths if p.stat().st_mtime >= cutoff)


def repo_slug():
    """`owner/name` of the `origin` remote, or None."""
    out = subprocess.run(["git", "remote", "get-url", "origin"], capture_output=True, text=True)
    match = re.search(r"github\.com[:/]([^/\s]+/[^/\s]+?)(?:\.git)?/?$", out.stdout.strip())
    return match.group(1) if match else None


def fmt(seconds):
    if seconds is None:
        return "—"
    minutes = seconds / 60
    return f"{minutes / 60:.1f} h" if minutes >= 90 else f"{minutes:.0f} min"


def main(argv):
    args = argv[1:]
    if not args or args[0].startswith("-"):
        sys.exit(__doc__)
    since = parse_window(args[0], datetime.datetime.now(UTC))
    repo = repo_slug()
    dirs = [pathlib.Path(a) for flag, a in zip(args, args[1:]) if flag == "--projects"] or sorted(
        p for p in (pathlib.Path.home() / ".claude" / "projects").glob("*") if p.is_dir())
    codex_dirs = [pathlib.Path(a).expanduser() for flag, a in zip(args, args[1:]) if flag == "--codex"] or [
        pathlib.Path.home() / ".codex" / "sessions", pathlib.Path.home() / ".codex" / "archived_sessions"]
    use_gh, as_json = "--no-gh" not in args, "--json" in args
    # A subagent opens PRs too (`<session>/subagents/agent-*.jsonl`); its unit
    # is split with its own events, the parent's wait staying the parent's.
    claude = recent([p for d in dirs for pattern in ("*.jsonl", "*/subagents/*.jsonl") for p in d.glob(pattern)], since)
    sessions = [(path.stem.removeprefix("agent-")[:8],
                 load_events(path.read_text(errors="replace").splitlines(), subagent=path.parent.name == "subagents"))
                for path in claude]
    seen = set()
    for path in recent([p for d in codex_dirs if d.is_dir() for p in d.glob("**/*.jsonl")], since):
        session, _cwd, events = load_codex(path.read_text(errors="replace").splitlines())
        # A finished rollout is moved to archived_sessions; never count one twice.
        if session not in seen:
            seen.add(session)
            sessions.append(((session or path.stem)[:8], events))
    rows, totals, blocking = [], dict.fromkeys(BUCKETS, 0.0), collections.Counter()
    for name, events in sessions:
        for unit in units(events, repo):
            if unit["opened"] < since:
                continue
            view = gh_view(unit["pr"], repo) if use_gh else None
            done = validated_at(view) if view else None
            end = done or unit["opened"]
            buckets, blocked, pushes = breakdown(events, unit["start"], end)
            for key in BUCKETS:
                totals[key] += buckets[key]
            blocking.update(blocked)
            rows.append({"pr": unit["pr"], "session": name, "start": unit["start"].isoformat(),
                         "edit_to_open_s": (unit["opened"] - unit["start"]).total_seconds(),
                         "open_to_green_s": (done - unit["opened"]).total_seconds() if done else None,
                         "pushes_in_session": pushes, **{f"{k}_s": v for k, v in buckets.items()}})
    if as_json:
        print(json.dumps({"since": since.isoformat(), "units": rows, "totals_s": totals,
                          "blocking_s": dict(blocking.most_common(15))}, indent=2))
        return
    print(f"{len(rows)} PRs of {repo or 'any repository'} opened since {since:%Y-%m-%d %H:%M}Z, "
          f"read from {len(claude)} Claude and {len(seen)} Codex sessions")
    print(f"{'PR':>5} {'session':8} {'edit→open':>9} {'open→green':>10} {'push':>4} {'tool':>7} {'model':>7} {'idle':>7} {'human':>7}")
    for r in sorted(rows, key=lambda r: r["pr"]):
        print(f"{r['pr']:>5} {r['session']:8} {fmt(r['edit_to_open_s']):>9} {fmt(r['open_to_green_s']):>10} "
              f"{r['pushes_in_session']:>4} " + " ".join(f"{fmt(r[k + '_s']):>7}" for k in BUCKETS))
    leads = [r["edit_to_open_s"] + r["open_to_green_s"] for r in rows if r["open_to_green_s"] is not None]
    if leads:
        print(f"lead time, first edit → green, {len(leads)} PRs: median {fmt(statistics.median(leads))}, max {fmt(max(leads))}")
    print("in-session time: " + ", ".join(f"{k} {fmt(v)}" for k, v in totals.items()))
    print("most blocking commands (foreground run + idle after them):")
    for key, seconds in blocking.most_common(10):
        print(f"  {fmt(seconds):>7}  {key}")


if __name__ == "__main__":
    main(sys.argv)
