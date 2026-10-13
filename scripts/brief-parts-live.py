#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Live proof that a brief split over several hooks reaches a real Claude Code (issue #906).

Claude Code caps what one ``UserPromptSubmit`` hook injects at 10,000
characters. ``pixel run-hook task-event --event prompt-submit --part K/N`` serves
one slice of the brief per hook; this driver shows the three slices arrive in
one prompt, and that the same oversized brief from one hook does not.

A debug build of ``pixel`` forces the oversized brief: with
``PIXEL_BRIEF_TEST_PAD_PARTS=3`` the brief is exactly three parts and each ends
in a line ``PIXEL-PART-MARKER-K-OF-3``. The model is asked, with no tools, to
echo the markers it can see in its context.

    brief-parts-live.py --binary target/debug/pixel [--claude ~/.local/bin/claude] [--arms split,single,split]

Each arm is one headless session (``--setting-sources project,local --settings
<file>``: nothing of the user's own settings is read, only one repository's
hooks), in a scratch git repository, with a scrubbed environment. The API key
variable is never passed on: the session uses the CLI's own login. The report is
one JSON object on stdout. Standard library only.
"""

import argparse
import hashlib
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

MARKER = re.compile(r"PIXEL-PART-MARKER-(\d+)-OF-(\d+)")
PARTS = 3
PROMPT = (
    "Do not use any tools. Your context for this message may contain lines of the exact form "
    "PIXEL-PART-MARKER-<k>-OF-3. Reply with every such line you can see, copied verbatim, "
    "one per line, and nothing else. If there is none, reply NONE."
)
ENV_KEEP = ("HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE", "TERM", "TZ",
            "__CF_USER_TEXT_ENCODING", "NODE_USE_SYSTEM_CA")
HOOK = "run-hook task-event --provider claude --event prompt-submit"


def sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


FAIL_OPEN = " || true"  # what `pixel install` ends every prompt-submit command with


def settings(binary, arm, old_binary):
    """One UserPromptSubmit entry per part (``split``) or one for the whole brief (``single``).

    The commands are the ones ``pixel install`` writes, tail included. ``split`` also
    registers a stand-in for a ``pixel`` older than ``--part``, which exits 2 on the
    flag: were its failure not swallowed Claude Code would block the prompt.
    """
    quoted = shlex.quote(str(binary))
    commands = ([f"{quoted} {HOOK} --part {k}/{PARTS}{FAIL_OPEN}" for k in range(1, PARTS + 1)]
                + [f"{shlex.quote(str(old_binary))} {HOOK} --part 1/{PARTS}{FAIL_OPEN}"]
                if arm == "split" else [f"{quoted} {HOOK}{FAIL_OPEN}"])
    return {"hooks": {"UserPromptSubmit": [
        {"hooks": [{"type": "command", "command": command, "timeout": 10}]} for command in commands]}}


def agent_env():
    env = {key: os.environ[key] for key in ENV_KEEP if key in os.environ}
    env["PATH"] = os.environ.get("PATH", "/usr/bin:/bin")
    env["DISABLE_AUTOUPDATER"] = "1"
    env["PIXEL_DAEMON_AUTO_START"] = "0"
    env["PIXEL_BRIEF_TEST_PAD_PARTS"] = str(PARTS)
    return env


def claude_argv(claude, settings_path):
    return [str(claude), "-p", "--output-format", "stream-json", "--verbose", "--include-hook-events",
            "--setting-sources", "project,local", "--settings", str(settings_path),
            "--strict-mcp-config", "--disable-slash-commands", "--no-session-persistence",
            "--permission-mode", "dontAsk", "--tools", "", "--max-budget-usd", "1"]


def read_stream(lines):
    """The answer, the hook outputs and the usage out of a ``stream-json`` transcript."""
    answer, hooks, usage, tools = "", [], None, 0
    for line in lines:
        try:
            event = json.loads(line)
        except ValueError:
            continue
        kind = event.get("type")
        if kind == "result":
            answer = event.get("result") or ""
            usage = event.get("usage")
        elif kind == "assistant":
            tools += sum(1 for block in event.get("message", {}).get("content", [])
                         if isinstance(block, dict) and block.get("type") == "tool_use")
        elif kind == "system" and event.get("subtype") == "hook_response":
            hooks.append({"command": event.get("command") or event.get("hook_name"),
                          "exit_code": event.get("exit_code"), "stdout_chars": len(event.get("stdout") or ""),
                          "part": part_of(event.get("stdout") or ""),
                          "additional_context_chars": context_chars(event.get("stdout") or "")})
    return answer, hooks, usage, tools


def part_of(stdout):
    """Which part a hook printed: the marker its slice ends with."""
    found = MARKER.findall(stdout)
    return int(found[-1][0]) if found else None


def context_chars(stdout):
    try:
        return len(json.loads(stdout)["hookSpecificOutput"]["additionalContext"])
    except (ValueError, KeyError, TypeError):
        return 0


def run_arm(claude, binary, work, arm):
    repo = work / f"repo-{arm}"
    shutil.rmtree(repo, ignore_errors=True)
    repo.mkdir(parents=True)
    (repo / "README.md").write_text("scratch repository for the brief-parts proof\n")
    for command in (["git", "init", "-q"], ["git", "add", "."],
                    ["git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "init"]):
        subprocess.run(command, cwd=repo, check=True, capture_output=True)
    path = work / f"settings-{arm}.json"
    old_binary = work / "old-pixel"
    old_binary.write_text("#!/bin/sh\necho \"error: unexpected argument '--part' found\" >&2\nexit 2\n")
    old_binary.chmod(0o755)
    path.write_text(json.dumps(settings(binary, arm, old_binary)))
    done = subprocess.run(claude_argv(claude, path), cwd=repo, env=agent_env(), input=PROMPT, text=True,
                          capture_output=True, timeout=300, check=False)
    answer, hooks, usage, tools = read_stream(done.stdout.splitlines())
    seen = [int(k) for k, n in MARKER.findall(answer) if int(n) == PARTS]
    arrival = [hook["part"] for hook in hooks if hook["part"]]
    return {"arm": arm, "exit_code": done.returncode, "tool_calls": tools, "markers_echoed": seen,
            "hooks_finished_in_order": arrival, "part_1_first": arrival[:1] == [1],
            "answer": answer.strip()[:400], "hooks": hooks, "usage": usage,
            "stderr": done.stderr.strip()[-300:]}


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--binary", required=True, help="a debug build of pixel")
    parser.add_argument("--claude", default=str(Path.home() / ".local/bin/claude"))
    parser.add_argument("--arms", default="split,single,split", help="comma list of split|single, one session each")
    args = parser.parse_args()
    if "ANTHROPIC_API_KEY" in os.environ:
        print("ANTHROPIC_API_KEY is set in this shell; it is not passed to the session.", file=sys.stderr)
    work = Path(tempfile.mkdtemp(prefix="pixel-brief-parts-"))
    binary = work / "pixel"
    shutil.copy2(args.binary, binary)
    report = {"binary_sha256": sha256(binary), "claude": subprocess.run(
        [args.claude, "--version"], capture_output=True, text=True, check=False).stdout.strip(), "runs": []}
    for arm in args.arms.split(","):
        report["runs"].append(run_arm(args.claude, binary, work, arm))
    json.dump(report, sys.stdout, indent=2)
    print()


if __name__ == "__main__":
    main()
