# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Lead time of the pixel repository's own pull requests, from Claude Code transcripts.

Usage: python3 lead_time.py <window> [--projects DIR]... [--no-gh] [--json]
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

Transcripts are read from `~/.claude/projects/<repo path with every
non-alphanumeric character turned into ->*`, which also matches the
worktrees under `<repo>/.claude/worktrees/`. Subagent transcripts are not
read: their time is already the parent's wait. Only pixel commands, PR
numbers and durations are printed; no prompt or file content.
"""
import collections, datetime, json, pathlib, re, statistics, subprocess, sys

UTC = datetime.timezone.utc
EDIT_TOOLS = {"Edit", "Write", "NotebookEdit", "MultiEdit"}
PR_URL = re.compile(r"github\.com/[^/\s]+/[^/\s]+/pull/(\d+)")
PR_CREATE = re.compile(r"\bgh\s+pr\s+create\b")
GIT_PUSH = re.compile(r"\bgit\s+push\b")
# Injected user-role text that no human typed.
NOT_HUMAN = re.compile(r"^\s*<(task-notification|system-reminder|local-command|command-name|command-message|user-prompt-submit-hook)")
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


def load_events(lines):
    """The session's user and assistant events, in time order, each reduced to
    what the classification needs."""
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
            elif kind == "text":
                event["text"] += item.get("text", "")
        events.append(event)
    events.sort(key=lambda e: e["ts"])
    return events


def next_kind(event):
    """What a gap ending at `event` was spent on."""
    if event["role"] == "assistant":
        return "model"
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
    interpreter or wrapper keeps what it runs: `sh scripts/gates.sh --force`
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


def units(events):
    """One unit per `gh pr create` whose result names a PR."""
    found, start = [], None
    for index, event in enumerate(events):
        for use in event["uses"]:
            if use["name"] in EDIT_TOOLS and start is None:
                start = event["ts"]
        for use in event["uses"]:
            if use["name"] != "Bash" or not PR_CREATE.search(use["input"].get("command", "")):
                continue
            result = next((later["results"][use["id"]] for later in events[index + 1:] if use["id"] in later["results"]), "")
            match = PR_URL.search(result)
            if match:
                found.append({"pr": int(match.group(1)), "start": start or event["ts"], "opened": event["ts"]})
                start = None
    return found


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


def gh_view(pr):
    out = subprocess.run(["gh", "pr", "view", str(pr), "--json", "headRefOid,statusCheckRollup,mergedAt"],
                         capture_output=True, text=True, timeout=30)
    return json.loads(out.stdout) if out.returncode == 0 else None


def project_dirs(root):
    name = re.sub(r"[^A-Za-z0-9]", "-", str(root))
    return sorted(pathlib.Path.home().joinpath(".claude/projects").glob(name + "*"))


def repo_root():
    common = subprocess.run(["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
                            capture_output=True, text=True, check=True).stdout.strip()
    return pathlib.Path(common).parent


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
    dirs = [pathlib.Path(a) for flag, a in zip(args, args[1:]) if flag == "--projects"] or project_dirs(repo_root())
    use_gh, as_json = "--no-gh" not in args, "--json" in args
    rows, totals, blocking = [], dict.fromkeys(BUCKETS, 0.0), collections.Counter()
    for path in sorted(p for d in dirs for p in d.glob("*.jsonl")):
        events = load_events(path.read_text(errors="replace").splitlines())
        for unit in units(events):
            if unit["opened"] < since:
                continue
            view = gh_view(unit["pr"]) if use_gh else None
            done = validated_at(view) if view else None
            end = done or unit["opened"]
            buckets, blocked, pushes = breakdown(events, unit["start"], end)
            for key in BUCKETS:
                totals[key] += buckets[key]
            blocking.update(blocked)
            rows.append({"pr": unit["pr"], "session": path.stem[:8], "start": unit["start"].isoformat(),
                         "edit_to_open_s": (unit["opened"] - unit["start"]).total_seconds(),
                         "open_to_green_s": (done - unit["opened"]).total_seconds() if done else None,
                         "pushes_in_session": pushes, **{f"{k}_s": v for k, v in buckets.items()}})
    if as_json:
        print(json.dumps({"since": since.isoformat(), "units": rows, "totals_s": totals,
                          "blocking_s": dict(blocking.most_common(15))}, indent=2))
        return
    print(f"{len(rows)} PRs opened since {since:%Y-%m-%d %H:%M}Z, from {len(dirs)} project dirs")
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
