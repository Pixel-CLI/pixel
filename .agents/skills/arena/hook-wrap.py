#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""The pixel binary a Codex hook calls when `arena.sh run-set --brief-extra CMD` hydrates the brief.

Everything but the prompt-submit hook is exec'd into the real binary. On the prompt-submit hook, part 1 runs the real
hook and then each `CMD` line of the `hook-wrap.json` next to this file (environment `CLONE`, `PROMPT`,
`BRIEF_FILE`): a command that prints `@@REPLACE@@` followed by text replaces the brief, any other output is
appended. A brief longer than one hook may carry is split over the parts at line boundaries: part 1 prints the
first piece and stores the rest, parts 2..N wait for them and print theirs. A failing command leaves the real
brief as it was."""
import hashlib
import json
import os
import shlex
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
CONFIG = json.load(open(os.path.join(HERE, "hook-wrap.json")))
REAL, EXTRA, CAP = CONFIG["real"], CONFIG.get("extra", []), 9500
MARK = "@@REPLACE@@"
args = sys.argv[1:]
if not (args[:2] == ["hook", "prompt-submit"] and "--host" in args):
    os.execv(REAL, [REAL] + args)
stdin = sys.stdin.read()
try:
    prompt = json.loads(stdin).get("prompt", "")
except ValueError:
    prompt = ""
part = int(args[args.index("--part") + 1].split("/")[0]) if "--part" in args else 1
cache = os.path.join(os.getcwd(), ".pixel", "hook-wrap-" + hashlib.sha256(prompt.encode()).hexdigest()[:16] + ".json")
done = cache + ".done"


def envelope(ctx):
    return json.dumps({"hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": ctx}})


def pieces(text):
    out, cur = [], ""
    for line in text.splitlines(True):
        if len(cur) + len(line) > CAP and cur:
            out.append(cur)
            cur = ""
        cur += line[:CAP]
    return out + ([cur] if cur else [])


if part > 1:
    deadline = time.time() + 8.5
    while time.time() < deadline and not os.path.exists(done):
        time.sleep(0.05)
    try:
        state = json.load(open(done))
    except (OSError, ValueError):
        state = {"mode": "pass"}
    if state["mode"] == "pieces":
        p = state["pieces"]
        sys.stdout.write(envelope(p[part - 1]) if part - 1 < len(p) else "{}")
        sys.exit(0)
    sys.stdout.write(subprocess.run([REAL] + args, input=stdin, capture_output=True, text=True).stdout)
    sys.exit(0)
try:
    os.remove(done)
except OSError:
    pass
out = subprocess.run([REAL] + args, input=stdin, capture_output=True, text=True).stdout
state = {"mode": "pass"}


def context_of(text):
    try:
        value = json.loads(text)["hookSpecificOutput"].get("additionalContext", "") if text.strip() else ""
    except (ValueError, KeyError, TypeError):
        return ""
    return value if isinstance(value, str) else ""


try:
    body = json.loads(out) if out.strip() else {}
    if not isinstance(body, dict) or not isinstance(body.get("hookSpecificOutput"), dict):
        body = {"hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": ""}}
    first = body["hookSpecificOutput"].get("additionalContext", "")
    if not isinstance(first, str):
        first = ""
    # The probes read the WHOLE brief, not part 1's slice: run the real parts 2..N (they find part 1's stored brief)
    # and join the slices, then split the result over the parts again.
    count = int(args[args.index("--part") + 1].split("/")[1]) if "--part" in args else 1
    full = first
    for k in range(2, count + 1):
        later = [a if a != args[args.index("--part") + 1] else f"{k}/{count}" for a in args]
        full += context_of(subprocess.run([REAL] + later, input=stdin, capture_output=True, text=True).stdout)
    ctx = full
    for command in EXTRA:
        with tempfile.NamedTemporaryFile("w", delete=False) as handle:
            handle.write(ctx)
        env = dict(os.environ, CLONE=os.getcwd(), PROMPT=prompt, BRIEF_FILE=handle.name, PIXEL_REAL=REAL)
        try:
            # Each command gets a few seconds: part 1 as a whole must stay inside what the later parts wait for.
            extra = subprocess.run(shlex.split(command), capture_output=True, text=True, timeout=5, env=env).stdout
        finally:
            os.unlink(handle.name)
        if extra.startswith(MARK):
            ctx = extra[len(MARK):].lstrip("\n")
        elif extra.strip():
            ctx = (ctx.rstrip("\n") + "\n" + extra) if ctx else "[PIXEL:BRIEF]\n" + extra
    if ctx != full:
        p = pieces(ctx)
        state = {"mode": "pieces", "pieces": p}
        body["hookSpecificOutput"]["additionalContext"] = p[0] if p else ""
        out = json.dumps(body)
except (OSError, ValueError, KeyError, subprocess.SubprocessError):
    state = {"mode": "pass"}
os.makedirs(os.path.dirname(cache), exist_ok=True)
json.dump(state, open(done, "w"))
try:
    delivered = sum(len(x) for x in state["pieces"]) if state["mode"] == "pieces" else len(ctx)
    with open(os.path.join(os.path.dirname(cache), "hook-wrap.jsonl"), "a") as log:
        log.write(json.dumps({"t": "wrapped", "bytes": delivered, "mode": state["mode"]}) + "\n")
except (NameError, OSError, KeyError):
    pass
sys.stdout.write(out)
